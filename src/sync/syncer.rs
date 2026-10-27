use crate::{
    imap::{
        IdleStopReason, RemoteChanges, RemoteMail, RemoteMailMetadata, SelectedClient, Selection,
    },
    maildir::{
        ChangeRecorder, FlagChange, LocalChanges, LocalEvent, LocalMail, LocalWatcher, MailChange,
        NewLocalMailMetadata,
    },
    repository::{ModSeq, SequenceSet, Uid, UidValidity},
    sync::{ByteBudget, Config, Error, RepositoryHandle, Shutdown, task::Task},
};
use std::{
    collections::{HashMap, HashSet},
    ops::ControlFlow,
    path::Path,
    pin::pin,
    time::Duration,
};

use compio::runtime::JoinHandle;
use crossfire::{
    AsyncRx,
    mpsc::{self, Array},
};
use futures::{
    FutureExt as _,
    future::{self, Either, OptionFuture},
};

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
    /// Stops early, if `shutdown` gets triggered during an initial sync, which resumes later.
    ///
    /// # Errors
    /// If syncing with the server or the local maildir fails
    pub async fn sync_once(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
        config: Config,
        changes: Option<ChangeRecorder>,
        shutdown: &Shutdown,
    ) -> Result<(), Error> {
        let synced = Self::sync(
            mailbox, mail_dir, state_dir, client, config, changes, shutdown,
        )
        .await;
        let Synced {
            client,
            task_handle,
            repository,
            ..
        } = match synced {
            Ok(synced) => synced,
            Err(Error::ShutDown) => return Ok(()),
            Err(e) => return Err(e),
        };

        Self::finish(client, repository, task_handle, shutdown, config).await
    }

    /// Syncs changes until `shutdown` gets triggered. Then finishes handling the current changes.
    ///
    /// # Errors
    /// If the server does not support IDLE or syncing with the server or the local maildir fails
    #[expect(clippy::too_many_arguments)]
    pub async fn sync_continuously(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
        idle_timeout: Duration,
        config: Config,
        changes: Option<ChangeRecorder>,
        shutdown: &Shutdown,
    ) -> Result<(), Error> {
        client.require_idle()?;
        let synced = Self::sync(
            mailbox, mail_dir, state_dir, client, config, changes, shutdown,
        )
        .await;
        let Synced {
            mut client,
            task_handle,
            repository,
            local_watcher,
        } = match synced {
            Ok(synced) => synced,
            Err(Error::ShutDown) => return Ok(()),
            Err(e) => return Err(e),
        };
        let stop_tx = client.take_idle_stop_tx();

        log::info!("listening for mail changes...");
        let local_change_rx = match local_watcher.watch().await {
            Ok(local_change_rx) => local_change_rx,
            Err(e) => {
                return Err(Self::shut_down(e.into(), client, repository, task_handle).await);
            }
        };
        let forwarder_shutdown = shutdown.clone();
        compio::runtime::spawn(async move {
            let mut stopped = pin!(forwarder_shutdown.wait().fuse());
            loop {
                // shutting down must not wait for a maildir, that keeps changing
                let event = futures::select_biased! {
                    () = stopped => break,
                    event = local_change_rx.recv().fuse() => event,
                };
                let Ok(event) = event else {
                    return;
                };
                if stop_tx.send(Self::idle_stop_reason(event)).await.is_err() {
                    return;
                }
            }
            // changes detected before shutting down still get synced
            if let Ok(event) = local_change_rx.try_recv()
                && stop_tx.send(Self::idle_stop_reason(event)).await.is_err()
            {
                return;
            }
            let _ = stop_tx.send(IdleStopReason::Shutdown).await;
        })
        .detach();
        let session = async {
            // local changes during the sync, e.g. a long initial one, happened before watching
            let changes = repository.detect_changes().await?;
            Self::handle_local(&mut client, &repository, changes, mailbox, config).await?;
            while Self::handle_idle(&mut client, &repository, mailbox, idle_timeout, config)
                .await?
                .is_continue()
            {}

            Ok(())
        };
        let result = Self::unless_abandoned(session, shutdown, config).await;

        match result {
            Ok(()) => {
                log::info!("shutting down");
                Self::finish(client, repository, task_handle, shutdown, config).await
            }
            Err(e) => match Self::shut_down(e, client, repository, task_handle).await {
                Error::ShutDown => Ok(()),
                e => Err(e),
            },
        }
    }

    fn idle_stop_reason(event: LocalEvent) -> IdleStopReason {
        log::trace!("detected local changes {event:?}");
        match event {
            LocalEvent::Changes(changes) => IdleStopReason::Local { changes },
            LocalEvent::Overflow => IdleStopReason::Rescan,
            LocalEvent::Error(e) => IdleStopReason::LocalError(e),
        }
    }

    /// Runs `future` to completion, unless the configured shutdown timeout passed after `shutdown`
    /// got triggered. Then fails with [`Error::ShutDown`]. Mails being uploaded then may get
    /// uploaded again by the next session.
    async fn unless_abandoned<T>(
        future: impl Future<Output = Result<T, Error>>,
        shutdown: &Shutdown,
        config: Config,
    ) -> Result<T, Error> {
        let abandoned = shutdown.timed_out(config.shutdown_timeout());
        match future::select(pin!(future), pin!(abandoned)).await {
            Either::Left((result, _)) => result,
            Either::Right(_) => {
                log::warn!("server did not respond while shutting down. Abandoning connection");
                Err(Error::ShutDown)
            }
        }
    }

    /// Logs out and lets task processing store the remaining changes. Gives up logging out once
    /// the configured shutdown timeout passed after `shutdown` got triggered, as the changes are
    /// safe.
    async fn finish(
        client: SelectedClient,
        repository: RepositoryHandle,
        task_handle: TaskHandle,
        shutdown: &Shutdown,
        config: Config,
    ) -> Result<(), Error> {
        drop(repository);
        let logout = async {
            futures::select_biased! {
                logged_out = client.logout().fuse() => logged_out,
                () = shutdown.timed_out(config.shutdown_timeout()).fuse() => {
                    log::warn!("server did not respond to logout");
                    Ok(())
                }
            }
        };
        let (logout_result, task_result) = futures::join!(logout, task_handle);
        task_result.map_err(|_| Error::TaskProcessingPanicked)??;
        logout_result?;

        Ok(())
    }

    /// Lets task processing store the remote changes received before `error` and close the
    /// state, so that a new session can load it right away
    async fn shut_down(
        error: Error,
        client: SelectedClient,
        repository: RepositoryHandle,
        task_handle: TaskHandle,
    ) -> Error {
        // closes the task channel
        drop(client);
        drop(repository);
        match task_handle.await {
            // local errors are permanent, so they must not hide behind a transient remote one
            Ok(Err(e)) => e,
            Ok(Ok(())) => error,
            Err(_) => Error::TaskProcessingPanicked,
        }
    }

    async fn handle_idle(
        client: &mut SelectedClient,
        repository: &RepositoryHandle,
        mailbox: &str,
        idle_timeout: Duration,
        config: Config,
    ) -> Result<ControlFlow<()>, Error> {
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
            }
            IdleStopReason::LocalError(e) => return Err(e.into()),
            IdleStopReason::Timeout => {
                log::info!(
                    "idle timed out after {} seconds. Reissuing...",
                    idle_timeout.as_secs()
                );
            }
            IdleStopReason::Shutdown => return Ok(ControlFlow::Break(())),
        }

        Ok(ControlFlow::Continue(()))
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

    async fn sync(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
        config: Config,
        changes: Option<ChangeRecorder>,
        shutdown: &Shutdown,
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

        // boxed, as the sync futures are large, but run only once per session
        match loaded {
            Some((maildir_repository, true)) => {
                Box::pin(Self::sync_existing(
                    maildir_repository,
                    client,
                    mailbox,
                    config,
                    changes,
                    shutdown,
                ))
                .await
            }
            Some((maildir_repository, false)) => {
                log::info!("resuming interrupted initial sync");
                Box::pin(Self::sync_initial(
                    Some(maildir_repository),
                    client,
                    &mailbox_maildir,
                    &mailbox_statedir,
                    mailbox,
                    config,
                    changes,
                    shutdown,
                ))
                .await
            }
            None => {
                log::info!("no existing maildir found. Running inital sync");
                Box::pin(Self::sync_initial(
                    None,
                    client,
                    &mailbox_maildir,
                    &mailbox_statedir,
                    mailbox,
                    config,
                    changes,
                    shutdown,
                ))
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

    /// Stops with [`Error::ShutDown`], once the server does not respond while shutting down
    async fn sync_existing(
        mut maildir_repository: MaildirRepository,
        client: AuthenticatedClient,
        mailbox: &str,
        config: Config,
        changes: Option<ChangeRecorder>,
        shutdown: &Shutdown,
    ) -> Result<Synced, Error> {
        // detecting local changes may already recover interrupted downloads
        maildir_repository.record_changes(changes);
        let (task_tx, task_rx) = mpsc::bounded_async(config.store_batch_size());
        let budget = Self::store_budget(config);
        let repository = RepositoryHandle::new(task_tx.clone());
        let selecting = async {
            let (uid_validity, highest_modseq) = futures::join!(
                maildir_repository.uid_validity(),
                maildir_repository.highest_modseq()
            );
            let (uid_validity, highest_modseq) = (uid_validity?, highest_modseq?);
            let selection = client
                .resync_select(
                    task_tx,
                    budget.clone(),
                    mailbox,
                    uid_validity,
                    highest_modseq,
                )
                .await?;
            Ok::<_, Error>((uid_validity, highest_modseq, selection))
        };
        let (local_changes, selected) = futures::join!(
            maildir_repository.detect_changes(),
            Self::unless_abandoned(selecting, shutdown, config)
        );
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
            Self::setup_task_processing(maildir_repository, task_rx, budget, config, None);

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
        };
        let synced = Self::unless_abandoned(synced, shutdown, config).await;
        if let Err(e) = synced {
            return Err(Self::shut_down(e, client, repository, task_handle).await);
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
        let mut batch = Self::open_store_batch(&mut news, repository, config).await?;
        while !batch.is_empty() {
            let (stored, next_batch) = futures::join!(
                client.store(mailbox, batch),
                Self::open_store_batch(&mut news, repository, config)
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

    async fn open_store_batch(
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
            if let Some(mail) = repository.open_mail(metadata).await? {
                bytes += mail.size();
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
        // retries are batched per change, as rejections may affect many mails at once
        let mut flag_retries: HashMap<FlagChange, HashMap<Uid, ModSeq>> = HashMap::new();
        let mut delete_retries: HashMap<Uid, ModSeq> = HashMap::new();
        let mut flag_changes: HashMap<FlagChange, Vec<Uid>> = HashMap::new();
        let mut still_rejected: Vec<(Uid, Option<FlagChange>)> = Vec::new();
        for (uid, change) in rejected {
            let Some(remote_mail) = remote.get(&uid) else {
                continue;
            };
            if let Some(change) = change {
                flag_changes.entry(change).or_default().push(uid);
            }
            if local.get(&uid) == Some(&remote_mail.flags()) {
                let retries = match change {
                    Some(change) => flag_retries.entry(change).or_default(),
                    None => &mut delete_retries,
                };
                retries.insert(uid, remote_mail.modseq());
            } else {
                still_rejected.push((uid, change));
            }
        }
        for (change, pending) in flag_retries {
            let rejected = client.store_flag_change_since(pending, change).await?;
            still_rejected.extend(rejected.into_iter().map(|uid| (uid, Some(change))));
        }
        if !delete_retries.is_empty() {
            let rejected = client.delete_since(delete_retries).await?;
            still_rejected.extend(rejected.into_iter().map(|uid| (uid, None)));
        }
        // accepted and discarded flag changes alike make the state match the local files, so
        // that applying the conflicting remote updates renames them
        for (change, uids) in flag_changes {
            repository.apply_flag_changes(uids, change).await?;
        }
        let mut conflicts = HashMap::new();
        let mut restored = Vec::new();
        for (uid, change) in still_rejected {
            if change.is_some() {
                log::warn!("mail {uid} changed on server. Discarding local flag change");
            } else {
                log::warn!("mail {uid} changed on server. Restoring locally deleted mail");
                restored.push(uid);
            }
            conflicts.insert(uid, remote[&uid].clone());
        }
        if let Ok(restored) = SequenceSet::try_from(&restored) {
            repository.delete(restored).await?;
        }
        let conflicts: Vec<RemoteMailMetadata> = conflicts.into_values().collect();
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

    /// Stops downloading with [`Error::ShutDown`], once `shutdown` gets triggered. The next
    /// session resumes it.
    #[expect(clippy::too_many_arguments)]
    async fn sync_initial(
        maildir_repository: Option<MaildirRepository>,
        client: AuthenticatedClient,
        mail_dir: &Path,
        state_dir: &Path,
        mailbox: &str,
        config: Config,
        changes: Option<ChangeRecorder>,
        shutdown: &Shutdown,
    ) -> Result<Synced, Error> {
        let (task_tx, task_rx) = mpsc::bounded_async(config.store_batch_size());
        let budget = Self::store_budget(config);
        let repository = RepositoryHandle::new(task_tx.clone());
        let (selection, local_state) = futures::join!(
            Self::unless_abandoned(
                async { Ok(client.select(task_tx, budget.clone(), mailbox).await?) },
                shutdown,
                config,
            ),
            OptionFuture::from(maildir_repository.as_ref().map(|repository| async {
                futures::join!(repository.uid_validity(), repository.uids())
            }))
        );
        let Selection {
            mut client,
            mailbox_data,
            ..
        } = selection?;

        let (mut maildir_repository, local_uids) = if let Some(maildir_repository) =
            maildir_repository
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
        // reporting every mail of a first download would flood the hooks
        let deferred_changes = if local_uids.is_empty() {
            changes
        } else {
            maildir_repository.record_changes(changes);
            None
        };
        let task_handle = Self::setup_task_processing(
            maildir_repository,
            task_rx,
            budget,
            config,
            deferred_changes,
        );

        let downloaded = async {
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
        };
        let synced = match future::select(pin!(downloaded), pin!(shutdown.wait())).await {
            Either::Left((downloaded, _)) => downloaded,
            Either::Right(_) => {
                log::info!("interrupting initial sync to shut down");
                Err(Error::ShutDown)
            }
        };
        if let Err(e) = synced {
            return Err(Self::shut_down(e, client, repository, task_handle).await);
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

    /// Merges queued flag updates, e.g. from unsolicited FETCH responses while idling, to apply
    /// them in a single repository transaction. Later updates of a mail replace earlier ones.
    fn collect_update_batch(
        first: RemoteMailMetadata,
        task_rx: &AsyncRx<Array<Task>>,
        config: Config,
    ) -> (Vec<RemoteMailMetadata>, Option<Task>) {
        let mut batch: HashMap<Uid, RemoteMailMetadata> = HashMap::new();
        batch.insert(first.uid(), first);
        while batch.len() < config.store_batch_size() {
            match task_rx.try_recv() {
                Ok(Task::UpdateFlags(metadata)) => {
                    batch.insert(metadata.uid(), metadata);
                }
                Ok(task) => return (batch.into_values().collect(), Some(task)),
                Err(_) => break,
            }
        }

        (batch.into_values().collect(), None)
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

    async fn process_task(maildir_repository: &MaildirRepository, task: Task) -> Result<(), Error> {
        match task {
            Task::NewMail(_) => unreachable!("new mail should be handled in batches"),
            Task::Delete(sequence_set) => {
                maildir_repository.delete_all(sequence_set.iter()).await?;
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
                let missing = maildir_repository.apply_remote_updates(&updates).await?;
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
            Task::OpenMail(metadata, reply) => {
                reply.send(maildir_repository.open_mail(metadata).await?);
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
            Task::UpdateFlags(_) => unreachable!("flag updates should be handled in batches"),
        }

        Ok(())
    }

    /// Reports mails stored by an initial sync, that ended before completing, as overflow
    fn report_unrecorded(deferred_changes: Option<ChangeRecorder>, stored_unrecorded: bool) {
        if stored_unrecorded && let Some(changes) = deferred_changes {
            changes.record(MailChange::Overflow);
        }
    }

    /// Allows downloading one batch while storing another, so that neither waits for the other
    fn store_budget(config: Config) -> ByteBudget {
        ByteBudget::new(config.store_batch_max_bytes().saturating_mul(2))
    }

    fn setup_task_processing(
        mut maildir_repository: MaildirRepository,
        task_rx: AsyncRx<Array<Task>>,
        budget: ByteBudget,
        config: Config,
        deferred_changes: Option<ChangeRecorder>,
    ) -> TaskHandle {
        compio::runtime::spawn(async move {
            // downloads must not wait for budget nobody releases anymore, even on panics
            let _close_budget = CloseOnDrop(budget.clone());
            log::debug!("Listening to incoming mail...");
            let result = Self::process_tasks(
                &mut maildir_repository,
                &task_rx,
                &budget,
                config,
                deferred_changes,
            )
            .await;
            if let Err(e) = maildir_repository.close().await {
                log::warn!("cannot close state: {e}");
            }

            result
        })
    }

    /// Starts recording changes with `deferred_changes` after completing the initial sync. Then
    /// reports an overflow instead of every mail stored until then.
    async fn process_tasks(
        maildir_repository: &mut MaildirRepository,
        task_rx: &AsyncRx<Array<Task>>,
        budget: &ByteBudget,
        config: Config,
        mut deferred_changes: Option<ChangeRecorder>,
    ) -> Result<(), Error> {
        let mut stored_unrecorded = false;
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
                    let (batch, task) = Self::collect_store_batch(remote_mail, task_rx, config);
                    next_task = task;
                    let result = maildir_repository.store_batch(&batch).await;
                    budget.release(
                        batch
                            .iter()
                            .map(|mail| budget.cost(mail.content().len()))
                            .sum(),
                    );
                    if result.is_ok() && deferred_changes.is_some() {
                        stored_unrecorded = true;
                    }
                    result.map_err(Error::from)
                }
                Task::UpdateFlags(metadata) => {
                    let (batch, task) = Self::collect_update_batch(metadata, task_rx, config);
                    next_task = task;
                    log::debug!("updating flags of {} mails", batch.len());
                    match maildir_repository.apply_remote_updates(&batch).await {
                        Ok(missing) => {
                            for uid in missing {
                                log::warn!("ignoring flag update of locally missing mail {uid}");
                            }
                            Ok(())
                        }
                        Err(e) => Err(e.into()),
                    }
                }
                Task::Delete(sequence_set) => {
                    let (batch, task) = Self::collect_delete_batch(&sequence_set, task_rx, config);
                    next_task = task;
                    log::debug!("deleting {} mails", batch.len());
                    maildir_repository
                        .delete_all(batch.into_iter())
                        .await
                        .map_err(Error::from)
                }
                task @ Task::CompleteInitialSync(..) => {
                    let result = Self::process_task(maildir_repository, task).await;
                    if result.is_ok()
                        && let Some(changes) = deferred_changes.take()
                    {
                        if stored_unrecorded {
                            changes.record(MailChange::Overflow);
                        }
                        maildir_repository.record_changes(Some(changes));
                    }
                    result
                }
                task => Self::process_task(maildir_repository, task).await,
            };
            if let Err(e) = result {
                log::error!("task processing failed: {e}");
                Self::report_unrecorded(deferred_changes, stored_unrecorded);
                return Err(e);
            }
        }
        log::debug!("Task channel closed. Stopping task processing");
        Self::report_unrecorded(deferred_changes, stored_unrecorded);

        Ok(())
    }
}

struct CloseOnDrop(ByteBudget);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use assertables::*;
    use crossfire::MAsyncTx;
    use tempfile::TempDir;

    use crate::{
        imap::{RemoteContent, RemoteMailMetadata},
        maildir::ChangeSink,
        repository::{Flag, ModSeq, Uid},
    };

    use super::*;

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<MailChange>>);

    impl ChangeSink for RecordingSink {
        fn record(&self, change: MailChange) {
            assert_ok!(self.0.lock()).push(change);
        }
    }

    /// Processes `tasks` of an initial sync into an empty maildir, whose changes get recorded
    /// only after completing it
    async fn process_initial_tasks(tasks: impl IntoIterator<Item = Task>) -> Vec<MailChange> {
        let (mail_dir, state_dir) = (assert_ok!(TempDir::new()), assert_ok!(TempDir::new()));
        let mut repository = assert_ok!(
            MaildirRepository::try_init(
                assert_ok!(UidValidity::try_from(1)),
                mail_dir.path(),
                state_dir.path()
            )
            .await
        );
        let sink = Arc::new(RecordingSink::default());
        let (task_tx, task_rx) = queue(tasks).await;
        drop(task_tx);

        assert_ok!(
            Syncer::process_tasks(
                &mut repository,
                &task_rx,
                &ByteBudget::new(1024),
                Config::default(),
                Some(ChangeRecorder::new(vec![sink.clone()])),
            )
            .await
        );
        assert_ok!(repository.close().await);

        std::mem::take(&mut *assert_ok!(sink.0.lock()))
    }

    #[compio::test]
    async fn test_initial_sync_ending_before_completion_reports_overflow() {
        let changes = process_initial_tasks([Task::NewMail(mail(1, 1))]).await;

        assert_eq!(changes, [MailChange::Overflow]);
    }

    #[compio::test]
    async fn test_initial_sync_without_mails_reports_nothing() {
        let changes = process_initial_tasks([]).await;

        assert_is_empty!(changes);
    }

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

        let (batch, next) =
            Syncer::collect_store_batch(mail(1, 1), &task_rx, Config::new(2, 100, Duration::ZERO));

        assert_eq!(uids(&batch), vec![1, 2]);
        assert!(next.is_none());
        assert_matches!(task_rx.try_recv(), Ok(Task::NewMail(_)));
    }

    #[compio::test]
    async fn test_collect_store_batch_respects_byte_limit() {
        let (_task_tx, task_rx) =
            queue([Task::NewMail(mail(2, 6)), Task::NewMail(mail(3, 1))]).await;

        let (batch, _) =
            Syncer::collect_store_batch(mail(1, 5), &task_rx, Config::new(10, 10, Duration::ZERO));

        assert_eq!(uids(&batch), vec![1, 2]);
    }

    #[compio::test]
    async fn test_collect_store_batch_takes_single_oversized_mail() {
        let (_task_tx, task_rx) = queue([Task::NewMail(mail(2, 1))]).await;

        let (batch, _) =
            Syncer::collect_store_batch(mail(1, 20), &task_rx, Config::new(10, 10, Duration::ZERO));

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

        let (batch, next) = delete_batch(&[1], &task_rx, Config::new(2, 100, Duration::ZERO));

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
