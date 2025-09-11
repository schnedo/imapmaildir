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
use tokio::task::JoinHandle;
use tokio_native_tls::{TlsConnector, TlsStream, native_tls};
use tokio_util::codec::Framed;

use crate::config::Config;
use crate::imap::NotAuthenticatedClient;
use crate::nuke::nuke;

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
        let mut client = NotAuthenticatedClient::start(host, port).await;
        client.login(username, password).await;

        Ok(())
    }
}
