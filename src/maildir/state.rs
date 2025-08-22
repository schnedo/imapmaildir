use std::{
    fs::create_dir_all,
    path::{Path, PathBuf},
};

use log::debug;
use rusqlite::{Connection, OpenFlags, Result};

use crate::{imap::UidValidity, sync::MailMetadata};

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
            "create table if not exists mail_metadata (
                uid integer primary key,
                flags integer not null
            ) strict;",
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
        let mut stmt = db
            .prepare("select user_version from pragma_user_version;")
            .expect("uid_validity statement should be preparable");
        let uid_validity = stmt
            .query_one([], |row| {
                Ok(UidValidity::new(
                    row.get(0).expect("uid_validity should be set in state"),
                ))
            })
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
