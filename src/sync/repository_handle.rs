use crossfire::{
    MAsyncTx,
    mpsc::Array,
    oneshot::{RxOneshot, TxOneshot, oneshot},
};

use crate::{
    imap::RemoteMailMetadata,
    maildir::{FlagChange, NewLocalMailMetadata},
    repository::{ModSeq, SequenceSet, Uid},
    sync::Task,
};

/// Access to the maildir repository owned by the task processor. Requests are processed in
/// order with all other tasks.
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

    /// Returns uids of mails, that do not exist locally
    pub async fn apply_remote_updates(&self, updates: Vec<RemoteMailMetadata>) -> Vec<Uid> {
        self.request(|reply| Task::ApplyRemoteUpdates(updates, reply))
            .await
    }

    pub async fn add_synced(&self, metadata: NewLocalMailMetadata, uid: Uid) {
        self.send(Task::AddSynced(metadata, uid)).await;
    }

    pub async fn apply_flag_change(&self, uid: Uid, change: FlagChange) {
        self.send(Task::ApplyFlagChange(uid, change)).await;
    }

    pub async fn highest_modseq(&self) -> ModSeq {
        self.request(Task::HighestModseq).await
    }
}
