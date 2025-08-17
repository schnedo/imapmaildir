use std::{
    fs::create_dir_all,
    path::{Path, PathBuf},
};

use log::debug;
use rusqlite::{Connection, OpenFlags, Result};
use serde::{Deserialize, Serialize};

use crate::{imap::UidValidity, sync::MailMetadata};

pub struct State {
    db: Connection,
    uid_validity: UidValidity,
}

impl State {
    pub fn create_new(state_dir: &Path, mailbox: &str, uid_validity: UidValidity) -> Self {
        let state_file = Self::prepare_state_file(state_dir, mailbox);
        let db = Connection::open_with_flags(
            state_file,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .expect("State DB should be creatable");

        db.execute_batch(
            "create table if not exists mailboxes (
                name text primary key,
                validity integer not null
            ) without rowid;
            create table if not exists mail_metadata (
                uid integer primary key,
                flags integer not null
            );",
        )
        .expect("creation of tables should succeed");
        let state = Self { db, uid_validity };
        state.create_mailbox(mailbox, uid_validity);
        state
    }

    fn create_mailbox(&self, mailbox: &str, uid_validity: UidValidity) {
        let mut stmt = self
            .db
            .prepare("insert into mailboxes (name,validity) values (?1,?2)")
            .expect("mailbox insert statement should be preparable");
        stmt.execute([mailbox, &uid_validity.to_string()])
            .expect("creation of new mailbox should succeed");
    }

    fn prepare_state_file(state_dir: &Path, mailbox: &str) -> PathBuf {
        debug!("creating state file {mailbox} in {state_dir:?}");
        create_dir_all(state_dir).expect("creation of state_dir should succeed");
        state_dir.join(mailbox)
    }

    pub fn load(state_dir: &Path, mailbox: &str) -> Result<Self> {
        let state_file = Self::prepare_state_file(state_dir, mailbox);
        let db = Connection::open_with_flags(
            state_file,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        let mut stmt = db
            .prepare("select (validity) from mailboxes where name = ?1")
            .expect("uid_validity statement should be preparable");
        let uid_validity = stmt
            .query_one([mailbox], |row| Ok(UidValidity::new(row.get(0)?)))
            .expect("uid_validity should be selectable");
        drop(stmt);

        Ok(Self { db, uid_validity })
    }

    pub fn uid_validity(&self) -> &UidValidity {
        &self.uid_validity
    }

    pub fn store(&self, metadata: &MailMetadata) {
        let mut stmt = self
            .db
            .prepare_cached("insert into mail_metadata (uid,flags) values (?1,?2)")
            .expect("insert mail metadata statement should be preparable");
        stmt.execute([
            metadata.uid().to_string(),
            metadata.flags().bits().to_string(),
        ])
        .expect("mail metadata should be insertable");
    }
}

impl Serialize for UidValidity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let num: u32 = (*self).into();
        num.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for UidValidity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let num = u32::deserialize(deserializer)?;
        Ok(num.into())
    }
}
