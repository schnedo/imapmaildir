use crate::{
    imap::{
        IdleStopReason, RemoteChanges, RemoteMail, RemoteMailMetadata, SelectedClient, Selection,
    },
    maildir::{
        FlagChange, LocalChanges, LocalEvent, LocalMail, LocalWatcher, MaildirRepositoryError,
        NewLocalMailMetadata,
    },
    repository::{ModSeq, SequenceSet, Uid, UidValidity},
    sync::{Config, Error, RepositoryHandle, task::Task},
};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    path::Path,
    time::Duration,
};

use compio::runtime::JoinHandle;
use crossfire::{
    AsyncRx,
    mpsc::{self, Array},
};
use futures::future::OptionFuture;

use crate::{
    imap::AuthenticatedClient,
    maildir::{LoadError, MaildirRepository},
};

pub struct Syncer {}

type TaskHandle = JoinHandle<Result<(), Error>>;

struct Synced {
    client: SelectedClient,
    task_handle: TaskHandle,
    repository: RepositoryHandle,
    local_watcher: LocalWatcher,
}

impl Syncer {
    /// # Errors
    /// If syncing with the server or the local maildir fails
    pub async fn sync_once(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
        config: Config,
        on_local_change: impl FnMut() + Send + 'static,
    ) -> Result<(), Error> {
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
        .await?;
        drop(repository);
        let (logout_result, task_result) = futures::join!(client.logout(), task_handle);
        task_result.map_err(|_| Error::TaskProcessingPanicked)??;
        logout_result?;

        Ok(())
    }

    /// # Errors
    /// If the server does not support IDLE or syncing with the server or the local maildir fails
    pub async fn sync_continuously(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
        idle_timeout: Duration,
        config: Config,
        mut on_local_change: impl FnMut() + Clone + Send + 'static,
    ) -> Result<Infallible, Error> {
        client.require_idle()?;
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
        .await?;
        let stop_tx = client.take_idle_stop_tx();

        log::info!("listening for mail changes...");
        let local_change_rx = match local_watcher.watch().await {
            Ok(local_change_rx) => local_change_rx,
            Err(e) => return Err(Self::task_error(e.into(), task_handle).await),
        };
        compio::runtime::spawn(async move {
            while let Ok(event) = local_change_rx.recv().await {
                log::trace!("detected local changes {event:?}");
                let reason = match event {
                    LocalEvent::Changes(changes) => IdleStopReason::Local { changes },
                    LocalEvent::Overflow => IdleStopReason::Rescan,
                    LocalEvent::Error(e) => IdleStopReason::LocalError(e),
                };
                if stop_tx.send(reason).await.is_err() {
                    break;
                }
            }
        })
        .detach();
        // local changes during the sync, e.g. a long initial one, happened before watching
        let rescanned = async {
            let changes = repository.detect_changes().await?;
            // uploading new mails renames them to carry their UID
            let renames_local_mails = !changes.news.is_empty();
            Self::handle_local(&mut client, &repository, changes, mailbox, config).await?;
            if renames_local_mails {
                on_local_change();
            }
            Ok(())
        }
        .await;
        if let Err(e) = rescanned {
            return Err(Self::task_error(e, task_handle).await);
        }

        let error = loop {
            if let Err(e) = Self::handle_idle(
                &mut client,
                &repository,
                mailbox,
                idle_timeout,
                config,
                &mut on_local_change,
            )
            .await
            {
                break e;
            }
        };

        Err(Self::task_error(error, task_handle).await)
    }

    async fn handle_idle(
        client: &mut SelectedClient,
        repository: &RepositoryHandle,
        mailbox: &str,
        idle_timeout: Duration,
        config: Config,
        on_local_change: &mut impl FnMut(),
    ) -> Result<(), Error> {
        match client.idle(idle_timeout).await? {
            IdleStopReason::Remote => {
                log::info!("handling new remote changes");
                let current_highest_modseq = repository.highest_modseq().await?;
                let updates = client.fetch_changes_since(current_highest_modseq).await?;
                let highest_modseq = updates.iter().map(RemoteMailMetadata::modseq).max();
                let deletions = if client.take_unknown_expunged() {
                    Self::remote_deletions(client, repository).await?
                } else {
                    None
                };
                let remote_changes = RemoteChanges { updates, deletions };
                Self::handle_remote_changes(client, repository, remote_changes).await?;
                if let Some(highest_modseq) = highest_modseq {
                    client.commit_highest_modseq(highest_modseq).await?;
                }
            }
            reason @ (IdleStopReason::Local { .. } | IdleStopReason::Rescan) => {
                log::info!("handling new local changes");
                let changes = if let IdleStopReason::Local { changes } = reason {
                    changes
                } else {
                    log::warn!("rescanning maildir for local changes");
                    repository.detect_changes().await?
                };
                Self::handle_local(client, repository, changes, mailbox, config).await?;
                on_local_change();
            }
            IdleStopReason::LocalError(e) => return Err(e.into()),
            IdleStopReason::Timeout => {
                log::info!(
                    "idle timed out after {} seconds. Reissuing...",
                    idle_timeout.as_secs()
                );
            }
        }

        Ok(())
    }

    async fn handle_local(
        client: &mut SelectedClient,
        repository: &RepositoryHandle,
        changes: LocalChanges,
        mailbox: &str,
        config: Config,
    ) -> Result<(), Error> {
        let highest_modseq = repository.highest_modseq().await?;
        Self::handle_local_changes(client, changes, highest_modseq, mailbox, repository, config)
            .await
    }

    /// Replaces `error` with the cause of stopped task processing if that caused it
    async fn task_error(error: Error, task_handle: TaskHandle) -> Error {
        if error.is_task_processing_stopped() {
            match task_handle.await {
                Ok(Err(e)) => e,
                Ok(Ok(())) => error,
                Err(_) => Error::TaskProcessingPanicked,
            }
        } else {
            error
        }
    }

    async fn sync(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
        config: Config,
        on_local_change: impl FnMut() + Send + 'static,
    ) -> Result<Synced, Error> {
        let mailbox_maildir = mail_dir.join(mailbox);
        let mailbox_statedir = state_dir.join(mailbox);

        let loaded = match MaildirRepository::load(&mailbox_maildir, &mailbox_statedir).await {
            Ok(maildir_repository) => {
                let initial_sync_complete = maildir_repository.is_initial_sync_complete().await?;
                Some((maildir_repository, initial_sync_complete))
            }
            Err(LoadError::Uninitialized) => None,
            Err(e) => return Err(e.into()),
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

    /// Detects remote deletions by comparing local with remote UIDs. Needed without QRESYNC.
    async fn remote_deletions(
        client: &mut SelectedClient,
        repository: &RepositoryHandle,
    ) -> Result<Option<SequenceSet>, Error> {
        let remote_uids = client.remote_uids().await?;
        let missing_uids = repository.uids_not_in(remote_uids).await?;

        Ok(SequenceSet::from_sorted(missing_uids).ok())
    }

    fn check_uid_validity(local: UidValidity, remote: UidValidity) -> Result<(), Error> {
        if local == remote {
            Ok(())
        } else {
            Err(Error::UidValidityMismatch { local, remote })
        }
    }

    async fn sync_existing(
        maildir_repository: MaildirRepository,
        client: AuthenticatedClient,
        mailbox: &str,
        config: Config,
        on_local_change: impl FnMut() + Send + 'static,
    ) -> Result<Synced, Error> {
        let (task_tx, task_rx) = mpsc::bounded_async(config.store_batch_size());
        let repository = RepositoryHandle::new(task_tx.clone());
        let (local_changes, selected) =
            futures::join!(maildir_repository.detect_changes(), async {
                let (uid_validity, highest_modseq) = futures::join!(
                    maildir_repository.uid_validity(),
                    maildir_repository.highest_modseq()
                );
                let (uid_validity, highest_modseq) = (uid_validity?, highest_modseq?);
                let selection = client
                    .resync_select(task_tx, mailbox, uid_validity, highest_modseq)
                    .await?;
                Ok::<_, Error>((uid_validity, highest_modseq, selection))
            });
        let (uid_validity, highest_modseq, selection) = selected?;
        let mut local_changes = local_changes?;
        let Selection {
            mut client,
            mut remote_changes,
            mailbox_data,
            ..
        } = selection;
        Self::check_uid_validity(uid_validity, mailbox_data.uid_validity())?;
        let local_watcher = maildir_repository.local_watcher();
        let task_handle =
            Self::setup_task_processing(maildir_repository, task_rx, config, on_local_change);

        let synced = async {
            let mut synced_modseq = mailbox_data.highest_modseq();
            if !client.supports_qresync() {
                // fetching all uids first seeds the sequence to uid map, so the fetch needs not fill it
                let deletions = Self::remote_deletions(&mut client, &repository).await?;
                let updates = client.fetch_changes_since(highest_modseq).await?;
                // updates got fetched after SELECT, so they may be newer than its HIGHESTMODSEQ
                if let Some(modseq) = updates.iter().map(RemoteMailMetadata::modseq).max() {
                    synced_modseq = synced_modseq.max(modseq);
                }
                remote_changes = RemoteChanges { updates, deletions };
            }
            let restored = Self::handle_conflicts(&remote_changes, &mut local_changes);
            if let Ok(restored) = SequenceSet::from_sorted(restored) {
                log::warn!("mails {restored} changed on server. Restoring locally deleted mails");
                // forgetting them makes applying the remote updates fetch them again, even if
                // their flags did not change
                repository.delete(restored).await?;
            }
            Self::handle_remote_changes(&mut client, &repository, remote_changes).await?;
            client.commit_highest_modseq(synced_modseq).await?;
            Self::handle_local_changes(
                &mut client,
                local_changes,
                highest_modseq,
                mailbox,
                &repository,
                config,
            )
            .await
        }
        .await;
        if let Err(e) = synced {
            return Err(Self::task_error(e, task_handle).await);
        }

        Ok(Synced {
            client,
            task_handle,
            repository,
            local_watcher,
        })
    }

    async fn handle_local_changes(
        client: &mut SelectedClient,
        local_changes: LocalChanges,
        highest_modseq: ModSeq,
        mailbox: &str,
        repository: &RepositoryHandle,
        config: Config,
    ) -> Result<(), Error> {
        let LocalChanges {
            updates,
            deletions,
            news,
            other_flags,
        } = local_changes;
        // storing them first lets following flag changes rename the files without searching them
        if !other_flags.is_empty() {
            repository.update_other_flags(other_flags).await?;
        }
        let mut news = news.into_iter();
        let mut batch = Self::read_store_batch(&mut news, repository, config).await?;
        while !batch.is_empty() {
            let (stored, next_batch) = futures::join!(
                client.store(mailbox, batch),
                Self::read_store_batch(&mut news, repository, config)
            );
            let synced = stored?
                .into_iter()
                .map(|(uid, metadata)| (metadata, uid))
                .collect();
            repository.add_synced(synced).await?;
            batch = next_batch?;
        }
        let updates = updates.build();
        let mut rejected = Vec::new();
        for (change, sequence_set) in updates.iter() {
            let rejected_uids = client
                .store_flag_change(highest_modseq, change, sequence_set)
                .await?;
            let accepted: Vec<Uid> = sequence_set
                .iter()
                .filter(|uid| !rejected_uids.contains(uid))
                .collect();
            if !accepted.is_empty() {
                repository.apply_flag_changes(accepted, change).await?;
            }
            rejected.extend(rejected_uids.into_iter().map(|uid| (uid, Some(change))));
        }
        if let Ok(set) = SequenceSet::try_from(&deletions) {
            let rejected_uids = client.delete(highest_modseq, &set).await?;
            rejected.extend(rejected_uids.into_iter().map(|uid| (uid, None)));
        }
        if !rejected.is_empty() {
            Self::retry_rejected_changes(client, repository, rejected).await?;
        }

        Ok(())
    }

    async fn read_store_batch(
        news: &mut impl Iterator<Item = NewLocalMailMetadata>,
        repository: &RepositoryHandle,
        config: Config,
    ) -> Result<Vec<LocalMail>, Error> {
        let mut batch = Vec::with_capacity(config.store_batch_size());
        let mut bytes = 0;
        while batch.len() < config.store_batch_size()
            && bytes < config.store_batch_max_bytes()
            && let Some(metadata) = news.next()
        {
            if let Some(mail) = repository.read_mail(metadata).await? {
                bytes += mail.content().len();
                batch.push(mail);
            }
        }

        Ok(batch)
    }

    async fn retry_rejected_changes(
        client: &mut SelectedClient,
        repository: &RepositoryHandle,
        rejected: Vec<(Uid, Option<FlagChange>)>,
    ) -> Result<(), Error> {
        let uids: Vec<Uid> = rejected.iter().map(|(uid, _)| *uid).collect();
        let set = SequenceSet::try_from(&uids).expect("rejected changes should not be empty");
        let remote: HashMap<Uid, RemoteMailMetadata> = client
            .fetch_metadata(&set)
            .await?
            .into_iter()
            .map(|metadata| (metadata.uid(), metadata))
            .collect();
        let local = repository.flags(uids).await?;
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
                        .await?
                        .is_empty(),
                    None => !client.delete(remote_mail.modseq(), &set).await?.is_empty(),
                }
            } else {
                true
            };
            match (change, still_rejected) {
                (Some(change), false) => repository.apply_flag_changes(vec![uid], change).await?,
                (Some(change), true) => {
                    log::warn!("mail {uid} changed on server. Discarding local flag change");
                    repository.apply_flag_changes(vec![uid], change).await?;
                    conflicts.push(remote_mail.clone());
                }
                (None, false) => {}
                (None, true) => {
                    log::warn!("mail {uid} changed on server. Restoring locally deleted mail");
                    repository.delete(set).await?;
                    conflicts.push(remote_mail.clone());
                }
            }
        }
        if !conflicts.is_empty() {
            let remote_changes = RemoteChanges {
                updates: conflicts,
                deletions: None,
            };
            Self::handle_remote_changes(client, repository, remote_changes).await?;
        }

        Ok(())
    }

    async fn handle_remote_changes(
        client: &mut SelectedClient,
        repository: &RepositoryHandle,
        remote_changes: RemoteChanges,
    ) -> Result<(), Error> {
        if let Some(set) = remote_changes.deletions {
            repository.delete(set).await?;
        }

        let missing = repository
            .apply_remote_updates(remote_changes.updates)
            .await?;
        if let Ok(sequence_set) = SequenceSet::try_from(&missing) {
            client.fetch_mail(&sequence_set).await?;
        }

        Ok(())
    }

    /// Discards local changes of mails changed remotely, as remote changes win. Returns the
    /// ascending UIDs of locally deleted mails, that got updated remotely and need restoring.
    fn handle_conflicts(
        remote_changes: &RemoteChanges,
        local_changes: &mut LocalChanges,
    ) -> Vec<Uid> {
        let mut remote_deletions: HashSet<Uid> = remote_changes
            .deletions
            .as_ref()
            .map_or_else(HashSet::new, |deletions| deletions.iter().collect());
        let mut remote_updates: HashSet<Uid> = remote_changes
            .updates
            .iter()
            .map(RemoteMailMetadata::uid)
            .collect();

        let mut restored = Vec::new();
        local_changes.deletions.retain(|deletion| {
            if remote_deletions.contains(deletion) {
                false
            } else if remote_updates.contains(deletion) {
                restored.push(*deletion);
                false
            } else {
                true
            }
        });
        for uid in remote_updates.drain() {
            local_changes.updates.remove(uid);
        }
        for uid in remote_deletions.drain() {
            local_changes.updates.remove(uid);
        }
        restored.sort_unstable();

        restored
    }

    async fn sync_initial(
        maildir_repository: Option<MaildirRepository>,
        client: AuthenticatedClient,
        mail_dir: &Path,
        state_dir: &Path,
        mailbox: &str,
        config: Config,
        on_local_change: impl FnMut() + Send + 'static,
    ) -> Result<Synced, Error> {
        let (task_tx, task_rx) = mpsc::bounded_async(config.store_batch_size());
        let repository = RepositoryHandle::new(task_tx.clone());
        let (selection, local_state) = futures::join!(
            client.select(task_tx, mailbox),
            OptionFuture::from(maildir_repository.as_ref().map(|repository| async {
                futures::join!(repository.uid_validity(), repository.uids())
            }))
        );
        let Selection {
            mut client,
            mailbox_data,
            ..
        } = selection?;

        let (maildir_repository, local_uids) = if let Some(maildir_repository) = maildir_repository
            && let Some((uid_validity, local_uids)) = local_state
        {
            Self::check_uid_validity(uid_validity?, mailbox_data.uid_validity())?;
            (maildir_repository, local_uids?)
        } else {
            (
                MaildirRepository::try_init(mailbox_data.uid_validity(), mail_dir, state_dir)
                    .await?,
                HashSet::new(),
            )
        };
        let local_watcher = maildir_repository.local_watcher();
        let task_handle =
            Self::setup_task_processing(maildir_repository, task_rx, config, on_local_change);

        let synced = async {
            if local_uids.is_empty() {
                client.fetch_all().await?;
            } else {
                let updates = client.fetch_all_metadata().await?;
                let remote_uids: HashSet<Uid> =
                    updates.iter().map(RemoteMailMetadata::uid).collect();
                let deletions: Vec<Uid> = local_uids.difference(&remote_uids).copied().collect();
                let remote_changes = RemoteChanges {
                    updates,
                    deletions: SequenceSet::try_from(&deletions).ok(),
                };
                Self::handle_remote_changes(&mut client, &repository, remote_changes).await?;
            }
            client
                .complete_initial_sync(mailbox_data.highest_modseq())
                .await?;

            Ok::<_, Error>(())
        }
        .await;
        if let Err(e) = synced {
            return Err(Self::task_error(e, task_handle).await);
        }

        Ok(Synced {
            client,
            task_handle,
            repository,
            local_watcher,
        })
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

    /// Merges queued deletions, e.g. from per-mail EXPUNGE responses without QRESYNC, to delete
    /// them in a single repository transaction
    fn collect_delete_batch(
        first: &SequenceSet,
        task_rx: &AsyncRx<Array<Task>>,
        config: Config,
    ) -> (Vec<Uid>, Option<Task>) {
        let mut batch: Vec<Uid> = first.iter().collect();
        while batch.len() < config.store_batch_size() {
            match task_rx.try_recv() {
                Ok(Task::Delete(sequence_set)) => batch.extend(sequence_set.iter()),
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
    ) -> Result<(), Error> {
        match task {
            Task::NewMail(_) => unreachable!("new mail should be handled in batches"),
            Task::Delete(sequence_set) => {
                maildir_repository.delete_all(sequence_set.iter()).await?;
                on_local_change();
            }
            Task::SetHighestModseq(modseq) => {
                log::debug!("Setting highest modseq to {modseq}");
                maildir_repository.update_highest_modseq(modseq).await?;
            }
            Task::CompleteInitialSync(modseq, done_tx) => {
                log::debug!("Completing initial sync with highest modseq {modseq}");
                maildir_repository.complete_initial_sync(modseq).await?;
                done_tx.send(());
            }
            Task::ApplyRemoteUpdates(updates, reply) => {
                let (missing, changed) = maildir_repository.apply_remote_updates(&updates).await?;
                if changed {
                    on_local_change();
                }
                reply.send(missing);
            }
            Task::AddSynced(synced) => {
                maildir_repository.add_synced_all(synced).await?;
            }
            Task::ApplyFlagChanges(uids, change) => {
                maildir_repository.apply_flag_changes(&uids, change).await?;
            }
            Task::UpdateOtherFlags(other_flags) => {
                maildir_repository
                    .update_other_flags_all(other_flags)
                    .await?;
            }
            Task::ReadMail(metadata, reply) => {
                reply.send(maildir_repository.read_mail(metadata).await?);
            }
            Task::Flags(uids, reply) => {
                reply.send(maildir_repository.flags(uids).await?);
            }
            Task::DetectChanges(reply) => {
                reply.send(maildir_repository.detect_changes().await?);
            }
            Task::HighestModseq(reply) => {
                reply.send(maildir_repository.highest_modseq().await?);
            }
            Task::UidsNotIn(uids, reply) => {
                reply.send(maildir_repository.uids_not_in(uids).await?);
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
                    Err(e) => return Err(e.into()),
                }
            }
        }

        Ok(())
    }

    fn setup_task_processing(
        maildir_repository: MaildirRepository,
        task_rx: AsyncRx<Array<Task>>,
        config: Config,
        mut on_local_change: impl FnMut() + Send + 'static,
    ) -> TaskHandle {
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
                let result = match task {
                    Task::NewMail(remote_mail) => {
                        let (batch, task) =
                            Self::collect_store_batch(remote_mail, &task_rx, config);
                        next_task = task;
                        let result = maildir_repository.store_batch(&batch).await;
                        if result.is_ok() {
                            on_local_change();
                        }
                        result.map_err(Error::from)
                    }
                    Task::Delete(sequence_set) => {
                        let (batch, task) =
                            Self::collect_delete_batch(&sequence_set, &task_rx, config);
                        next_task = task;
                        log::debug!("deleting {} mails", batch.len());
                        let result = maildir_repository.delete_all(batch.into_iter()).await;
                        if result.is_ok() {
                            on_local_change();
                        }
                        result.map_err(Error::from)
                    }
                    task => {
                        Self::process_task(&maildir_repository, task, &mut on_local_change).await
                    }
                };
                if let Err(e) = result {
                    log::error!("task processing failed: {e}");
                    return Err(e);
                }
            }
            log::debug!("Task channel closed. Stopping task processing");

            Ok(())
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

    fn delete(uids: &[u32]) -> Task {
        Task::Delete(assert_ok!(SequenceSet::from_sorted(
            uids.iter().map(|uid| assert_ok!(Uid::try_from(*uid)))
        )))
    }

    fn delete_batch(
        first: &[u32],
        task_rx: &AsyncRx<Array<Task>>,
        config: Config,
    ) -> (Vec<u32>, Option<Task>) {
        let Task::Delete(first) = delete(first) else {
            unreachable!()
        };
        let (batch, next) = Syncer::collect_delete_batch(&first, task_rx, config);

        (batch.into_iter().map(u32::from).collect(), next)
    }

    #[compio::test]
    async fn test_collect_delete_batch_merges_queued_deletions() {
        let (_task_tx, task_rx) = queue([delete(&[3]), delete(&[5, 6])]).await;

        let (batch, next) = delete_batch(&[1, 2], &task_rx, Config::default());

        assert_eq!(batch, vec![1, 2, 3, 5, 6]);
        assert!(next.is_none());
    }

    #[compio::test]
    async fn test_collect_delete_batch_respects_count_limit() {
        let (_task_tx, task_rx) = queue([delete(&[2]), delete(&[3])]).await;

        let (batch, next) = delete_batch(&[1], &task_rx, Config::new(2, 100));

        assert_eq!(batch, vec![1, 2]);
        assert!(next.is_none());
        assert_matches!(task_rx.try_recv(), Ok(Task::Delete(_)));
    }

    #[compio::test]
    async fn test_collect_delete_batch_ends_on_other_task() {
        let (_task_tx, task_rx) = queue([
            delete(&[2]),
            Task::SetHighestModseq(assert_ok!(ModSeq::try_from(9))),
            delete(&[3]),
        ])
        .await;

        let (batch, next) = delete_batch(&[1], &task_rx, Config::default());

        assert_eq!(batch, vec![1, 2]);
        assert_matches!(next, Some(Task::SetHighestModseq(_)));
    }

    #[test]
    fn test_handle_conflicts_restores_locally_deleted_mails_updated_remotely() {
        let uid = |uid: u32| assert_ok!(Uid::try_from(uid));
        let remote_changes = RemoteChanges {
            updates: [5, 2]
                .into_iter()
                .map(|updated| {
                    RemoteMailMetadata::new(
                        uid(updated),
                        Flag::Seen.into(),
                        assert_ok!(ModSeq::try_from(9)),
                    )
                })
                .collect(),
            deletions: Some(SequenceSet::from(uid(3))),
        };
        let mut local_changes = LocalChanges::new(
            vec![uid(5), uid(3), uid(4), uid(2)],
            Vec::new(),
            crate::maildir::LocalFlagChangesBuilder::default(),
        );

        let restored = Syncer::handle_conflicts(&remote_changes, &mut local_changes);

        assert_eq!(restored, vec![uid(2), uid(5)]);
        assert_eq!(local_changes.deletions, vec![uid(4)]);
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
