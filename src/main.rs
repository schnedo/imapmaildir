use core::str;

use clap::Parser;
use log::debug;
use tokio::sync::mpsc;
mod config;
mod imap;
mod logging;
mod maildir;
mod nuke;
mod state;
mod sync;

use crate::config::Config;
use crate::imap::{NotAuthenticatedClient, SequenceSetBuilder};
use crate::maildir::MaildirRepository;
use crate::nuke::nuke;
use crate::sync::MailMetadata;
use anyhow::Result;

#[derive(Parser, Debug)]
#[command(version, about, long_about=None)]
struct Args {
    /// `rm -rf` the configured account (WARNING: includes all mails)
    #[arg(long)]
    nuke: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging::init();

    let config = Config::load_from_file();

    if args.nuke {
        nuke(&config);
        Ok(())
    } else {
        let host: &str = config.host();
        let port = config.port();
        let username = config.user();
        let password = &config.password();
        let mailbox = config
            .mailboxes()
            .first()
            .expect("there should be a mailbox configured");
        let state_dir = config.statedir();
        let account = config.account();
        let mail_dir = config.maildir();

        let client = NotAuthenticatedClient::connect(host, port).await;
        let client = client.login(username, password).await;
        let (mail_tx, mut mail_rx) = mpsc::channel(32);

        let maildir_repository = if let Some(maildir_repository) =
            MaildirRepository::load(account, mailbox, mail_dir, state_dir).await
        {
            let uid_validity = maildir_repository.uid_validity().await;
            let highest_modseq = maildir_repository.highest_modseq().await;

            let mut selection = client
                .qresync_select(mail_tx, mailbox, uid_validity, highest_modseq)
                .await;
            assert_eq!(uid_validity, selection.mailbox_data.uid_validity());
            maildir_repository
                .set_highest_modseq(selection.mailbox_data.highest_modseq())
                .await;

            let mut sequence_set = SequenceSetBuilder::new();
            for update in &selection.mail_updates {
                if maildir_repository.update(update).await.is_err() {
                    sequence_set.add(update.uid().expect("uid should exist here").into());
                }
            }
            if let Ok(sequence_set) = sequence_set.build() {
                selection.client.fetch_mail(&sequence_set).await;
            }

            maildir_repository.detect_changes().await;

            maildir_repository
        } else {
            let mut selection = client.select(mail_tx, mailbox).await;

            let maildir_repository = MaildirRepository::init(
                account,
                mailbox,
                selection.mailbox_data.uid_validity(),
                mail_dir,
                state_dir,
            )
            .await;
            maildir_repository
                .set_highest_modseq(selection.mailbox_data.highest_modseq())
                .await;
            selection.client.fetch_all().await;

            maildir_repository
        };

        debug!("Listening to incoming mails...");
        while let Some(mail) = mail_rx.recv().await {
            maildir_repository.store(&mail).await;
        }

        Ok(())
    }
}
