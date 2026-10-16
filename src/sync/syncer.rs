use crate::{
    imap::{
        IdleStopReason, RemoteChanges, RemoteMail, RemoteMailMetadata, SelectedClient, Selection,
    },
    maildir::LocalChanges,
    repository::{SequenceSet, SequenceSetBuilder, Uid},
    sync::{Config, task::Task},
};
use std::{collections::HashSet, path::Path, time::Duration};

use compio::runtime::JoinHandle;
use crossfire::{
    AsyncRx,
    mpsc::{self, Array},
};

use crate::{imap::AuthenticatedClient, maildir::MaildirRepository};

pub struct Syncer {}

impl Syncer {
    #[expect(clippy::missing_panics_doc)]
    pub async fn sync_once(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
        config: Config,
        on_local_change: impl FnMut() + Send + 'static,
    ) {
        let ((client, task_handle), _) = Self::sync(
            mailbox,
            mail_dir,
            state_dir,
            client,
            config,
            on_local_change,
        )
        .await;
        client.logout().await;
        task_handle
            .await
            .expect("task processing should finish successfully");
    }

    #[expect(clippy::missing_panics_doc)]
    pub async fn sync_continuously(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
        idle_timeout: Duration,
        config: Config,
        mut on_local_change: impl FnMut() + Clone + Send + 'static,
    ) -> ! {
        let ((mut client, task_handle), maildir_repository) = Self::sync(
            mailbox,
            mail_dir,
            state_dir,
            client,
            config,
            on_local_change.clone(),
        )
        .await;
        task_handle.detach();
        let repo = maildir_repository.clone();
        let stop_tx = client.idle_stop_tx();

        log::info!("listening for mail changes...");
        let local_change_rx = repo.watch().await;
        compio::runtime::spawn(async move {
            while let Ok(changes) = local_change_rx.recv().await {
                log::trace!("detected local changes {changes:?}");
                stop_tx
                    .send(IdleStopReason::Local { changes })
                    .await
                    .expect("stop idle channel should still be open");
            }
        })
        .detach();

        loop {
            match client.idle(idle_timeout).await {
                IdleStopReason::Remote => {
                    log::info!("handling new remote changes");
                    let current_highest_modseq = maildir_repository
                        .highest_modseq()
                        .expect("getting highest modseq should succeed");
                    client.fetch_since(current_highest_modseq).await;
                }
                IdleStopReason::Local { changes } => {
                    log::info!("handling new local changes");
                    Self::handle_local_changes(&mut client, changes, mailbox, &maildir_repository)
                        .await;
                    on_local_change();
                }
                IdleStopReason::Timeout => {
                    log::info!(
                        "idle timed out after {} seconds. Reissuing...",
                        idle_timeout.as_secs()
                    );
                    // todo: optional client.fetch_since for gmail support
                }
            }
        }
    }

    async fn sync(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
        config: Config,
        on_local_change: impl FnMut() + Send + 'static,
    ) -> ((SelectedClient, JoinHandle<()>), MaildirRepository) {
        let mailbox_maildir = mail_dir.join(mailbox);
        let mailbox_statedir = state_dir.join(mailbox);

        match MaildirRepository::load(&mailbox_maildir, &mailbox_statedir).await {
            Ok(maildir_repository)
                if maildir_repository
                    .is_initial_sync_complete()
                    .expect("checking for completed initial sync should succeed") =>
            {
                (
                    Self::sync_existing(
                        &maildir_repository,
                        client,
                        mailbox,
                        config,
                        on_local_change,
                    )
                    .await,
                    maildir_repository,
                )
            }
            Ok(maildir_repository) => {
                log::info!("resuming interrupted initial sync");
                Self::sync_initial(
                    Some(maildir_repository),
                    client,
                    &mailbox_maildir,
                    &mailbox_statedir,
                    mailbox,
                    config,
                    on_local_change,
                )
                .await
            }
            Err(_) => {
                log::info!("no existing maildir found. Running inital sync");
                Self::sync_initial(
                    None,
                    client,
                    &mailbox_maildir,
                    &mailbox_statedir,
                    mailbox,
                    config,
                    on_local_change,
                )
                .await
            }
        }
    }

    async fn sync_existing(
        maildir_repository: &MaildirRepository,
        client: AuthenticatedClient,
        mailbox: &str,
        config: Config,
        on_local_change: impl FnMut() + Send + 'static,
    ) -> (SelectedClient, JoinHandle<()>) {
        let uid_validity = maildir_repository
            .uid_validity()
            .expect("getting uid_validity should succeed");
        let highest_modseq = maildir_repository
            .highest_modseq()
            .expect("getting highest_modseq should succeed");
        let mut local_changes = maildir_repository
            .detect_changes()
            .await
            .expect("detecting local changes should succeed");
        let (task_tx, task_rx) = mpsc::bounded_async(config.store_batch_size());
        let task_handle = Self::setup_task_processing(
            maildir_repository.clone(),
            task_rx,
            config,
            on_local_change,
        );

        let Selection {
            mut client,
            remote_changes,
            mailbox_data,
            ..
        } = client
            .qresync_select(task_tx, mailbox, uid_validity, highest_modseq)
            .await;
        assert_eq!(
            uid_validity,
            mailbox_data.uid_validity(),
            "remote uid validity should be the same as local"
        );

        Self::handle_conflicts(&remote_changes, &mut local_changes);
        Self::handle_remote_changes(&mut client, maildir_repository, remote_changes).await;
        client
            .commit_highest_modseq(mailbox_data.highest_modseq())
            .await;
        Self::handle_local_changes(&mut client, local_changes, mailbox, maildir_repository).await;
        (client, task_handle)
    }

    async fn handle_local_changes(
        client: &mut SelectedClient,
        local_changes: LocalChanges,
        mailbox: &str,
        maildir_repository: &MaildirRepository,
    ) {
        let LocalChanges {
            highest_modseq,
            updates,
            deletions,
            news,
        } = local_changes;
        let mailinfos = client.store(mailbox, news.into_iter()).await;
        // todo: parallelize these
        while let Ok((uid, metadata)) = mailinfos.recv().await {
            maildir_repository
                .add_synced(metadata, uid)
                .await
                .expect("writing back synced uid should succeed");
        }
        let updates = updates.build();
        for (change, sequence_set) in updates.iter() {
            let rejected = client
                .store_flag_change(highest_modseq, change, sequence_set)
                .await;
            for uid in sequence_set.iter().filter(|uid| !rejected.contains(uid)) {
                maildir_repository
                    .apply_flag_change(uid, change)
                    .await
                    .expect("updating flags in maildir_repository should succeed");
            }
        }
        if let Ok(set) = SequenceSet::try_from(&deletions) {
            client.delete(highest_modseq, &set).await;
        }
    }

    async fn handle_remote_changes(
        client: &mut SelectedClient,
        maildir_repository: &MaildirRepository,
        remote_changes: RemoteChanges,
    ) {
        if let Some(set) = remote_changes.deletions {
            for uid in set.iter() {
                maildir_repository
                    .delete(uid)
                    .await
                    .expect("deleting mails should succeed");
            }
        }

        let mut refetch_mails = SequenceSetBuilder::default();
        for update in &remote_changes.updates {
            if let Err(error) = maildir_repository.update_flags(update).await {
                match error {
                    crate::maildir::Error::Maildir(_) => todo!("handle error"),
                    crate::maildir::Error::State(_) => todo!("handle error"),
                    crate::maildir::Error::NoExists { uid } => refetch_mails.add(uid),
                }
            }
        }
        if let Ok(sequence_set) = refetch_mails.build() {
            client.fetch_mail(&sequence_set).await;
        }
    }

    fn handle_conflicts(remote_changes: &RemoteChanges, local_changes: &mut LocalChanges) {
        let mut remote_deletions: HashSet<Uid> = remote_changes
            .deletions
            .as_ref()
            .map_or_else(HashSet::new, |deletions| deletions.iter().collect());
        let mut remote_updates: HashSet<Uid> = remote_changes
            .updates
            .iter()
            .map(RemoteMailMetadata::uid)
            .collect();

        local_changes.deletions.retain(|deletion| {
            !remote_updates.contains(deletion) && !remote_deletions.contains(deletion)
        });
        for uid in remote_updates.drain() {
            local_changes.updates.remove(uid);
        }
        for uid in remote_deletions.drain() {
            local_changes.updates.remove(uid);
        }
    }

    async fn sync_initial(
        maildir_repository: Option<MaildirRepository>,
        client: AuthenticatedClient,
        mail_dir: &Path,
        state_dir: &Path,
        mailbox: &str,
        config: Config,
        on_local_change: impl FnMut() + Send + 'static,
    ) -> ((SelectedClient, JoinHandle<()>), MaildirRepository) {
        let (task_tx, task_rx) = mpsc::bounded_async(config.store_batch_size());
        let Selection {
            mut client,
            mailbox_data,
            ..
        } = client.select(task_tx, mailbox).await;

        let maildir_repository = if let Some(maildir_repository) = maildir_repository {
            assert_eq!(
                maildir_repository
                    .uid_validity()
                    .expect("getting uid_validity should succeed"),
                mailbox_data.uid_validity(),
                "remote uid validity should be the same as local"
            );
            maildir_repository
        } else {
            MaildirRepository::try_init(mailbox_data.uid_validity(), mail_dir, state_dir)
                .await
                .expect("initializing maildir repository should succeed")
        };
        let task_handle = Self::setup_task_processing(
            maildir_repository.clone(),
            task_rx,
            config,
            on_local_change,
        );

        let local_uids = maildir_repository
            .uids()
            .expect("getting stored uids should succeed");
        if local_uids.is_empty() {
            client.fetch_all().await;
        } else {
            let updates = client.fetch_all_metadata().await;
            let remote_uids: HashSet<Uid> = updates.iter().map(RemoteMailMetadata::uid).collect();
            let deletions: Vec<Uid> = local_uids.difference(&remote_uids).copied().collect();
            let remote_changes = RemoteChanges {
                updates,
                deletions: SequenceSet::try_from(&deletions).ok(),
            };
            Self::handle_remote_changes(&mut client, &maildir_repository, remote_changes).await;
        }
        client
            .complete_initial_sync(mailbox_data.highest_modseq())
            .await;

        ((client, task_handle), maildir_repository)
    }

    fn collect_store_batch(
        first: RemoteMail,
        task_rx: &AsyncRx<Array<Task>>,
        config: Config,
    ) -> (Vec<RemoteMail>, Option<Task>) {
        let mut bytes = first.content().len();
        let mut batch = vec![first];
        while batch.len() < config.store_batch_size() && bytes < config.store_batch_max_bytes() {
            match task_rx.try_recv() {
                Ok(Task::NewMail(remote_mail)) => {
                    bytes += remote_mail.content().len();
                    batch.push(remote_mail);
                }
                Ok(task) => return (batch, Some(task)),
                Err(_) => break,
            }
        }

        (batch, None)
    }

    fn setup_task_processing(
        maildir_repository: MaildirRepository,
        task_rx: AsyncRx<Array<Task>>,
        config: Config,
        mut on_local_change: impl FnMut() + Send + 'static,
    ) -> JoinHandle<()> {
        compio::runtime::spawn(async move {
            log::debug!("Listening to incoming mail...");
            let mut next_task = None;
            loop {
                let task = if let Some(task) = next_task.take() {
                    task
                } else if let Ok(task) = task_rx.recv().await {
                    task
                } else {
                    break;
                };
                match task {
                    Task::NewMail(remote_mail) => {
                        let (batch, task) =
                            Self::collect_store_batch(remote_mail, &task_rx, config);
                        next_task = task;
                        maildir_repository
                            .store_batch(&batch)
                            .await
                            .expect("storing new mail should succeed");
                        on_local_change();
                    }
                    Task::Delete(sequence_set) => {
                        for uid in sequence_set.iter() {
                            maildir_repository
                                .delete(uid)
                                .await
                                .expect("deleting mails should succeed");
                        }
                        on_local_change();
                    }
                    Task::SetHighestModseq(modseq) => {
                        log::debug!("Setting highest modseq to {modseq}");
                        maildir_repository
                            .update_highest_modseq(modseq)
                            .expect("setting highest_modseq should succeed");
                    }
                    Task::CompleteInitialSync(modseq, done_tx) => {
                        log::debug!("Completing initial sync with highest modseq {modseq}");
                        maildir_repository
                            .complete_initial_sync(modseq)
                            .expect("completing initial sync should succeed");
                        done_tx.send(());
                    }
                    Task::UpdateFlags(remote_mail_metadata) => {
                        log::debug!(
                            "Setting flags of mail {} to {}",
                            remote_mail_metadata.uid(),
                            remote_mail_metadata.flags()
                        );
                        maildir_repository
                            .update_flags(&remote_mail_metadata)
                            .await
                            .expect("updating flags should succeed");
                        on_local_change();
                    }
                }
            }
            log::debug!("Task channel closed. Stopping task processing");
        })
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;
    use crossfire::MAsyncTx;

    use crate::{
        imap::{RemoteContent, RemoteMailMetadata},
        repository::{Flag, ModSeq, Uid},
    };

    use super::*;

    fn mail(uid: u32, size: usize) -> RemoteMail {
        RemoteMail::new(
            RemoteMailMetadata::new(
                assert_ok!(Uid::try_from(uid)),
                Flag::Seen.into(),
                assert_ok!(ModSeq::try_from(i64::from(uid))),
            ),
            RemoteContent::from_string("x".repeat(size)),
        )
    }

    async fn queue(
        tasks: impl IntoIterator<Item = Task>,
    ) -> (MAsyncTx<Array<Task>>, AsyncRx<Array<Task>>) {
        let (task_tx, task_rx) = mpsc::bounded_async(16);
        for task in tasks {
            assert_ok!(task_tx.send(task).await);
        }

        (task_tx, task_rx)
    }

    fn uids(batch: &[RemoteMail]) -> Vec<u32> {
        batch
            .iter()
            .map(|mail| u32::from(mail.metadata().uid()))
            .collect()
    }

    #[compio::test]
    async fn test_collect_store_batch_collects_queued_mails() {
        let (_task_tx, task_rx) =
            queue([Task::NewMail(mail(2, 1)), Task::NewMail(mail(3, 1))]).await;

        let (batch, next) = Syncer::collect_store_batch(mail(1, 1), &task_rx, Config::default());

        assert_eq!(uids(&batch), vec![1, 2, 3]);
        assert!(next.is_none());
    }

    #[compio::test]
    async fn test_collect_store_batch_respects_count_limit() {
        let (_task_tx, task_rx) =
            queue([Task::NewMail(mail(2, 1)), Task::NewMail(mail(3, 1))]).await;

        let (batch, next) = Syncer::collect_store_batch(mail(1, 1), &task_rx, Config::new(2, 100));

        assert_eq!(uids(&batch), vec![1, 2]);
        assert!(next.is_none());
        assert_matches!(task_rx.try_recv(), Ok(Task::NewMail(_)));
    }

    #[compio::test]
    async fn test_collect_store_batch_respects_byte_limit() {
        let (_task_tx, task_rx) =
            queue([Task::NewMail(mail(2, 6)), Task::NewMail(mail(3, 1))]).await;

        let (batch, _) = Syncer::collect_store_batch(mail(1, 5), &task_rx, Config::new(10, 10));

        assert_eq!(uids(&batch), vec![1, 2]);
    }

    #[compio::test]
    async fn test_collect_store_batch_takes_single_oversized_mail() {
        let (_task_tx, task_rx) = queue([Task::NewMail(mail(2, 1))]).await;

        let (batch, _) = Syncer::collect_store_batch(mail(1, 20), &task_rx, Config::new(10, 10));

        assert_eq!(uids(&batch), vec![1]);
    }

    #[compio::test]
    async fn test_collect_store_batch_ends_on_other_task() {
        let (_task_tx, task_rx) = queue([
            Task::NewMail(mail(2, 1)),
            Task::SetHighestModseq(assert_ok!(ModSeq::try_from(9))),
            Task::NewMail(mail(3, 1)),
        ])
        .await;

        let (batch, next) = Syncer::collect_store_batch(mail(1, 1), &task_rx, Config::default());

        assert_eq!(uids(&batch), vec![1, 2]);
        assert_matches!(next, Some(Task::SetHighestModseq(_)));
    }
}
