use std::collections::HashMap;

use crossfire::{
    MAsyncTx,
    mpsc::Array,
    oneshot::{RxOneshot, TxOneshot, oneshot},
};

use enumflags2::BitFlags;

use crate::{
    imap::RemoteMailMetadata,
    maildir::{FlagChange, LocalChanges, LocalMail, NewLocalMailMetadata},
    repository::{Flag, ModSeq, SequenceSet, Uid},
    sync::{Error, Task},
};

pub struct RepositoryHandle {
    task_tx: MAsyncTx<Array<Task>>,
}

impl RepositoryHandle {
    pub fn new(task_tx: MAsyncTx<Array<Task>>) -> Self {
        Self { task_tx }
    }

    async fn send(&self, task: Task) -> Result<(), Error> {
        self.task_tx
            .send(task)
            .await
            .map_err(|_| Error::TaskProcessingStopped)
    }

    async fn request<R>(&self, task: impl FnOnce(TxOneshot<R>) -> Task) -> Result<R, Error> {
        let (reply_tx, reply_rx): (_, RxOneshot<R>) = oneshot();
        self.send(task(reply_tx)).await?;
        reply_rx.await.map_err(|_| Error::TaskProcessingStopped)
    }

    pub async fn delete(&self, uids: SequenceSet) -> Result<(), Error> {
        self.send(Task::Delete(uids)).await
    }

    pub async fn apply_remote_updates(
        &self,
        updates: Vec<RemoteMailMetadata>,
    ) -> Result<Vec<Uid>, Error> {
        self.request(|reply| Task::ApplyRemoteUpdates(updates, reply))
            .await
    }

    pub async fn add_synced(&self, synced: Vec<(NewLocalMailMetadata, Uid)>) -> Result<(), Error> {
        self.send(Task::AddSynced(synced)).await
    }

    pub async fn apply_flag_changes(
        &self,
        uids: Vec<Uid>,
        change: FlagChange,
    ) -> Result<(), Error> {
        self.send(Task::ApplyFlagChanges(uids, change)).await
    }

    pub async fn read_mail(
        &self,
        metadata: NewLocalMailMetadata,
    ) -> Result<Option<LocalMail>, Error> {
        self.request(|reply| Task::ReadMail(metadata, reply)).await
    }

    pub async fn flags(&self, uids: Vec<Uid>) -> Result<HashMap<Uid, BitFlags<Flag>>, Error> {
        self.request(|reply| Task::Flags(uids, reply)).await
    }

    pub async fn detect_changes(&self) -> Result<LocalChanges, Error> {
        self.request(Task::DetectChanges).await
    }

    pub async fn highest_modseq(&self) -> Result<ModSeq, Error> {
        self.request(Task::HighestModseq).await
    }
}
