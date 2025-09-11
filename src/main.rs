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
use crate::imap::{ImapCodec, ResponseData, TagGenerator};
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

#[derive(Default)]
struct ImapState {
    capabilities: Mutex<BitFlags<Capability>>,
}

impl ImapState {
    fn handle_untagged_response(&self, response: &Response<'_>) {
        trace!("handling untagged response {response:?}");
        match response {
            imap_proto::Response::Capabilities(items)
            | imap_proto::Response::Data {
                status: imap_proto::Status::Ok,
                code: Some(imap_proto::ResponseCode::Capabilities(items)),
                information: _,
            } => {
                self.update_capabilities(items);
            }
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
            _ => warn!("ignoring unknown untagged response: {response:?}"),
        }
    }

    fn update_capabilities(&self, capabilities: &[imap_proto::Capability]) {
        let mut caps = self
            .capabilities
            .lock()
            .expect("capabilities should be lockable");
        for capability in capabilities {
            match capability {
                imap_proto::Capability::Imap4rev1 => {
                    caps.insert(Capability::Imap4rev1);
                }
                imap_proto::Capability::Auth(cow) => {
                    trace!("unhandled auth capabilty {cow}");
                }
                imap_proto::Capability::Atom(cow) => match cow.as_ref() {
                    "CONDSTORE" => {
                        caps.insert(Capability::Condstore);
                    }
                    "ENABLE" => {
                        caps.insert(Capability::Enable);
                    }
                    "IDLE" => {
                        caps.insert(Capability::Idle);
                    }
                    "QRESYNC" => {
                        caps.insert(Capability::QResync);
                    }
                    _ => {
                        trace!("unhandled capability {cow}");
                    }
                },
            }
        }
        trace!("updated capabilities to {caps:?}");
    }
}

struct Client {
    connection: Connection,
    state: Arc<ImapState>,
}

impl Client {
    async fn start(host: &str, port: u16) -> Self {
        let (untagged_response_sender, mut untagged_response_receiver) = mpsc::channel(32);
        let connection = Connection::start(host, port, untagged_response_sender).await;
        let this = Self {
            connection,
            state: Arc::new(ImapState::default()),
        };
        let state = this.state.clone();

        tokio::spawn(async move {
            while let Some(response) = untagged_response_receiver.recv().await {
                state.handle_untagged_response(response.parsed());
            }
        });

        this
    }

    async fn login(&mut self, username: &str, password: &str) {
        debug!("LOGIN <user> <password>");
        let response = self
            .connection
            .send(&format!("LOGIN {username} {password}"))
            .await
            .expect("login should succeed");
        if let Some(imap_proto::ResponseCode::Capabilities(items)) =
            response.unsafe_get_tagged_response_code()
        {
            self.state.update_capabilities(items);
        } else {
            self.connection
                .send("CAPABILITY")
                .await
                .expect("capabilities should succeed");
        }
    }
}

#[derive(Debug)]
enum TaggedResponseError {
    No { information: Option<String> },
    Bad { information: Option<String> },
}
type SendReturnValue = Result<ResponseData, TaggedResponseError>;
type Callbacks = Arc<Mutex<Option<oneshot::Sender<SendReturnValue>>>>;
struct Connection {
    tag_generator: TagGenerator,
    outbound_tx: mpsc::Sender<(String, String)>,
    inbound_rx: mpsc::Receiver<SendReturnValue>,
}

impl Connection {
    async fn start(host: &str, port: u16, untagged_response_sender: Sender<ResponseData>) -> Self {
        debug!("Connecting to server");
        let tls = native_tls::TlsConnector::new().expect("native tls should be available");
        let tls = TlsConnector::from(tls);
        let stream =
            (TcpStream::connect((host, port)).await).expect("connection to server should succeed");
        let stream = (tls.connect(host, stream).await).expect("upgrading to tls should succeed");

        let mut stream = Framed::new(stream, ImapCodec::default());
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<(String, String)>(2);
        let (inbound_tx, inbound_rx) = mpsc::channel::<SendReturnValue>(2);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((tag, command)) = outbound_rx.recv() => {
                        trace!("{tag:?}: sending");
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
                            Response::Done { tag, status, code, information } => {
                                trace!("{tag:?} {status:?} {information:?}");
                                match status {
                                    imap_proto::Status::Ok => {
                                        inbound_tx.send(Ok(response))
                                            .await
                                            .expect("sending response out of network task should succeed");
                                    }
                                    imap_proto::Status::No => {
                                        inbound_tx.send(Err(TaggedResponseError::No{information: information.as_ref().map(ToString::to_string)}))
                                            .await
                                            .expect("sending response out of network task should succeed");
                                    },
                                    imap_proto::Status::Bad => {
                                        inbound_tx.send(Err(TaggedResponseError::Bad{information: information.as_ref().map(ToString::to_string)}))
                                            .await
                                            .expect("sending response out of network task should succeed");
                                    },
                                    imap_proto::Status::PreAuth => panic!("receiving tagged PreAuth response is not possible per specification"),
                                    imap_proto::Status::Bye => panic!("receiving tagged Bye response is not possible per specification"),
                                }
                            } ,
                            Response::Continue { code, information } => {
                                trace!("+ {information:?}");
                                inbound_tx.send(Ok(response))
                                    .await
                                    .expect("sending response out of network task should succeed");
                            },
                            _ => {
                                untagged_response_sender.send(response).await.expect("untagged response channel should still be open");
                            },
                        }
                    }
                }
            }
        });

        Self {
            tag_generator: TagGenerator::default(),
            outbound_tx,
            inbound_rx,
        }
    }

    async fn send(&mut self, command: &str) -> SendReturnValue {
        let tag = self.tag_generator.next();
        self.outbound_tx
            .send((tag, command.to_string()))
            .await
            .expect("sending request to io task should succeed");
        self.inbound_rx
            .recv()
            .await
            .expect("channel to network task should still be open")
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
        let username = config.user();
        let password = &config.password();
        let mut client = Client::start(host, port).await;
        client.login(username, password).await;

        Ok(())
    }
}
