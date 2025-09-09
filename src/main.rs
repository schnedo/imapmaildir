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
use log::{debug, trace};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_native_tls::{TlsConnector, TlsStream, native_tls};
use tokio_util::codec::Framed;

use crate::config::Config;
use crate::imap::{ImapCodec, TagGenerator};
use crate::nuke::nuke;

#[derive(Parser, Debug)]
#[command(version, about, long_about=None)]
struct Args {
    /// `rm -rf` the configured account (WARNING: includes all mails)
    #[arg(long)]
    nuke: bool,
}

#[bitflags]
#[repr(u8)]
#[derive(Copy, Clone, Debug)]
enum Capability {
    Condstore,
    Enable,
    Idle,
    Imap4rev1,
    QResync,
}

fn update_from(
    capabilities: &mut BitFlags<Capability>,
    new_capabilities: &[imap_proto::Capability],
) {
    for capability in new_capabilities {
        match capability {
            imap_proto::Capability::Imap4rev1 => {
                capabilities.insert(Capability::Imap4rev1);
            }
            imap_proto::Capability::Auth(cow) => {
                trace!("unhandled auth capabilty {cow}");
            }
            imap_proto::Capability::Atom(cow) => match cow.as_ref() {
                "CONDSTORE" => {
                    capabilities.insert(Capability::Condstore);
                }
                "ENABLE" => {
                    capabilities.insert(Capability::Enable);
                }
                "IDLE" => {
                    capabilities.insert(Capability::Idle);
                }
                "QRESYNC" => {
                    capabilities.insert(Capability::QResync);
                }
                _ => {
                    trace!("unhandled capability {cow}");
                }
            },
        }
    }
    trace!("updated capabilities to {capabilities:?}");
}

type Callbacks = Arc<Mutex<HashMap<String, oneshot::Sender<Result<(), ()>>>>>;
struct Client {
    commands_in_flight: Callbacks,
    capabilities: BitFlags<Capability>,
    tag_generator: TagGenerator,
    tx: mpsc::Sender<(String, String)>,
}

impl Client {
    #[expect(clippy::too_many_lines)]
    async fn start(host: &str, port: u16) -> Self {
        debug!("Connecting to server");
        let tls = native_tls::TlsConnector::new().expect("native tls should be available");
        let tls = TlsConnector::from(tls);
        let stream =
            (TcpStream::connect((host, port)).await).expect("connection to server should succeed");
        let stream = (tls.connect(host, stream).await).expect("upgrading to tls should succeed");

        let mut stream = Framed::new(stream, ImapCodec::default());
        let (tx, mut rx) = mpsc::channel::<(String, String)>(2);

        let commands_in_flight: Callbacks = Arc::new(Mutex::new(HashMap::new()));
        let mut capabilities = BitFlags::default();
        let in_flight = commands_in_flight.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((tag, command)) = rx.recv() => {
                        trace!("sending {tag:?}");
                        let request = Request(
                            Cow::Borrowed(tag.as_bytes()),
                            Cow::Borrowed(command.as_bytes()),
                        );
                        stream
                            .send(&request)
                            .await
                            .expect("sending command should succeed");
                    }
                    Some(response) = stream.next() => {
                        let response = response.expect("response should be receivable");
                        trace!("{:?}", response.parsed());
                        match response.parsed() {
                            imap_proto::Response::Capabilities(items)
                            | imap_proto::Response::Data {
                                status: imap_proto::Status::Ok,
                                code: Some(imap_proto::ResponseCode::Capabilities(items)),
                                information: _,
                            } => {
                                update_from(&mut capabilities, items);
                            }
                            imap_proto::Response::Done {
                                tag,
                                status,
                                code,
                                information,
                            } => {
                                trace!("ended {tag:?} {status:?} {information:?}");
                                match status {
                                    imap_proto::Status::Ok => {
                                        if let Some(code) = code {
                                            match code {
                                                imap_proto::ResponseCode::Alert => todo!(),
                                                imap_proto::ResponseCode::BadCharset(cows) => todo!(),
                                                imap_proto::ResponseCode::Capabilities(items) => {
                                                    update_from(&mut capabilities, items);
                                                },
                                                imap_proto::ResponseCode::HighestModSeq(_) => todo!(),
                                                imap_proto::ResponseCode::Parse => todo!(),
                                                imap_proto::ResponseCode::PermanentFlags(cows) => todo!(),
                                                imap_proto::ResponseCode::ReadOnly => todo!(),
                                                imap_proto::ResponseCode::ReadWrite => todo!(),
                                                imap_proto::ResponseCode::TryCreate => todo!(),
                                                imap_proto::ResponseCode::UidNext(_) => todo!(),
                                                imap_proto::ResponseCode::UidValidity(_) => todo!(),
                                                imap_proto::ResponseCode::Unseen(_) => todo!(),
                                                imap_proto::ResponseCode::AppendUid(_, uid_set_members) => todo!(),
                                                imap_proto::ResponseCode::CopyUid(_, uid_set_members, uid_set_members1) => todo!(),
                                                imap_proto::ResponseCode::UidNotSticky => todo!(),
                                                imap_proto::ResponseCode::MetadataLongEntries(_) => todo!(),
                                                imap_proto::ResponseCode::MetadataMaxSize(_) => todo!(),
                                                imap_proto::ResponseCode::MetadataTooMany => todo!(),
                                                imap_proto::ResponseCode::MetadataNoPrivate => todo!(),
                                                _ => todo!(),
                                            }
                                        }
                                        if let Some(cb) = in_flight.lock().expect("locking commands_in_flight for response should succeed").remove(&tag.0) {
                                            cb.send(Ok(())).expect("sending response out of io task should succeed");
                                        }
                                    },
                                    imap_proto::Status::No => todo!(),
                                    imap_proto::Status::Bad => todo!(),
                                    imap_proto::Status::PreAuth => todo!(),
                                    imap_proto::Status::Bye => todo!(),
                                }
                            },
                            imap_proto::Response::Continue { code, information } => todo!(),
                            imap_proto::Response::Data {
                                status,
                                code,
                                information,
                            } => todo!(),
                            imap_proto::Response::Expunge(_) => todo!(),
                            imap_proto::Response::Vanished { earlier, uids } => todo!(),
                            imap_proto::Response::Fetch(_, attribute_values) => todo!(),
                            imap_proto::Response::MailboxData(mailbox_datum) => todo!(),
                            imap_proto::Response::Quota(quota) => todo!(),
                            imap_proto::Response::QuotaRoot(quota_root) => todo!(),
                            imap_proto::Response::Id(hash_map) => todo!(),
                            imap_proto::Response::Acl(acl) => todo!(),
                            imap_proto::Response::ListRights(list_rights) => todo!(),
                            imap_proto::Response::MyRights(my_rights) => todo!(),
                            _ => todo!(),
                        }
                    }
                }
            }
        });

        Self {
            commands_in_flight,
            capabilities: BitFlags::default(),
            tag_generator: TagGenerator::default(),
            tx,
        }
    }

    async fn send(&mut self, command: &str) -> Result<Result<(), ()>, oneshot::Canceled> {
        let (a, b) = oneshot::channel();
        let tag = self.tag_generator.next();
        self.commands_in_flight
            .lock()
            .expect("sender should be able to acquire lock")
            .insert(tag.clone(), a);
        self.tx
            .send((tag, command.to_string()))
            .await
            .expect("sending request to io task should succeed");
        b.into_future().await
    }

    async fn login(&mut self, username: &str, password: &str) {
        debug!("LOGIN <user> <password>");
        self.send(&format!("LOGIN {username} {password}"))
            .await
            .expect("communication to io task should not have been canceled")
            .expect("login should succeed");
    }
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
        let mut client = Client::start(host, port).await;
        let username = config.user();
        let password = config.password();
        client.login(username, &password).await;

        Ok(())
    }
}
