use crate::{
    imap::{IdleStopReason, RemoteChanges, RemoteMailMetadata, SelectedClient, Selection},
    maildir::LocalChanges,
    repository::{MailboxMetadata, SequenceSet, SequenceSetBuilder, Uid},
    sync::task::Task,
};
use std::{collections::HashSet, path::Path, time::Duration};

use log::{debug, info, trace};
use tokio::sync::mpsc;

use crate::{imap::AuthenticatedClient, maildir::MaildirRepository};

pub struct Syncer {}

impl Syncer {
    #[expect(clippy::missing_panics_doc)]
    pub async fn sync_once(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
    ) {
        let ((_, task_tx), _) = Self::sync(mailbox, mail_dir, state_dir, client).await;
        task_tx
            .send(Task::Shutdown)
            .await
            .expect("sending shutdown task should succeed");
    }

    #[expect(clippy::missing_panics_doc)]
    pub async fn sync_continuously(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
        idle_timeout: Duration,
    ) -> ! {
        let ((mut client, _), maildir_repository) =
            Self::sync(mailbox, mail_dir, state_dir, client).await;
        let repo = maildir_repository.clone();
        let mut local_change_rx = repo.watch().await;

        let stop_tx = client.idle_stop_tx();
        tokio::spawn(async move {
            while let Some(changes) = local_change_rx.recv().await {
                trace!("detected local changes {changes:?}");
                stop_tx
                    .send(IdleStopReason::Local { changes })
                    .await
                    .expect("stop idle channel should still be open");
            }
        });

        loop {
            match client.idle(idle_timeout).await {
                IdleStopReason::Remote => {
                    trace!("handling remote idle changes");
                    let current_highest_modseq = maildir_repository
                        .highest_modseq()
                        .expect("getting highest modseq should succeed");
                    client.fetch_since(current_highest_modseq).await;
                }
                IdleStopReason::Local { changes } => {
                    trace!("handling local idle changes");
                    Self::handle_local_changes(&mut client, changes, mailbox, &maildir_repository)
                        .await;
                }
                IdleStopReason::Timeout => {}
            }
        }
    }

    async fn sync(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
    ) -> ((SelectedClient, mpsc::Sender<Task>), MaildirRepository) {
        let mailbox_maildir = mail_dir.join(mailbox);
        let mailbox_statedir = state_dir.join(mailbox);

        if let Ok(maildir_repository) = MaildirRepository::load(&mailbox_maildir, &mailbox_statedir)
        {
            (
                (Self::sync_existing(&maildir_repository, client, mailbox).await),
                maildir_repository,
            )
        } else {
            info!("no existing maildir found. Running inital sync");

            Self::sync_new(client, &mailbox_maildir, &mailbox_statedir, mailbox).await
        }
    }

    async fn sync_existing(
        maildir_repository: &MaildirRepository,
        client: AuthenticatedClient,
        mailbox: &str,
    ) -> (SelectedClient, mpsc::Sender<Task>) {
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
        let (task_tx, task_rx) = mpsc::channel(32);
        Self::setup_task_processing(maildir_repository.clone(), task_rx);

        let Selection {
            mut client,
            remote_changes,
            mailbox_data,
            ..
        } = client
            .qresync_select(task_tx.clone(), mailbox, uid_validity, highest_modseq)
            .await;
        assert_eq!(
            uid_validity,
            mailbox_data.uid_validity(),
            "remote uid validity should be the same as local"
        );

        Self::handle_conflicts(&remote_changes, &mut local_changes);
        Self::handle_remote_changes(
            &mut client,
            maildir_repository,
            remote_changes,
            &mailbox_data,
        )
        .await;
        Self::handle_local_changes(&mut client, local_changes, mailbox, maildir_repository).await;
        (client, task_tx)
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
        let mut mailinfos = client.store(mailbox, news.into_iter()).await;
        // todo: parallelize these
        while let Some((uid, metadata)) = mailinfos.recv().await {
            maildir_repository
                .add_synced(metadata, uid)
                .await
                .expect("writing back synced uid should succeed");
        }
        let updates = updates.build();
        for (flag, sequence_set) in updates.removed_flags() {
            client.remove_flag(highest_modseq, flag, sequence_set).await;
            for uid in sequence_set.iter() {
                maildir_repository
                    .remove_flag(uid, flag)
                    .await
                    .expect("removing flag from maildir_repository should succeed");
            }
        }
        for (flag, sequence_set) in updates.additional_flags() {
            client.add_flag(highest_modseq, flag, sequence_set).await;
            for uid in sequence_set.iter() {
                maildir_repository
                    .add_flag(uid, flag)
                    .await
                    .expect("adding flag to maildir_repository should succeed");
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
        mailbox_data: &MailboxMetadata,
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
        maildir_repository
            .set_highest_modseq(mailbox_data.highest_modseq())
            .expect("setting highest_modseq should succeed");
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

    async fn sync_new(
        client: AuthenticatedClient,
        mail_dir: &Path,
        state_dir: &Path,
        mailbox: &str,
    ) -> ((SelectedClient, mpsc::Sender<Task>), MaildirRepository) {
        let (task_tx, task_rx) = mpsc::channel(32);
        let mut selection = client.select(task_tx.clone(), mailbox).await;

        let maildir_repository =
            MaildirRepository::try_init(&selection.mailbox_data, mail_dir, state_dir)
                .expect("initializing maildir repository should succeed");
        Self::setup_task_processing(maildir_repository.clone(), task_rx);
        selection.client.fetch_all().await;

        ((selection.client, task_tx), maildir_repository)
    }

    fn setup_task_processing(
        maildir_repository: MaildirRepository,
        mut task_rx: mpsc::Receiver<Task>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            debug!("Listening to incoming mail...");
            while let Some(task) = task_rx.recv().await {
                match task {
                    Task::NewMail(remote_mail) => {
                        maildir_repository
                            .store(&remote_mail)
                            .await
                            .expect("storing new mail should succeed");
                    }
                    Task::Delete(sequence_set) => {
                        for uid in sequence_set.iter() {
                            maildir_repository
                                .delete(uid)
                                .await
                                .expect("deleting mails should succeed");
                        }
                    }
                    Task::Shutdown => {
                        task_rx.close();
                    }
                    Task::UpdateModseq(uid, mod_seq) => {
                        debug!("Setting modseq of mail {uid} to {mod_seq}");
                        maildir_repository
                            .update_highest_modseq(mod_seq)
                            .expect("setting highest_modseq should succeed");
                    }
                    Task::UpdateFlags(remote_mail_metadata) => {
                        debug!(
                            "Setting flags of mail {} to {}",
                            remote_mail_metadata.uid(),
                            remote_mail_metadata.flags()
                        );
                        maildir_repository
                            .update_flags(&remote_mail_metadata)
                            .await
                            .expect("updating flags should succeed");
                    }
                }
            }
        })
    }
}
