use std::collections::HashMap;

use crossfire::{
    MAsyncTx,
    mpsc::Array,
    oneshot::{RxOneshot, TxOneshot, oneshot},
};

use enumflags2::BitFlags;

use crate::{
    imap::RemoteMailMetadata,
    maildir::{FlagChange, NewLocalMailMetadata},
    repository::{Flag, ModSeq, SequenceSet, Uid},
    sync::Task,
};

pub struct RepositoryHandle {
    task_tx: MAsyncTx<Array<Task>>,
}

impl RepositoryHandle {
    pub fn new(task_tx: MAsyncTx<Array<Task>>) -> Self {
        Self { task_tx }
    }

    async fn send(&self, task: Task) {
        self.task_tx
            .send(task)
            .await
            .expect("task processing should still be running");
    }

    async fn request<R>(&self, task: impl FnOnce(TxOneshot<R>) -> Task) -> R {
        let (reply_tx, reply_rx): (_, RxOneshot<R>) = oneshot();
        self.send(task(reply_tx)).await;
        reply_rx
            .await
            .expect("task processing should reply to requests")
    }

    pub async fn delete(&self, uids: SequenceSet) {
        self.send(Task::Delete(uids)).await;
    }

    pub async fn apply_remote_updates(&self, updates: Vec<RemoteMailMetadata>) -> Vec<Uid> {
        self.request(|reply| Task::ApplyRemoteUpdates(updates, reply))
            .await
    }

    pub async fn add_synced(&self, synced: Vec<(NewLocalMailMetadata, Uid)>) {
        self.send(Task::AddSynced(synced)).await;
    }

    pub async fn apply_flag_changes(&self, uids: Vec<Uid>, change: FlagChange) {
        self.send(Task::ApplyFlagChanges(uids, change)).await;
    }

    pub async fn flags(&self, uids: Vec<Uid>) -> HashMap<Uid, BitFlags<Flag>> {
        self.request(|reply| Task::Flags(uids, reply)).await
    }

    pub async fn highest_modseq(&self) -> ModSeq {
        self.request(Task::HighestModseq).await
    }
}
