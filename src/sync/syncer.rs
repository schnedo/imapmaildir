use crate::{
    imap::{
        IdleStopReason, RemoteChanges, RemoteMail, RemoteMailMetadata, SelectedClient, Selection,
    },
    maildir::{FlagChange, LocalChanges, LocalEvent, LocalWatcher, MaildirRepositoryError},
    repository::{ModSeq, SequenceSet, Uid},
    sync::{Config, RepositoryHandle, task::Task},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Duration,
};

use compio::runtime::JoinHandle;
use crossfire::{
    AsyncRx,
    mpsc::{self, Array},
};
use futures::future::OptionFuture;

use crate::{imap::AuthenticatedClient, maildir::MaildirRepository};

pub struct Syncer {}

struct Synced {
    client: SelectedClient,
    task_handle: JoinHandle<()>,
    repository: RepositoryHandle,
    local_watcher: LocalWatcher,
}

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
        let Synced {
            client,
            task_handle,
            repository,
            ..
        } = Self::sync(
            mailbox,
            mail_dir,
            state_dir,
            client,
            config,
            on_local_change,
        )
        .await;
        drop(repository);
        let ((), task_result) = futures::join!(client.logout(), task_handle);
        task_result.expect("task processing should finish successfully");
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
        let Synced {
            mut client,
            task_handle,
            repository,
            local_watcher,
        } = Self::sync(
            mailbox,
            mail_dir,
            state_dir,
            client,
            config,
            on_local_change.clone(),
        )
        .await;
        task_handle.detach();
        let stop_tx = client.take_idle_stop_tx();

        log::info!("listening for mail changes...");
        let local_change_rx = local_watcher.watch().await;
        compio::runtime::spawn(async move {
            while let Ok(event) = local_change_rx.recv().await {
                log::trace!("detected local changes {event:?}");
                let reason = match event {
                    LocalEvent::Changes(changes) => IdleStopReason::Local { changes },
                    LocalEvent::Overflow => IdleStopReason::Rescan,
                };
                stop_tx
                    .send(reason)
                    .await
                    .expect("stop idle channel should still be open");
            }
        })
        .detach();

        loop {
            match client.idle(idle_timeout).await {
                IdleStopReason::Remote => {
                    log::info!("handling new remote changes");
                    let current_highest_modseq = repository.highest_modseq().await;
                    let updates = client.fetch_changes_since(current_highest_modseq).await;
                    let highest_modseq = updates.iter().map(RemoteMailMetadata::modseq).max();
                    let remote_changes = RemoteChanges {
                        updates,
                        deletions: None,
                    };
                    Self::handle_remote_changes(&mut client, &repository, remote_changes).await;
                    if let Some(highest_modseq) = highest_modseq {
                        client.commit_highest_modseq(highest_modseq).await;
                    }
                }
                reason @ (IdleStopReason::Local { .. } | IdleStopReason::Rescan) => {
                    log::info!("handling new local changes");
                    let changes = if let IdleStopReason::Local { changes } = reason {
                        changes
                    } else {
                        log::warn!("rescanning maildir for local changes");
                        repository.detect_changes().await
                    };
                    let highest_modseq = repository.highest_modseq().await;
                    Self::handle_local_changes(
                        &mut client,
                        changes,
                        highest_modseq,
                        mailbox,
                        &repository,
                    )
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
    ) -> Synced {
        let mailbox_maildir = mail_dir.join(mailbox);
        let mailbox_statedir = state_dir.join(mailbox);

        let loaded = if let Ok(maildir_repository) =
            MaildirRepository::load(&mailbox_maildir, &mailbox_statedir).await
        {
            let initial_sync_complete = maildir_repository
                .is_initial_sync_complete()
                .await
                .expect("checking for completed initial sync should succeed");
            Some((maildir_repository, initial_sync_complete))
        } else {
            None
        };

        match loaded {
            Some((maildir_repository, true)) => {
                Self::sync_existing(maildir_repository, client, mailbox, config, on_local_change)
                    .await
            }
            Some((maildir_repository, false)) => {
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
            None => {
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
        maildir_repository: MaildirRepository,
        client: AuthenticatedClient,
        mailbox: &str,
        config: Config,
        on_local_change: impl FnMut() + Send + 'static,
    ) -> Synced {
        let (task_tx, task_rx) = mpsc::bounded_async(config.store_batch_size());
        let repository = RepositoryHandle::new(task_tx.clone());
        let (local_changes, (uid_validity, highest_modseq, selection)) =
            futures::join!(maildir_repository.detect_changes(), async {
                let (uid_validity, highest_modseq) = futures::join!(
                    maildir_repository.uid_validity(),
                    maildir_repository.highest_modseq()
                );
                let uid_validity = uid_validity.expect("getting uid_validity should succeed");
                let highest_modseq = highest_modseq.expect("getting highest_modseq should succeed");
                let selection = client
                    .qresync_select(task_tx, mailbox, uid_validity, highest_modseq)
                    .await;
                (uid_validity, highest_modseq, selection)
            });
        let mut local_changes = local_changes.expect("detecting local changes should succeed");
        let Selection {
            mut client,
            remote_changes,
            mailbox_data,
            ..
        } = selection;
        let local_watcher = maildir_repository.local_watcher();
        let task_handle =
            Self::setup_task_processing(maildir_repository, task_rx, config, on_local_change);

        assert_eq!(
            uid_validity,
            mailbox_data.uid_validity(),
            "remote uid validity should be the same as local"
        );

        Self::handle_conflicts(&remote_changes, &mut local_changes);
        Self::handle_remote_changes(&mut client, &repository, remote_changes).await;
        client
            .commit_highest_modseq(mailbox_data.highest_modseq())
            .await;
        Self::handle_local_changes(
            &mut client,
            local_changes,
            highest_modseq,
            mailbox,
            &repository,
        )
        .await;

        Synced {
            client,
            task_handle,
            repository,
            local_watcher,
        }
    }

    async fn handle_local_changes(
        client: &mut SelectedClient,
        local_changes: LocalChanges,
        highest_modseq: ModSeq,
        mailbox: &str,
        repository: &RepositoryHandle,
    ) {
        let LocalChanges {
            updates,
            deletions,
            news,
        } = local_changes;
        let synced: Vec<_> = client
            .store(mailbox, news.into_iter())
            .await
            .into_iter()
            .map(|(uid, metadata)| (metadata, uid))
            .collect();
        if !synced.is_empty() {
            repository.add_synced(synced).await;
        }
        let updates = updates.build();
        let mut rejected = Vec::new();
        for (change, sequence_set) in updates.iter() {
            let rejected_uids = client
                .store_flag_change(highest_modseq, change, sequence_set)
                .await;
            let accepted: Vec<Uid> = sequence_set
                .iter()
                .filter(|uid| !rejected_uids.contains(uid))
                .collect();
            if !accepted.is_empty() {
                repository.apply_flag_changes(accepted, change).await;
            }
            rejected.extend(rejected_uids.into_iter().map(|uid| (uid, Some(change))));
        }
        if let Ok(set) = SequenceSet::try_from(&deletions) {
            let rejected_uids = client.delete(highest_modseq, &set).await;
            rejected.extend(rejected_uids.into_iter().map(|uid| (uid, None)));
        }
        if !rejected.is_empty() {
            Self::retry_rejected_changes(client, repository, rejected).await;
        }
    }

    async fn retry_rejected_changes(
        client: &mut SelectedClient,
        repository: &RepositoryHandle,
        rejected: Vec<(Uid, Option<FlagChange>)>,
    ) {
        let uids: Vec<Uid> = rejected.iter().map(|(uid, _)| *uid).collect();
        let set = SequenceSet::try_from(&uids).expect("rejected changes should not be empty");
        let remote: HashMap<Uid, RemoteMailMetadata> = client
            .fetch_metadata(&set)
            .await
            .into_iter()
            .map(|metadata| (metadata.uid(), metadata))
            .collect();
        let local = repository.flags(uids).await;
        let mut conflicts = Vec::new();
        for (uid, change) in rejected {
            let Some(remote_mail) = remote.get(&uid) else {
                continue;
            };
            let set = SequenceSet::from(uid);
            let still_rejected = if local.get(&uid) == Some(&remote_mail.flags()) {
                match change {
                    Some(change) => !client
                        .store_flag_change(remote_mail.modseq(), change, &set)
                        .await
                        .is_empty(),
                    None => !client.delete(remote_mail.modseq(), &set).await.is_empty(),
                }
            } else {
                true
            };
            match (change, still_rejected) {
                (Some(change), false) => repository.apply_flag_changes(vec![uid], change).await,
                (Some(change), true) => {
                    log::warn!("mail {uid} changed on server. Discarding local flag change");
                    repository.apply_flag_changes(vec![uid], change).await;
                    conflicts.push(remote_mail.clone());
                }
                (None, false) => {}
                (None, true) => {
                    log::warn!("mail {uid} changed on server. Restoring locally deleted mail");
                    repository.delete(set).await;
                    conflicts.push(remote_mail.clone());
                }
            }
        }
        if !conflicts.is_empty() {
            let remote_changes = RemoteChanges {
                updates: conflicts,
                deletions: None,
            };
            Self::handle_remote_changes(client, repository, remote_changes).await;
        }
    }

    async fn handle_remote_changes(
        client: &mut SelectedClient,
        repository: &RepositoryHandle,
        remote_changes: RemoteChanges,
    ) {
        if let Some(set) = remote_changes.deletions {
            repository.delete(set).await;
        }

        let missing = repository
            .apply_remote_updates(remote_changes.updates)
            .await;
        if let Ok(sequence_set) = SequenceSet::try_from(&missing) {
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
    ) -> Synced {
        let (task_tx, task_rx) = mpsc::bounded_async(config.store_batch_size());
        let repository = RepositoryHandle::new(task_tx.clone());
        let (
            Selection {
                mut client,
                mailbox_data,
                ..
            },
            local_state,
        ) = futures::join!(
            client.select(task_tx, mailbox),
            OptionFuture::from(maildir_repository.as_ref().map(|repository| async {
                futures::join!(repository.uid_validity(), repository.uids())
            }))
        );

        let (maildir_repository, local_uids) = if let Some(maildir_repository) = maildir_repository
            && let Some((uid_validity, local_uids)) = local_state
        {
            assert_eq!(
                uid_validity.expect("getting uid_validity should succeed"),
                mailbox_data.uid_validity(),
                "remote uid validity should be the same as local"
            );
            (
                maildir_repository,
                local_uids.expect("getting stored uids should succeed"),
            )
        } else {
            (
                MaildirRepository::try_init(mailbox_data.uid_validity(), mail_dir, state_dir)
                    .await
                    .expect("initializing maildir repository should succeed"),
                HashSet::new(),
            )
        };
        let local_watcher = maildir_repository.local_watcher();
        let task_handle =
            Self::setup_task_processing(maildir_repository, task_rx, config, on_local_change);

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
            Self::handle_remote_changes(&mut client, &repository, remote_changes).await;
        }
        client
            .complete_initial_sync(mailbox_data.highest_modseq())
            .await;

        Synced {
            client,
            task_handle,
            repository,
            local_watcher,
        }
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

    async fn process_task(
        maildir_repository: &MaildirRepository,
        task: Task,
        on_local_change: &mut impl FnMut(),
    ) {
        match task {
            Task::NewMail(_) => unreachable!("new mail should be handled in batches"),
            Task::Delete(sequence_set) => {
                maildir_repository
                    .delete_all(sequence_set.iter())
                    .await
                    .expect("deleting mails should succeed");
                on_local_change();
            }
            Task::SetHighestModseq(modseq) => {
                log::debug!("Setting highest modseq to {modseq}");
                maildir_repository
                    .update_highest_modseq(modseq)
                    .await
                    .expect("setting highest_modseq should succeed");
            }
            Task::CompleteInitialSync(modseq, done_tx) => {
                log::debug!("Completing initial sync with highest modseq {modseq}");
                maildir_repository
                    .complete_initial_sync(modseq)
                    .await
                    .expect("completing initial sync should succeed");
                done_tx.send(());
            }
            Task::ApplyRemoteUpdates(updates, reply) => {
                let (missing, changed) = maildir_repository
                    .apply_remote_updates(&updates)
                    .await
                    .unwrap_or_else(|e| panic!("updating flags should succeed: {e}"));
                if changed {
                    on_local_change();
                }
                reply.send(missing);
            }
            Task::AddSynced(synced) => {
                maildir_repository
                    .add_synced_all(synced)
                    .await
                    .expect("writing back synced uids should succeed");
            }
            Task::ApplyFlagChanges(uids, change) => {
                maildir_repository
                    .apply_flag_changes(&uids, change)
                    .await
                    .expect("updating flags in maildir_repository should succeed");
            }
            Task::Flags(uids, reply) => {
                reply.send(
                    maildir_repository
                        .flags(uids)
                        .await
                        .expect("getting flags should succeed"),
                );
            }
            Task::DetectChanges(reply) => {
                reply.send(
                    maildir_repository
                        .detect_changes()
                        .await
                        .expect("detecting local changes should succeed"),
                );
            }
            Task::HighestModseq(reply) => {
                reply.send(
                    maildir_repository
                        .highest_modseq()
                        .await
                        .expect("getting highest modseq should succeed"),
                );
            }
            Task::UpdateFlags(remote_mail_metadata) => {
                log::debug!(
                    "Setting flags of mail {} to {}",
                    remote_mail_metadata.uid(),
                    remote_mail_metadata.flags()
                );
                match maildir_repository.update_flags(&remote_mail_metadata).await {
                    Ok(()) => on_local_change(),
                    Err(MaildirRepositoryError::NoExists { uid }) => {
                        log::warn!("ignoring flag update of locally missing mail {uid}");
                    }
                    Err(e) => panic!("updating flags should succeed: {e}"),
                }
            }
        }
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
                    task => {
                        Self::process_task(&maildir_repository, task, &mut on_local_change).await;
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
