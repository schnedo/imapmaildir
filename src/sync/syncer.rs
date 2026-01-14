use crate::{
    imap::{RemoteChanges, RemoteMailMetadata, SelectedClient, Selection},
    maildir::LocalChanges,
    repository::{MailboxMetadata, SequenceSet, SequenceSetBuilder, Uid},
    sync::task::Task,
};
use std::{collections::HashSet, path::Path};

use log::info;
use tokio::sync::mpsc;

use crate::{imap::AuthenticatedClient, maildir::MaildirRepository};

pub struct Syncer {}

impl Syncer {
    pub async fn sync(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
    ) {
        let mailbox_maildir = mail_dir.join(mailbox);
        let mailbox_statedir = state_dir.join(mailbox);
        let (task_tx, task_rx) = mpsc::channel(32);

        match MaildirRepository::load(&mailbox_maildir, &mailbox_statedir, task_rx) {
            Ok(maildir_repository) => {
                Self::sync_existing(&maildir_repository, client, mailbox, task_tx).await;
            }
            Err(task_rx) => {
                info!("no existing maildir found. Running inital sync");
                Self::sync_new(
                    client,
                    &mailbox_maildir,
                    &mailbox_statedir,
                    mailbox,
                    task_rx,
                    task_tx,
                )
                .await;
            }
        }
    }

    async fn sync_existing(
        maildir_repository: &MaildirRepository,
        client: AuthenticatedClient,
        mailbox: &str,
        task_tx: mpsc::Sender<Task>,
    ) {
        let uid_validity = maildir_repository.uid_validity().await;
        let highest_modseq = maildir_repository.highest_modseq().await;

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

        let mut local_changes = maildir_repository.detect_changes().await;
        Self::handle_conflicts(&remote_changes, &mut local_changes);
        Self::handle_remote_changes(
            &mut client,
            maildir_repository,
            &remote_changes,
            &mailbox_data,
        )
        .await;
        Self::handle_local_changes(&mut client, local_changes, mailbox, maildir_repository).await;
        task_tx
            .send(Task::Shutdown())
            .await
            .expect("sending shutdown task should succeed");
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
        while let Some(info) = mailinfos.recv().await {
            let (mut metadata, uid) = info.unpack();
            maildir_repository.add_synced(&mut metadata, uid).await;
        }
        let updates = updates.build();
        for (flag, sequence_set) in updates.removed_flags() {
            client.remove_flag(highest_modseq, flag, sequence_set).await;
        }
        for (flag, sequence_set) in updates.additional_flags() {
            client.add_flag(highest_modseq, flag, sequence_set).await;
        }
        if let Ok(set) = SequenceSet::try_from(&deletions) {
            client.delete(highest_modseq, &set).await;
        }
    }

    async fn handle_remote_changes(
        client: &mut SelectedClient,
        maildir_repository: &MaildirRepository,
        remote_changes: &RemoteChanges,
        mailbox_data: &MailboxMetadata,
    ) {
        if let Some(set) = &remote_changes.deletions {
            for uid in set.iter() {
                maildir_repository.delete(uid).await;
            }
        }

        let mut refetch_mails = SequenceSetBuilder::default();
        for update in &remote_changes.updates {
            if maildir_repository.update_flags(update).await.is_err() {
                refetch_mails.add(update.uid());
            }
        }
        if let Ok(sequence_set) = refetch_mails.build() {
            client.fetch_mail(&sequence_set).await;
        }
        maildir_repository
            .set_highest_modseq(mailbox_data.highest_modseq())
            .await;
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
        task_rx: mpsc::Receiver<Task>,
        task_tx: mpsc::Sender<Task>,
    ) {
        let mut selection = client.select(task_tx.clone(), mailbox).await;

        MaildirRepository::init(
            selection.mailbox_data.uid_validity(),
            selection.mailbox_data.highest_modseq(),
            mail_dir,
            state_dir,
            task_rx,
        );
        selection.client.fetch_all().await;
        task_tx
            .send(Task::Shutdown())
            .await
            .expect("sending shutdown task should succeed");
    }
}
