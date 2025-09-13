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
use crate::imap::NotAuthenticatedClient;
use crate::maildir::MaildirRepository;
use crate::nuke::nuke;
use crate::state::State;
use crate::sync::Repository;

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

        let client = NotAuthenticatedClient::start(host, port).await;
        let client = client.login(username, password).await;

        if let Ok(state) = State::load(state_dir, account, mailbox).await {
            todo!("handle already initialized account");
        } else {
            let state = State::init(state_dir, account, mailbox)
                .await
                .expect("state should be creatable");
            let (mut client, mut mail_rx) = client.select(state.clone(), mailbox).await;
            let mailbox = mailbox.clone();
            let state_dir = state_dir.clone();
            let account = account.to_string();
            let mail_dir = mail_dir.clone();
            let writing_task = tokio::task::spawn(async move {
                let maildir_repository =
                    MaildirRepository::init(&account, &mailbox, &mail_dir, &state);
                while let Some(mail) = mail_rx.recv().await {
                    maildir_repository.store(&mail).await;
                }
            });
            yield_now().await;
            client.init().await;
            writing_task.await?;
        }

        Ok(())
    }
}
