use crate::{
    imap::{Mailbox, ModSeq, RemoteChanges, RemoteMail, SelectedClient, Selection},
    maildir::LocalChanges,
};
use std::{collections::HashSet, path::Path};

use log::debug;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    imap::{AuthenticatedClient, SequenceSet, SequenceSetBuilder},
    maildir::MaildirRepository,
};

pub struct Syncer {}

impl Syncer {
    pub async fn sync(
        account: &str,
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        client: AuthenticatedClient,
    ) -> JoinHandle<()> {
        let (mail_tx, mut mail_rx) = mpsc::channel(32);
        let (highest_modseq_tx, highest_modseq_rx) = mpsc::channel(32);
        let (deleted_tx, mut deleted_rx) = mpsc::channel(32);
        let maildir_repository = if let Some(maildir_repository) =
            MaildirRepository::load(account, mailbox, mail_dir, state_dir)
        {
            Self::sync_existing(
                &maildir_repository,
                client,
                mail_tx,
                highest_modseq_tx,
                deleted_tx,
                mailbox,
            )
            .await;

            maildir_repository
        } else {
            Self::sync_new(
                client,
                account,
                mail_dir,
                state_dir,
                mail_tx,
                highest_modseq_tx,
                deleted_tx,
                mailbox,
            )
            .await
        };
        maildir_repository.handle_highest_modseq(highest_modseq_rx);

        tokio::spawn(async move {
            debug!("Listening to incoming mail...");
            loop {
                tokio::select! {
                    Some(mail) = mail_rx.recv() => {
                        maildir_repository.store(&mail);
                    }
                    Some(set) = deleted_rx.recv() => {
                    for uid in set.iter() {
                        maildir_repository.delete(uid);
                    }
                    }
                }
            }
        })
    }

    async fn sync_existing(
        maildir_repository: &MaildirRepository,
        client: AuthenticatedClient,
        mail_tx: mpsc::Sender<RemoteMail>,
        highest_modseq_tx: mpsc::Sender<ModSeq>,
        deleted_tx: mpsc::Sender<SequenceSet>,
        mailbox: &str,
    ) {
        let uid_validity = maildir_repository.uid_validity();
        let highest_modseq = maildir_repository.highest_modseq();

        let Selection {
            mut client,
            remote_changes,
            mailbox_data,
            ..
        } = client
            .qresync_select(
                mail_tx,
                highest_modseq_tx,
                deleted_tx,
                mailbox,
                uid_validity,
                highest_modseq,
            )
            .await;
        assert_eq!(
            uid_validity,
            mailbox_data.uid_validity(),
            "remote uid validity should be the same as local"
        );

        let mut local_changes = maildir_repository.detect_changes();
        Self::handle_conflicts(&remote_changes, &mut local_changes);

        Self::handle_remote_changes(
            &mut client,
            maildir_repository,
            &remote_changes,
            &mailbox_data,
        )
        .await;

        Self::handle_local_changes(&mut client, local_changes, mailbox, maildir_repository).await;
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
            maildir_repository.add_synced(&mut metadata, uid);
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
        mailbox_data: &Mailbox,
    ) {
        if let Some(set) = &remote_changes.deletions {
            for uid in set.iter() {
                maildir_repository.delete(uid);
            }
        }

        let mut sequence_set = SequenceSetBuilder::default();
        for update in &remote_changes.updates {
            if maildir_repository.update_flags(update).is_err() {
                sequence_set.add(update.uid());
            }
        }
        if let Ok(sequence_set) = sequence_set.build() {
            client.fetch_mail(&sequence_set).await;
        }
        maildir_repository.set_highest_modseq(mailbox_data.highest_modseq());
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
        account: &str,
        mail_dir: &Path,
        state_dir: &Path,
        mail_tx: mpsc::Sender<RemoteMail>,
        highest_modseq_tx: mpsc::Sender<ModSeq>,
        deleted_tx: mpsc::Sender<SequenceSet>,
        mailbox: &str,
    ) -> MaildirRepository {
        let mut selection = client
            .select(mail_tx, highest_modseq_tx, deleted_tx, mailbox)
            .await;

        let maildir_repository = MaildirRepository::init(
            account,
            mailbox,
            selection.mailbox_data.uid_validity(),
            mail_dir,
            state_dir,
        );
        selection.client.fetch_all().await;
        maildir_repository.set_highest_modseq(selection.mailbox_data.highest_modseq());

        maildir_repository
    }
}
