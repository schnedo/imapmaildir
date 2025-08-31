use std::{
    convert::Into,
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
    imap::{Uid, UidValidity},
    maildir::maildir_repository::LocalMailMetadata,
    sync::{Flag, MailMetadata},
};

pub struct StateEntry {
    uid: Option<Uid>,
    flags: BitFlags<Flag>,
    fileprefix: String,
}

impl StateEntry {
    pub fn new(uid: Option<Uid>, flags: BitFlags<Flag>, fileprefix: String) -> Self {
        Self {
            uid,
            flags,
            fileprefix,
        }
    }

    pub fn set_flags(&mut self, flags: BitFlags<Flag>) {
        self.flags = flags
    }

    pub fn flags(&self) -> BitFlags<Flag> {
        self.flags
    }

    pub fn fileprefix(&self) -> &str {
        self.fileprefix.as_str()
    }

    pub fn uid(&self) -> Option<Uid> {
        self.uid
    }
}

pub struct State {
    db: Connection,
    uid_validity: UidValidity,
}

impl State {
    pub fn create_new(
        state_dir: &Path,
        account: &str,
        mailbox: &str,
        uid_validity: UidValidity,
    ) -> Self {
        let state_file = Self::prepare_state_file(state_dir, account, mailbox);
        debug!("creating new state file {}", state_file.to_string_lossy());
        let db = Connection::open_with_flags(
            state_file,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .expect("State DB should be creatable");
        db.pragma_update(None, "user_version", u32::from(uid_validity))
            .expect("setting sqlite user_version to uid_validity should succeed");
        db.execute_batch(
            "pragma journal_mode=wal;
            pragma synchronous=1;
            create table if not exists mail_metadata (
                uid integer primary key,
                flags integer not null,
                fileprefix text not null
            ) strict;
            pragma optimize;
",
        )
        .expect("creation of tables should succeed");

        Self { db, uid_validity }
    }

    fn prepare_state_file(state_dir: &Path, account: &str, mailbox: &str) -> PathBuf {
        let mut state_dir = state_dir.join(account);
        create_dir_all(&state_dir).expect("creation of state_dir should succeed");
        state_dir.push(mailbox);
        state_dir
    }

    pub fn load(state_dir: &Path, account: &str, mailbox: &str) -> Result<Self> {
        let state_file = Self::prepare_state_file(state_dir, account, mailbox);
        let db = Connection::open_with_flags(
            state_file,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        db.pragma_update(None, "journal_mode", 1)
            .expect("journal_mode should be settable to normal");
        let uid_validity = db
            .query_one("select user_version from pragma_user_version;", [], |row| {
                Ok(UidValidity::new(
                    row.get(0).expect("uid_validity should be set in state"),
                ))
            })
            .expect("uid_validity should be selectable");

        Ok(Self { db, uid_validity })
    }

    pub fn uid_validity(&self) -> UidValidity {
        self.uid_validity
    }

    pub fn update(&self, data: &StateEntry) {
        let mut stmt = self
            .db
            .prepare_cached("update mail_metadata set flags=?1 where uid=?2")
            .expect("update metadata statement should be preparable");
        stmt.execute((data.flags.bits(), data.uid.map_or(0, Into::into)))
            .expect("mail metadata should be updateable");
    }

    pub fn store(&self, data: &StateEntry) -> Option<Uid> {
        if let Some(uid) = data.uid {
            let mut stmt = self
                .db
                .prepare_cached(
                    "insert into mail_metadata (uid,flags,fileprefix) values (?1,?2,?3)",
                )
                .expect("insert mail metadata statement should be preparable");
            stmt.execute((u32::from(uid), data.flags.bits(), &data.fileprefix))
                .expect("mail metadata should be insertable");
            None
        } else {
            let mut stmt = self
                .db
                .prepare_cached("insert into mail_metadata (flags,fileprefix) values (?1,?2)")
                .expect("insert mail metadata statement should be preparable");
            stmt.execute((data.flags.bits(), &data.fileprefix))
                .expect("mail metadata should be insertable");
            Some(Uid::from(
                self.db
                    .last_insert_rowid()
                    .try_into()
                    .expect("newly stored mail id should be parsable to Uid"),
            ))
        }
    }

    pub fn exists(&self, uid: Option<Uid>) -> Option<StateEntry> {
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

    pub fn for_each(&self, cb: impl Fn(&StateEntry)) {
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

impl TryFrom<&Row<'_>> for StateEntry {
    type Error = Error;

    fn try_from(value: &Row) -> Result<Self, Self::Error> {
        let uid: u32 = value.get(0)?;
        let uid = Uid::try_from(uid).ok();
        let flags = Flag::from_bits_truncate(value.get(1)?);
        Ok(Self {
            uid,
            flags,
            fileprefix: value.get(2)?,
        })
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.db
            .execute("pragma optimize;", [])
            .expect("sqlite should be optimizable");
    }
}
