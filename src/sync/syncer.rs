use crate::{
    imap::{RemoteChanges, SelectedClient, Selection},
    maildir::LocalChanges,
    repository::{MailboxMetadata, SequenceSet, SequenceSetBuilder},
    sync::task::Task,
};
use std::{collections::HashSet, path::Path};

use log::debug;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{imap::AuthenticatedClient, maildir::MaildirRepository};

pub struct Syncer {}

impl Syncer {
    pub async fn sync(
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
    ) -> JoinHandle<()> {
        if let Some(maildir_repository) = MaildirRepository::load(mail_dir, state_dir) {
            Self::sync_existing(&maildir_repository, client, mailbox).await
        } else {
            Self::sync_new(client, mail_dir, state_dir, mailbox).await
        }
    }

    fn setup_task_processing(
        mut task_rx: mpsc::Receiver<Task>,
        maildir_repository: MaildirRepository,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            debug!("Listening to incoming mail...");
            while let Some(task) = task_rx.recv().await {
                match task {
                    Task::NewMail(remote_mail) => {
                        maildir_repository.store(&remote_mail).await;
                    }
                    Task::Delete(sequence_set) => {
                        for uid in sequence_set.iter() {
                            maildir_repository.delete(uid).await;
                        }
                    }
                    Task::HighestModSeq(mod_seq) => {
                        maildir_repository.set_highest_modseq(mod_seq).await;
                    }
                    Task::Shutdown() => {
                        task_rx.close();
                    }
                }
            }
        })
    }

    async fn sync_existing(
        maildir_repository: &MaildirRepository,
        client: AuthenticatedClient,
        mailbox: &str,
    ) -> JoinHandle<()> {
        let uid_validity = maildir_repository.uid_validity().await;
        let highest_modseq = maildir_repository.highest_modseq().await;

        let (task_tx, task_rx) = mpsc::channel(32);
        let handle = Self::setup_task_processing(task_rx, maildir_repository.clone());

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

        handle
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

        let mut sequence_set = SequenceSetBuilder::default();
        for update in &remote_changes.updates {
            if maildir_repository.update_flags(update).await.is_err() {
                sequence_set.add(update.uid());
            }
        }
        if let Ok(sequence_set) = sequence_set.build() {
            client.fetch_mail(&sequence_set).await;
        }
        maildir_repository
            .set_highest_modseq(mailbox_data.highest_modseq())
            .await;
    }

    // todo: add configurable conflict strategy; right now: remote wins
    fn handle_conflicts(remote_changes: &RemoteChanges, local_changes: &mut LocalChanges) {
        let mut remote_deletions = HashSet::new();
        if let Some(deletions) = &remote_changes.deletions {
            for deletion in deletions.iter() {
                remote_deletions.insert(deletion);
            }
        }
        let mut remote_updates = HashSet::new();
        for update in &remote_changes.updates {
            remote_updates.insert(update.uid());
        }

        local_changes
            .deletions
            .retain(|deletion| !remote_updates.contains(deletion));
        for uid in remote_updates.drain() {
            local_changes.updates.remove(uid);
        }
    }

    async fn sync_new(
        client: AuthenticatedClient,
        mail_dir: &Path,
        state_dir: &Path,
        mailbox: &str,
    ) -> JoinHandle<()> {
        let (task_tx, task_rx) = mpsc::channel(32);
        let mut selection = client.select(task_tx.clone(), mailbox).await;

        let maildir_repository = MaildirRepository::init(
            selection.mailbox_data.uid_validity(),
            selection.mailbox_data.highest_modseq(),
            mail_dir,
            state_dir,
        );
        let handle = Self::setup_task_processing(task_rx, maildir_repository);
        selection.client.fetch_all().await;
        task_tx
            .send(Task::Shutdown())
            .await
            .expect("sending shutdown task should succeed");

        handle
    }
}
