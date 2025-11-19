use crate::sync::repository::MailMetadata;
use std::path::Path;

use log::debug;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    imap::{AuthenticatedClient, SequenceSetBuilder},
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
        let maildir_repository = if let Some(maildir_repository) =
            MaildirRepository::load(account, mailbox, mail_dir, state_dir).await
        {
            let uid_validity = maildir_repository.uid_validity().await;
            let highest_modseq = maildir_repository.highest_modseq().await;

            let mut selection = client
                .qresync_select(
                    mail_tx,
                    highest_modseq_tx,
                    mailbox,
                    uid_validity,
                    highest_modseq,
                )
                .await;
            assert_eq!(uid_validity, selection.mailbox_data.uid_validity());

            maildir_repository.detect_changes().await;
            // todo: handle conflicts

            let mut sequence_set = SequenceSetBuilder::new();
            for update in &selection.mail_updates {
                if maildir_repository.update(update).await.is_err() {
                    sequence_set.add(update.uid().expect("uid should exist here").into());
                }
            }
            if let Ok(sequence_set) = sequence_set.build() {
                selection.client.fetch_mail(&sequence_set).await;
            }
            maildir_repository
                .set_highest_modseq(selection.mailbox_data.highest_modseq())
                .await;

            maildir_repository
        } else {
            let mut selection = client.select(mail_tx, highest_modseq_tx, mailbox).await;

            let maildir_repository = MaildirRepository::init(
                account,
                mailbox,
                selection.mailbox_data.uid_validity(),
                mail_dir,
                state_dir,
            )
            .await;
            selection.client.fetch_all().await;
            maildir_repository
                .set_highest_modseq(selection.mailbox_data.highest_modseq())
                .await;

            maildir_repository
        };
        maildir_repository.handle_highest_modseq(highest_modseq_rx);

        tokio::spawn(async move {
            debug!("Listening to incoming mail...");
            while let Some(mail) = mail_rx.recv().await {
                maildir_repository.store(&mail).await;
            }
        })
    }
}
