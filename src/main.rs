use core::str;

use clap::Parser;
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
use crate::state::State;
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

        let (mut selection, maildir_repository) =
            if let Ok(state) = State::load(state_dir, account, mailbox).await {
                let uid_validity = state.uid_validity().await;
                let highest_modseq = state.highest_modseq().await;
                let mut selection = client
                    .qresync_select(mailbox, uid_validity, highest_modseq)
                    .await;
                assert_eq!(uid_validity, selection.mailbox_data.uid_validity());
                state
                    .set_highest_modseq(selection.mailbox_data.highest_modseq())
                    .await;
                let maildir_repository = MaildirRepository::load(account, mailbox, mail_dir, state);

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

                (selection, maildir_repository)
            } else {
                let mut selection = client.select(mailbox).await;

                let state = State::init(
                    state_dir,
                    account,
                    mailbox,
                    selection.mailbox_data.uid_validity(),
                )
                .await
                .expect("state should be creatable");
                state
                    .set_highest_modseq(selection.mailbox_data.highest_modseq())
                    .await;
                let maildir_repository = MaildirRepository::init(account, mailbox, mail_dir, state);
                selection.client.fetch_all().await;

                (selection, maildir_repository)
            };

        let recieve_task = tokio::task::spawn(async move {
            while let Some(mail) = selection.mail_rx.recv().await {
                maildir_repository.store(&mail).await;
            }
        });

        recieve_task.await?;

        Ok(())
    }
}
