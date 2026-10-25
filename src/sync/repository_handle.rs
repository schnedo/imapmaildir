use std::collections::HashMap;

use crossfire::{
    MAsyncTx,
    mpsc::Array,
    oneshot::{RxOneshot, TxOneshot, oneshot},
};

use enumflags2::BitFlags;

use crate::{
    imap::RemoteMailMetadata,
    maildir::{FlagChange, LocalChanges, LocalMail, NewLocalMailMetadata, OtherFlags},
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

    /// Records mails stored on the server. Gets queued, even if the returned future gets dropped,
    /// e.g. when abandoning the connection while shutting down, as storing them again would
    /// duplicate them on the server. Task processing runs until it got queued.
    pub async fn add_synced(&self, synced: Vec<(NewLocalMailMetadata, Uid)>) -> Result<(), Error> {
        let task_tx = self.task_tx.clone();
        let (queued_tx, queued_rx): (_, RxOneshot<bool>) = oneshot();
        compio::runtime::spawn(async move {
            queued_tx.send(task_tx.send(Task::AddSynced(synced)).await.is_ok());
        })
        .detach();

        match queued_rx.await {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(Error::TaskProcessingStopped),
        }
    }

    pub async fn apply_flag_changes(
        &self,
        uids: Vec<Uid>,
        change: FlagChange,
    ) -> Result<(), Error> {
        self.send(Task::ApplyFlagChanges(uids, change)).await
    }

    pub async fn update_other_flags(
        &self,
        other_flags: Vec<(Uid, OtherFlags)>,
    ) -> Result<(), Error> {
        self.send(Task::UpdateOtherFlags(other_flags)).await
    }

    pub async fn open_mail(
        &self,
        metadata: NewLocalMailMetadata,
    ) -> Result<Option<LocalMail>, Error> {
        self.request(|reply| Task::OpenMail(metadata, reply)).await
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

    /// Stored UIDs missing from the ascending `uids` in ascending order
    pub async fn uids_not_in(&self, uids: Vec<Uid>) -> Result<Vec<Uid>, Error> {
        self.request(|reply| Task::UidsNotIn(uids, reply)).await
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use assertables::*;
    use crossfire::mpsc;

    use super::*;

    #[compio::test]
    async fn test_add_synced_gets_queued_after_being_dropped() {
        let (task_tx, task_rx) = mpsc::bounded_async(1);
        let repository = RepositoryHandle::new(task_tx);
        assert_ok!(
            repository
                .delete(SequenceSet::from(assert_ok!(Uid::try_from(1))))
                .await
        );
        {
            let adding = pin!(repository.add_synced(Vec::new()));
            assert!(futures::poll!(adding).is_pending());
        }
        drop(repository);

        assert_matches!(task_rx.recv().await, Ok(Task::Delete(_)));
        assert_matches!(task_rx.recv().await, Ok(Task::AddSynced(_)));
        assert!(task_rx.recv().await.is_err());
    }
}
