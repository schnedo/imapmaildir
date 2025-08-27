use std::{
    fs::create_dir_all,
    path::{Path, PathBuf},
};

use derive_getters::Getters;
use enumflags2::{BitFlag, BitFlags};
use log::debug;
use rusqlite::{types::FromSql, Connection, OpenFlags, OptionalExtension, Result, ToSql};

use crate::{
    imap::{Uid, UidValidity},
    sync::{Flag, MailMetadata},
};

#[derive(Getters)]
pub struct StateEntry {
    metadata: MailMetadata,
    fileprefix: String,
}

impl StateEntry {
    pub fn new(metadata: MailMetadata, fileprefix: String) -> Self {
        Self {
            metadata,
            fileprefix,
        }
    }

    pub fn set_flags(&mut self, flags: BitFlags<Flag>) {
        self.metadata.set_flags(flags);
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
        stmt.execute((data.metadata.flags().bits(), u32::from(data.metadata.uid())))
            .expect("mail metadata should be updateable");
    }

    pub fn store(&self, data: &StateEntry) {
        let mut stmt = self
            .db
            .prepare_cached("insert into mail_metadata (uid,flags,fileprefix) values (?1,?2,?3)")
            .expect("insert mail metadata statement should be preparable");
        stmt.execute((
            u32::from(data.metadata.uid()),
            data.metadata.flags().bits(),
            &data.fileprefix,
        ))
        .expect("mail metadata should be insertable");
    }

    pub fn exists(&self, uid: Uid) -> Option<StateEntry> {
        let mut stmt = self
            .db
            .prepare_cached("select * from mail_metadata where uid = ?1")
            .expect("selection of existing mails should be preparable");
        stmt.query_one([u32::from(uid)], |row| {
            Ok(StateEntry {
                metadata: MailMetadata::new(
                    row.get(0)
                        .expect("first index of state entry row should be readable"),
                    Flag::from_bits_truncate(
                        row.get(1)
                            .expect("second index of state entry row should be readable"),
                    ),
                ),
                fileprefix: row
                    .get(2)
                    .expect("third index of state entry row should be readable"),
            })
        })
        .optional()
        .expect("existence of uid should be queryable")
    }
}

impl FromSql for Uid {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        i64::column_result(value).map(|as_i64| {
            Uid::from(u32::try_from(as_i64).expect("parsing uid field in sqlite should succeed"))
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
