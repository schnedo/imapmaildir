use std::{
    cell::Cell,
    convert::Into,
    fmt::Display,
    fs::create_dir_all,
    path::{Path, PathBuf},
};

use derive_getters::Getters;
use enumflags2::{BitFlag, BitFlags};
use log::debug;
use rusqlite::{
    Connection, Error, MappedRows, OpenFlags, OptionalExtension, Result, Row, ToSql, types::FromSql,
};

use crate::{
    imap::{ModSeq, Uid, UidValidity},
    maildir::LocalMailMetadata,
    sync::{Flag, MailMetadata},
};

pub struct State {
    db: Connection,
}

impl State {
    pub fn load(state_dir: &Path, account: &str, mailbox: &str) -> Result<Self, Error> {
        let state_file = Self::prepare_state_file(state_dir, account, mailbox);
        debug!(
            "try loading existing state file {}",
            state_file.to_string_lossy()
        );
        let db = Connection::open_with_flags(
            state_file,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;

        Ok(Self { db })
    }

    pub fn init(state_dir: &Path, account: &str, mailbox: &str) -> Self {
        let state_file = Self::prepare_state_file(state_dir, account, mailbox);
        debug!("creating new state file {}", state_file.to_string_lossy());
        let db = Connection::open(state_file).expect("State DB should be creatable");
        db.execute_batch(
            "pragma journal_mode=wal;
            pragma user_version=0;
            pragma synchronous=1;
            create table mail_metadata (
                uid integer primary key,
                flags integer not null,
                fileprefix text not null
            ) strict;
            create table uid_validity (
                uid_validity integer primary key
            ) strict;
            pragma optimize;",
        )
        .expect("creation of tables should succeed");

        Self { db }
    }

    fn prepare_state_file(state_dir: &Path, account: &str, mailbox: &str) -> PathBuf {
        let mut state_dir = state_dir.join(account);
        create_dir_all(&state_dir).expect("creation of state_dir should succeed");
        state_dir.push(mailbox);
        state_dir
    }

    pub fn uid_validity(&self) -> UidValidity {
        self.db
            .query_one("select * from uid_validity", (), |row| {
                let validity: u32 = row.get(0)?;
                Ok(UidValidity::from(validity))
            })
            .expect("uid_validity should be selectable")
    }

    pub fn set_uid_validity(&self, uid_validity: UidValidity) {
        self.db
            .execute(
                "insert into uid_validity (uid_validity) values (?1)",
                [u32::from(uid_validity)],
            )
            .expect("uid should be settable");
    }

    pub fn set_modseq(&self, value: ModSeq) {
        self.db
            .pragma_update(None, "user_version", u64::from(value))
            .expect("setting modseq should succeed");
    }

    pub fn modseq(&self) -> ModSeq {
        self.db
            .query_one("select * from pragma_user_version", [], |row| {
                let modseq: u64 = row.get(0)?;
                Ok(modseq
                    .try_into()
                    .expect("cached highest modseq should be valid"))
            })
            .expect("getting modseq should succeed")
    }

    pub fn update(&self, data: &LocalMailMetadata) {
        let mut stmt = self
            .db
            .prepare_cached("update mail_metadata set flags=?1 where uid=?2")
            .expect("update metadata statement should be preparable");
        stmt.execute((data.flags().bits(), data.uid().map_or(0, Into::into)))
            .expect("mail metadata should be updateable");
    }

    pub fn store(&self, data: &LocalMailMetadata) -> Option<Uid> {
        if let Some(uid) = data.uid() {
            let mut stmt = self
                .db
                .prepare_cached(
                    "insert into mail_metadata (uid,flags,fileprefix) values (?1,?2,?3)",
                )
                .expect("insert mail metadata statement should be preparable");
            stmt.execute((u32::from(uid), data.flags().bits(), &data.fileprefix()))
                .expect("mail metadata should be insertable");
            None
        } else {
            let mut stmt = self
                .db
                .prepare_cached("insert into mail_metadata (flags,fileprefix) values (?1,?2)")
                .expect("insert mail metadata statement should be preparable");
            stmt.execute((data.flags().bits(), &data.fileprefix()))
                .expect("mail metadata should be insertable");
            Some(
                self.db
                    .last_insert_rowid()
                    .try_into()
                    .expect("newly stored mail id should be parsable to Uid"),
            )
        }
    }

    pub fn exists(&self, uid: Option<Uid>) -> Option<LocalMailMetadata> {
        let mut stmt = self
            .db
            .prepare_cached("select * from mail_metadata where uid = ?1")
            .expect("selection of existing mails should be preparable");
        stmt.query_one([uid.map_or(0, Into::into)], |row| {
            Ok(row.try_into().expect("stateentry should be parsable"))
        })
        .optional()
        .expect("existence of uid should be queryable")
    }

    pub fn for_each(&self, mut cb: impl FnMut(&LocalMailMetadata)) {
        let mut stmt = self
            .db
            .prepare("select (uid,flags,fileprefix) from mail_metadata;")
            .expect("select all mail_metadata should be preparable");
        let rows = stmt
            .query_map([], |row| row.try_into())
            .expect("all metadata should be selectable");
        for row in rows {
            let entry = row.expect("stateentry should be parsable");
            cb(&entry);
        }
    }
}

impl TryFrom<&Row<'_>> for LocalMailMetadata {
    type Error = Error;

    fn try_from(value: &Row) -> Result<Self, Self::Error> {
        let uid: u32 = value.get(0)?;
        let uid = Uid::try_from(uid).ok();
        let flags = Flag::from_bits_truncate(value.get(1)?);
        Ok(Self::new(uid, flags, value.get(2)?))
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.db
            .execute("pragma optimize;", [])
            .expect("sqlite should be optimizable");
    }
}
