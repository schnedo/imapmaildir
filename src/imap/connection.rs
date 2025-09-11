use std::borrow::Cow;

use futures::{SinkExt, StreamExt};
use log::{debug, trace};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_native_tls::{TlsConnector, native_tls};
use tokio_util::codec::Framed;

use crate::imap::{
    codec::{ImapCodec, ResponseData},
    tag_generator::TagGenerator,
};

#[derive(Debug)]
pub enum TaggedResponseError {
    No { information: Option<String> },
    Bad { information: Option<String> },
}
pub type SendReturnValue = Result<ResponseData, TaggedResponseError>;
pub struct Connection {
    tag_generator: TagGenerator,
    outbound_tx: mpsc::Sender<(String, String)>,
    inbound_rx: mpsc::Receiver<SendReturnValue>,
}

impl Connection {
    pub async fn start(
        host: &str,
        port: u16,
        untagged_response_sender: mpsc::Sender<ResponseData>,
    ) -> Self {
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
                        let request = imap_proto::Request(
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
                            imap_proto::Response::Done { tag, status, code, information } => {
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
                            imap_proto::Response::Continue { code, information } => {
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

    pub async fn send(&mut self, command: &str) -> SendReturnValue {
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
