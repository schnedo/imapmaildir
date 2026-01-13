use std::{
    convert::Into,
    fs::create_dir_all,
    path::{Path, PathBuf},
    sync::Arc,
};

use enumflags2::BitFlag;
use log::{debug, trace};
use rusqlite::{Connection, Error, OpenFlags, OptionalExtension, Result, Row};
use tokio::sync::{Mutex, mpsc};

use crate::{
    maildir::LocalMailMetadata,
    repository::{Flag, ModSeq, Uid, UidValidity},
};

fn get_highest_modseq(db: &Connection) -> ModSeq {
    db.query_one("select * from pragma_user_version", [], |row| {
        let modseq: u64 = row.get(0)?;
        Ok(modseq
            .try_into()
            .expect("cached highest modseq should be valid"))
    })
    .expect("getting modseq should succeed")
}

fn set_highest_modseq(db: &Connection, value: ModSeq) {
    trace!("setting highest_modseq {value}");
    db.pragma_update(None, "user_version", u64::from(value))
        .expect("setting modseq should succeed");
}

fn get_state_version(db: &Connection) -> u32 {
    db.query_one("select state_version from maildir_info", [], |row| {
        row.get(0)
    })
    .expect("stored state version should be gettable")
}

const CURRENT_VERSION: u32 = 1;

#[derive(Clone)]
pub struct State {
    db: Arc<Mutex<Connection>>,
    cached_highest_modseq: Arc<Mutex<ModSeq>>,
}

impl State {
    fn new(db: Connection) -> Self {
        let cached_highest_modseq = get_highest_modseq(&db);

        Self {
            db: Arc::new(Mutex::new(db)),
            cached_highest_modseq: Arc::new(Mutex::new(cached_highest_modseq)),
        }
    }

    pub fn load(state_dir: &Path) -> Result<Self, Error> {
        let state_file = Self::prepare_state_file(state_dir);
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

        if get_state_version(&db) != CURRENT_VERSION {
            todo!("handle state version mismatch")
        }

        Ok(Self::new(db))
    }

    pub fn init(
        state_dir: &Path,
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
    ) -> Result<Self, Error> {
        let state_file = Self::prepare_state_file(state_dir);
        debug!("creating new state file {}", state_file.to_string_lossy());
        let db = Connection::open(state_file)?;
        db.execute_batch(
            "pragma journal_mode=wal;
            pragma user_version=1;
            pragma synchronous=1;
            create table mail_metadata (
                uid integer primary key,
                flags integer not null,
                fileprefix text not null
            ) strict;
            create table maildir_info (
                uid_validity integer primary key,
                state_version integer not null
            ) strict;
            pragma optimize;",
        )
        .expect("creation of tables should succeed");
        trace!("setting cached uid_validity {uid_validity}");
        db.execute(
            "insert or ignore into maildir_info (state_version, uid_validity) values (?1, ?2)",
            [CURRENT_VERSION, u32::from(uid_validity)],
        )
        .expect("maildir_info should be settable");
        set_highest_modseq(&db, highest_modseq);

        Ok(Self::new(db))
    }

    fn prepare_state_file(state_dir: &Path) -> PathBuf {
        create_dir_all(state_dir).expect("creation of state dir should succeed");

        state_dir.join("imapmaildir.db")
    }

    pub async fn uid_validity(&self) -> UidValidity {
        trace!("getting cached uid_validity");
        self.db
            .lock()
            .await
            .query_one("select uid_validity from maildir_info", (), |row| {
                let validity: u32 = row.get(0)?;
                let validity = validity
                    .try_into()
                    .expect("cached uid validity should be spec compliant");
                Ok(validity)
            })
            .expect("uid_validity should be selectable")
    }

    pub async fn update_highest_modseq(&self, value: ModSeq) {
        trace!(
            "check for updating highest_modseq {:?} with {value:?}",
            self.cached_highest_modseq
        );
        let mut cached_highest_modseq = self.cached_highest_modseq.lock().await;
        if value > *cached_highest_modseq {
            self.set_highest_modseq_uncached(value).await;
            *cached_highest_modseq = value;
        }
    }

    async fn set_highest_modseq_uncached(&self, value: ModSeq) {
        trace!("setting highest_modseq {value}");
        let db = self.db.lock().await;
        set_highest_modseq(&db, value);
    }

    pub async fn set_highest_modseq(&self, value: ModSeq) {
        trace!(
            "check for setting highest_modseq {:?} to {value:?}",
            self.cached_highest_modseq
        );
        let mut cached_highest_modseq = self.cached_highest_modseq.lock().await;
        if *cached_highest_modseq != value {
            self.set_highest_modseq_uncached(value).await;
            *cached_highest_modseq = value;
        }
    }

    pub async fn highest_modseq(&self) -> ModSeq {
        trace!("getting cached highest_modseq");
        let db = self.db.lock().await;
        get_highest_modseq(&db)
    }

    pub async fn update(&self, data: &LocalMailMetadata) {
        trace!("updating mail cache {data:?}");
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached("update mail_metadata set flags=?1 where uid=?2")
            .expect("preparation of cached update mail statement should succeed");
        stmt.execute((data.flags().bits(), data.uid().map_or(0, Into::into)))
            .expect("updating metadata should succeed");
    }

    pub async fn store(&self, data: &LocalMailMetadata) {
        trace!("storing mail cache {data:?}");
        let uid = data.uid().expect("stored mail should have uid");
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached("insert into mail_metadata (uid,flags,fileprefix) values (?1,?2,?3)")
            .expect("preparation of cached insert mail metadata should succeed");
        stmt.execute((u32::from(uid), data.flags().bits(), &data.fileprefix()))
            .expect("storing mail should succeed");
    }

    pub async fn get_by_id(&self, uid: Uid) -> Option<LocalMailMetadata> {
        trace!("get existing metadata with {uid:?}");
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached("select * from mail_metadata where uid = ?1")
            .expect("selection of existing mails should be preparable");

        stmt.query_one([u32::from(uid)], |row| {
            Ok(row.try_into().expect("stateentry should be parsable"))
        })
        .optional()
        .expect("existing matadata should be queryable")
    }

    pub async fn delete_by_id(&self, uid: Uid) {
        trace!("deleting {uid:?}");
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached("delete from mail_metadata where uid = ?1")
            .expect("deletion of existing mails should be preparable");
        stmt.execute([u32::from(uid)])
            .expect("deletion of existing mail should succeed");
    }

    pub async fn get_all(&self, all_entries_tx: mpsc::Sender<LocalMailMetadata>) -> ModSeq {
        trace!("getting all stored mail metadata");
        let db = self.db.lock().await;
        let mut stmt = db
            .prepare_cached("select uid,flags,fileprefix from mail_metadata;")
            .expect("select all mail_metadata should be preparable");

        let current_highest_modseq = *self.cached_highest_modseq.lock().await;
        for entry in stmt
            .query_map([], |row| LocalMailMetadata::try_from(row))
            .expect("all metadata should be selectable")
            .map(|maybe_row| {
                maybe_row.expect("local mail metadata should be buildable from db row")
            })
        {
            all_entries_tx
                .send(entry)
                .await
                .expect("sending all mail metadata should succeed");
        }

        current_highest_modseq
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let db = self
            .db
            .try_lock()
            .expect("db should not be unlocked when dropping State");
        db.execute("pragma optimize;", [])
            .expect("sqlite should be optimizable");
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
