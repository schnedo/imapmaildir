use std::{
    cell::Cell,
    convert::Into,
    fmt::{Debug, Display},
    fs::create_dir_all,
    path::{Path, PathBuf},
};

use derive_getters::Getters;
use enumflags2::{BitFlag, BitFlags};
use log::{debug, trace};
use rusqlite::{
    Connection, Error, MappedRows, OpenFlags, OptionalExtension, Result, Row, ToSql, types::FromSql,
};
use tokio::sync::{mpsc, oneshot};

use crate::{
    imap::{ModSeq, Uid, UidValidity},
    maildir::LocalMailMetadata,
    sync::{Flag, MailMetadata},
};

pub type DbTask = dyn FnOnce(&mut Connection) + Send;
pub type BoxedDbTask = Box<DbTask>;
pub struct State {
    db_tx: mpsc::Sender<BoxedDbTask>,
}

impl State {
    pub async fn load(state_dir: &Path, account: &str, mailbox: &str) -> Result<Self, Error> {
        let (db_tx, mut db_rx) = mpsc::channel::<BoxedDbTask>(32);
        let (open_tx, open_rx) = oneshot::channel();

        let state_file = Self::prepare_state_file(state_dir, account, mailbox);
        tokio::task::spawn_blocking(move || {
            debug!(
                "try loading existing state file {}",
                state_file.to_string_lossy()
            );
            match Connection::open_with_flags(
                state_file,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_URI,
            ) {
                Ok(mut db) => {
                    // todo: move this into shutdown logic
                    db.execute("pragma optimize;", [])
                        .expect("sqlite should be optimizable");
                    open_tx
                        .send(Ok(()))
                        .expect("db open channel should still be open");
                    while let Some(task) = db_rx.blocking_recv() {
                        task(&mut db);
                    }
                }
                Err(e) => {
                    open_tx
                        .send(Err(e))
                        .expect("db open channel should still be open");
                }
            }
        });

        open_rx
            .await
            .expect("db open channel should still be open")?;

        Ok(Self { db_tx })
    }

    pub async fn init(state_dir: &Path, account: &str, mailbox: &str) -> Result<Self, Error> {
        let (db_tx, mut db_rx) = mpsc::channel::<BoxedDbTask>(32);
        let (open_tx, open_rx) = oneshot::channel();

        let state_file = Self::prepare_state_file(state_dir, account, mailbox);
        tokio::task::spawn_blocking(move || {
            debug!("creating new state file {}", state_file.to_string_lossy());
            match Connection::open(state_file) {
                Ok(mut db) => {
                    open_tx
                        .send(Ok(()))
                        .expect("db open channel should still be open");
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
                    while let Some(task) = db_rx.blocking_recv() {
                        task(&mut db);
                    }
                }
                Err(e) => {
                    open_tx
                        .send(Err(e))
                        .expect("db open channel should still be open");
                }
            }
        });

        open_rx
            .await
            .expect("db open channel should still be open")?;

        Ok(Self { db_tx })
    }

    fn prepare_state_file(state_dir: &Path, account: &str, mailbox: &str) -> PathBuf {
        let mut state_dir = state_dir.join(account);
        create_dir_all(&state_dir).expect("creation of state_dir should succeed");
        state_dir.push(mailbox);
        state_dir
    }

    async fn execute<T, F>(&self, task: F) -> Result<T, Error>
    where
        T: Send + Debug + 'static,
        F: FnOnce(&mut Connection) -> Result<T, Error> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let handle =
            tokio::spawn(async { rx.await.expect("task result channel should still be open") });
        self.db_tx
            .send(Box::new(move |db| {
                tx.send(task(db))
                    .expect("task channel should still be open");
            }))
            .await
            .expect("db task sending channel should still be open");
        handle.await.expect("result receive task should not fail")
    }

    pub async fn uid_validity(&self) -> UidValidity {
        trace!("getting cached uid_validity");
        self.execute(|db| {
            db.query_one("select * from uid_validity", (), |row| {
                let validity: u32 = row.get(0)?;
                Ok(UidValidity::from(validity))
            })
        })
        .await
        .expect("uid_validity should be selectable")
    }

    pub async fn set_uid_validity(&self, uid_validity: UidValidity) {
        trace!("setting cached uid_validity {uid_validity}");
        self.execute(move |db| {
            db.execute(
                "insert into uid_validity (uid_validity) values (?1)",
                [u32::from(uid_validity)],
            )
        })
        .await
        .expect("uid should be settable");
    }

    pub async fn set_modseq(&self, value: ModSeq) {
        trace!("setting cached highest_modseq {value}");
        self.execute(move |db| db.pragma_update(None, "user_version", u64::from(value)))
            .await
            .expect("setting modseq should succeed");
    }

    pub async fn modseq(&self) -> ModSeq {
        trace!("getting cached highest_modseq");
        self.execute(|db| {
            db.query_one("select * from pragma_user_version", [], |row| {
                let modseq: u64 = row.get(0)?;
                Ok(modseq
                    .try_into()
                    .expect("cached highest modseq should be valid"))
            })
        })
        .await
        .expect("getting modseq should succeed")
    }

    pub async fn update(&self, data: LocalMailMetadata) {
        trace!("updating mail cache {data:?}");
        self.execute(move |db| {
            let mut stmt = db.prepare_cached("update mail_metadata set flags=?1 where uid=?2")?;
            stmt.execute((data.flags().bits(), data.uid().map_or(0, Into::into)))?;
            Ok(())
        })
        .await
        .expect("updating metadata should succeed");
    }

    pub async fn store(&self, data: LocalMailMetadata) -> Option<Uid> {
        trace!("storing mail cache {data:?}");
        self.execute(move |db| {
            if let Some(uid) = data.uid() {
                let mut stmt = db.prepare_cached(
                    "insert into mail_metadata (uid,flags,fileprefix) values (?1,?2,?3)",
                )?;
                stmt.execute((u32::from(uid), data.flags().bits(), &data.fileprefix()))?;
                Ok(None)
            } else {
                let mut stmt = db
                    .prepare_cached("insert into mail_metadata (flags,fileprefix) values (?1,?2)")
                    .expect("insert mail metadata statement should be preparable");
                stmt.execute((data.flags().bits(), &data.fileprefix()))
                    .expect("mail metadata should be insertable");
                Ok(Some(
                    db.last_insert_rowid()
                        .try_into()
                        .expect("newly stored mail id should be parsable to Uid"),
                ))
            }
        })
        .await
        .expect("storing mail should succeed")
    }

    pub async fn exists(&self, uid: Option<Uid>) -> Option<LocalMailMetadata> {
        trace!("checking existence of {uid:?}");
        self.execute(move |db| {
            let mut stmt = db
                .prepare_cached("select * from mail_metadata where uid = ?1")
                .expect("selection of existing mails should be preparable");
            stmt.query_one([uid.map_or(0, Into::into)], |row| {
                Ok(row.try_into().expect("stateentry should be parsable"))
            })
            .optional()
        })
        .await
        .expect("existence of uid should be queryable")
    }

    // todo: think about streaming this
    pub async fn for_each(&self, mut cb: impl FnMut(&LocalMailMetadata)) {
        trace!("consuming all cached mail data");
        let rows: Vec<LocalMailMetadata> = self
            .execute(move |db| {
                let mut stmt = db
                    .prepare("select (uid,flags,fileprefix) from mail_metadata;")
                    .expect("select all mail_metadata should be preparable");
                stmt.query_map([], |row| LocalMailMetadata::try_from(row))?
                    .collect()
            })
            .await
            .expect("all metadata should be selectable");
        for row in rows {
            cb(&row);
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
