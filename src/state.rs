use std::{
    convert::Into,
    fmt::Debug,
    fs::create_dir_all,
    path::{Path, PathBuf},
};

use enumflags2::BitFlag;
use log::{debug, trace};
use rusqlite::{Connection, Error, OpenFlags, OptionalExtension, Result, Row};
use tokio::sync::{mpsc, oneshot};

use crate::{
    imap::{ModSeq, Uid, UidValidity},
    maildir::LocalMailMetadata,
    sync::Flag,
};

struct SyncState {
    db: Connection,
}

impl SyncState {
    fn create(
        open_tx: oneshot::Sender<Result<(), Error>>,
        mut task_rx: mpsc::Receiver<Task>,
        state: Result<Self, Error>,
    ) {
        match state {
            Ok(db) => {
                open_tx
                    .send(Ok(()))
                    .expect("database load channel should send");
                while let Some(task) = task_rx.blocking_recv() {
                    db.run(task);
                }
            }
            Err(e) => open_tx
                .send(Err(e))
                .expect("database load channel should send"),
        }
    }

    fn load(state_file: &Path) -> Result<Self, Error> {
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

    pub fn init(state_file: &Path, uid_validity: UidValidity) -> Result<Self, Error> {
        debug!("creating new state file {}", state_file.to_string_lossy());
        let db = Connection::open(state_file)?;
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
        trace!("setting cached uid_validity {uid_validity}");
        db.execute(
            "insert or ignore into uid_validity (uid_validity) values (?1)",
            [u32::from(uid_validity)],
        )
        .expect("uid_validity should be settable");

        Ok(Self { db })
    }

    #[expect(clippy::too_many_lines)]
    pub fn run(&self, task: Task) {
        match task {
            Task::SetHighestModseq(value, sender) => {
                trace!("setting cached highest_modseq {value}");
                {
                    self.db
                        .pragma_update(None, "user_version", u64::from(value))
                        .expect("setting modseq should succeed");
                    sender.send(())
                }
                .expect("db task return channel should still be open");
            }
            Task::GetHighestModseq(sender) => {
                trace!("getting cached highest_modseq");
                sender
                    .send(
                        self.db
                            .query_one("select * from pragma_user_version", [], |row| {
                                let modseq: u64 = row.get(0)?;
                                Ok(modseq
                                    .try_into()
                                    .expect("cached highest modseq should be valid"))
                            })
                            .expect("getting modseq should succeed"),
                    )
                    .expect("db task return channel should still be open");
            }
            Task::GetAll(sender) => {
                trace!("getting all stored mail metadata");
                let mut stmt = self
                    .db
                    .prepare_cached("select uid,flags,fileprefix from mail_metadata;")
                    .expect("select all mail_metadata should be preparable");
                sender
                    .send(
                        stmt.query_map([], |row| LocalMailMetadata::try_from(row))
                            .expect("all metadata should be selectable")
                            .map(|maybe_row| {
                                maybe_row
                                    .expect("local mail metadata should be buildable from db row")
                            })
                            .collect(),
                    )
                    .expect("db task return channel should still be open");
            }
            Task::DeleteByUid(uid, sender) => {
                trace!("deleting {uid:?}");
                let mut stmt = self
                    .db
                    .prepare_cached("delete from mail_metadata where uid = ?1")
                    .expect("deletion of existing mails should be preparable");
                stmt.execute([u32::from(uid)])
                    .expect("deletion of existing mail should succeed");

                sender
                    .send(())
                    .expect("db task return channel should still be open");
            }
            Task::GetByUid(uid, sender) => {
                trace!("get existing metadata with {uid:?}");
                let mut stmt = self
                    .db
                    .prepare_cached("select * from mail_metadata where uid = ?1")
                    .expect("selection of existing mails should be preparable");
                sender
                    .send(
                        stmt.query_one([u32::from(uid)], |row| {
                            Ok(row.try_into().expect("stateentry should be parsable"))
                        })
                        .optional()
                        .expect("existing matadata should be queryable"),
                    )
                    .expect("db task return channel should still be open");
            }
            Task::Store(local_mail_metadata, sender) => {
                trace!("storing mail cache {local_mail_metadata:?}");
                let uid = local_mail_metadata
                    .uid()
                    .expect("stored mail should have uid");
                let mut stmt = self
                    .db
                    .prepare_cached(
                        "insert into mail_metadata (uid,flags,fileprefix) values (?1,?2,?3)",
                    )
                    .expect("preparation of cached insert mail metadata should succeed");
                stmt.execute((
                    u32::from(uid),
                    local_mail_metadata.flags().bits(),
                    &local_mail_metadata.fileprefix(),
                ))
                .expect("storing mail should succeed");

                sender
                    .send(())
                    .expect("db task return channel should still be open");
            }
            Task::Update(local_mail_metadata, sender) => {
                trace!("updating mail cache {local_mail_metadata:?}");
                let mut stmt = self
                    .db
                    .prepare_cached("update mail_metadata set flags=?1 where uid=?2")
                    .expect("preparation of cached update mail statement should succeed");
                stmt.execute((
                    local_mail_metadata.flags().bits(),
                    local_mail_metadata.uid().map_or(0, Into::into),
                ))
                .expect("updating metadata should succeed");
                sender
                    .send(())
                    .expect("db task return channel should still be open");
            }
            Task::GetUidValidity(sender) => {
                trace!("getting cached uid_validity");
                sender
                    .send(
                        self.db
                            .query_one("select * from uid_validity", (), |row| {
                                let validity: u32 = row.get(0)?;
                                let validity = validity
                                    .try_into()
                                    .expect("cached uid validity should be spec compliant");
                                Ok(validity)
                            })
                            .expect("uid_validity should be selectable"),
                    )
                    .expect("db task return channel should still be open");
            }
        }
    }
}

impl Drop for SyncState {
    fn drop(&mut self) {
        self.db
            .execute("pragma optimize;", [])
            .expect("sqlite should be optimizable");
    }
}

#[derive(Debug)]
enum Task {
    SetHighestModseq(ModSeq, oneshot::Sender<()>),
    GetHighestModseq(oneshot::Sender<ModSeq>),
    GetAll(oneshot::Sender<Vec<LocalMailMetadata>>),
    DeleteByUid(Uid, oneshot::Sender<()>),
    GetByUid(Uid, oneshot::Sender<Option<LocalMailMetadata>>),
    Store(LocalMailMetadata, oneshot::Sender<()>),
    Update(LocalMailMetadata, oneshot::Sender<()>),
    GetUidValidity(oneshot::Sender<UidValidity>),
}

#[derive(Clone)]
pub struct State {
    task_tx: mpsc::Sender<Task>,
}

impl State {
    async fn new(state: Result<SyncState, Error>) -> Result<Self, Error> {
        let (open_tx, open_rx) = oneshot::channel();
        let (task_tx, task_rx) = mpsc::channel::<Task>(32);

        // todo: use thread::spawn
        tokio::task::spawn_blocking(move || SyncState::create(open_tx, task_rx, state));

        open_rx
            .await
            .expect("database load channel should receive")?;

        Ok(Self { task_tx })
    }

    pub async fn load(state_dir: &Path, account: &str, mailbox: &str) -> Result<Self, Error> {
        let state_file = Self::prepare_state_file(state_dir, account, mailbox);

        Self::new(SyncState::load(&state_file)).await
    }

    pub async fn init(
        state_dir: &Path,
        account: &str,
        mailbox: &str,
        uid_validity: UidValidity,
    ) -> Result<Self, Error> {
        let state_file = Self::prepare_state_file(state_dir, account, mailbox);

        Self::new(SyncState::init(&state_file, uid_validity)).await
    }

    pub fn handle_highest_modseq(&self, mut highest_modseq_rx: mpsc::Receiver<ModSeq>) {
        let state = self.clone();

        tokio::spawn(async move {
            while let Some(highest_modseq) = highest_modseq_rx.recv().await {
                state.update_highest_modseq(highest_modseq).await;
            }
        });
    }

    fn prepare_state_file(state_dir: &Path, account: &str, mailbox: &str) -> PathBuf {
        let mut state_dir = state_dir.join(account);
        create_dir_all(&state_dir).expect("creation of state_dir should succeed");
        state_dir.push(mailbox);
        state_dir
    }

    pub async fn uid_validity(&self) -> UidValidity {
        trace!("getting cached uid_validity");
        let (tx, rx) = oneshot::channel();
        self.task_tx
            .send(Task::GetUidValidity(tx))
            .await
            .expect("sending GetUidValidity task should succeed");
        rx.await
            .expect("receiving GetUidValidity response should succeed")
    }

    pub async fn update_highest_modseq(&self, value: ModSeq) {
        // todo: think about using cached highest_modseq and maybe mutex
        if value > self.highest_modseq().await {
            self.set_highest_modseq(value).await;
        }
    }

    pub async fn set_highest_modseq(&self, value: ModSeq) {
        trace!("setting cached highest_modseq {value}");
        let (tx, rx) = oneshot::channel();
        self.task_tx
            .send(Task::SetHighestModseq(value, tx))
            .await
            .expect("sending SetHighestModseq task should succeed");
        rx.await
            .expect("receiving SetHighestModseq response should succeed");
    }

    pub async fn highest_modseq(&self) -> ModSeq {
        trace!("getting cached highest_modseq");
        let (tx, rx) = oneshot::channel();
        self.task_tx
            .send(Task::GetHighestModseq(tx))
            .await
            .expect("sending GetHighestModseq task should succeed");
        rx.await
            .expect("receiving GetHighestModseq response should succeed")
    }

    pub async fn update(&self, data: LocalMailMetadata) {
        trace!("updating mail cache {data:?}");
        let (tx, rx) = oneshot::channel();
        self.task_tx
            .send(Task::Update(data, tx))
            .await
            .expect("sending Update task should succeed");
        rx.await.expect("receiving Update response should succeed");
    }

    pub async fn store(&self, data: LocalMailMetadata) {
        trace!("storing mail cache {data:?}");
        let (tx, rx) = oneshot::channel();
        self.task_tx
            .send(Task::Store(data, tx))
            .await
            .expect("sending Store task should succeed");
        rx.await.expect("receiving Store response should succeed");
    }

    pub async fn get_by_id(&self, uid: Uid) -> Option<LocalMailMetadata> {
        trace!("get existing metadata with {uid:?}");
        let (tx, rx) = oneshot::channel();
        self.task_tx
            .send(Task::GetByUid(uid, tx))
            .await
            .expect("sending GetByUid task should succeed");
        rx.await
            .expect("receiving GetByUid response should succeed")
    }

    // todo: delete multiple
    pub async fn delete_by_id(&self, uid: Uid) {
        trace!("deleting {uid:?}");
        let (tx, rx) = oneshot::channel();
        self.task_tx
            .send(Task::DeleteByUid(uid, tx))
            .await
            .expect("sending DeleteByUid task should succeed");
        rx.await
            .expect("receiving DeleteByUid response should succeed");
    }

    // todo: think about streaming this
    pub async fn for_each(&self, mut cb: impl FnMut(&LocalMailMetadata)) {
        trace!("consuming all cached mail data");
        let (tx, rx) = oneshot::channel();
        self.task_tx
            .send(Task::GetAll(tx))
            .await
            .expect("sending GetAll task should succeed");
        let rows = rx.await.expect("receiving GetAll response should succeed");
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
