#![expect(dead_code, unused_variables, unused_imports)]

use core::str;
use std::borrow::Cow;
use std::collections::HashMap;
use std::default;
use std::sync::{Arc, Mutex};

use clap::Parser;
mod config;
mod imap;
mod logging;
mod maildir;
mod nuke;
mod state;
mod sync;

use anyhow::Result;
use enumflags2::{BitFlag, BitFlags, bitflags};
use futures::channel::oneshot;
use futures::stream::SplitSink;
use futures::{Sink, SinkExt, StreamExt};
use futures_util::sink::Send;
use imap_proto::{Request, Response};
use log::{debug, trace, warn};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::{JoinHandle, yield_now};
use tokio_native_tls::{TlsConnector, TlsStream, native_tls};
use tokio_util::codec::Framed;

use crate::config::Config;
use crate::imap::{NotAuthenticatedClient, SequenceSetBuilder};
use crate::maildir::MaildirRepository;
use crate::nuke::nuke;
use crate::state::State;
use crate::sync::{MailMetadata, Repository};

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

        let (mut selection, maildir_repository) = if let Ok(state) =
            State::load(state_dir, account, mailbox).await
        {
            let uid_validity = state.uid_validity().await;
            let highest_modseq = state.highest_modseq().await;
            let mut selection = client
                .qresync_select(mailbox, uid_validity, highest_modseq)
                .await;
            assert_eq!(uid_validity, selection.client.uid_validity());
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

            (selection, maildir_repository)
        } else {
            let mut selection = client.select(mailbox).await;

            let state = State::init(state_dir, account, mailbox, selection.client.uid_validity())
                .await
                .expect("state should be creatable");
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
