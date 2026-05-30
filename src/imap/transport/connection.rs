use std::{borrow::Cow, fs, io, path::PathBuf};

use futures::{SinkExt, StreamExt};
use log::{debug, trace};
use thiserror::Error;
use tokio::{net::TcpStream, sync::mpsc};
use tokio_native_tls::{
    TlsConnector,
    native_tls::{self, Certificate},
};
use tokio_util::codec::Framed;

use crate::{
    config,
    imap::transport::{
        codec::{ImapCodec, ResponseData},
        tag_generator::TagGenerator,
    },
};

#[derive(Debug)]
pub enum TaggedResponseError {
    No {},
    Bad {},
}
pub type SendReturnValue = Result<ResponseData, TaggedResponseError>;
#[derive(Debug)]
pub struct Connection {
    tag_generator: TagGenerator,
    // todo: use impl Iterator<u8> instead of Vec<u8>
    outbound_tx: mpsc::Sender<(String, Vec<u8>)>,
    inbound_rx: mpsc::Receiver<SendReturnValue>,
}

impl Connection {
    pub async fn start(
        connection_config: &config::Connection,
        untagged_response_sender: mpsc::Sender<ResponseData>,
    ) -> Result<Self, Error> {
        debug!("Connecting to server");
        let mut tls = native_tls::TlsConnector::builder();
        if let Some(cert_file) = connection_config.server_certificate_file() {
            let cert =
                fs::read(cert_file).map_err(|e| Error::TlsError(TlsError::CertfileReadError(e)))?;
            tls.add_root_certificate(Certificate::from_pem(&cert).map_err(|_| {
                Error::TlsError(TlsError::CertfileFormatInvalid(cert_file.clone()))
            })?);
        }
        let tls = tls
            .build()
            .map_err(|_| Error::TlsError(TlsError::NativeTls))?;
        let tls = TlsConnector::from(tls);
        let host = connection_config.host().as_str();
        let port = connection_config.port();
        let stream =
            (TcpStream::connect((host, port)).await).map_err(|cause| Error::Connection {
                host: host.to_string(),
                port,
                cause,
            })?;
        let stream = (tls.connect(host, stream).await)
            .map_err(|e| Error::TlsError(TlsError::Validation(e)))?;

        let mut stream = Framed::new(stream, ImapCodec::default());
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<(String, Vec<u8>)>(30);
        let (inbound_tx, inbound_rx) = mpsc::channel::<SendReturnValue>(30);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((tag, command)) = outbound_rx.recv() => {
                        trace!("{tag:?}: sending");
                        let request = imap_proto::Request(
                            Cow::Borrowed(tag.as_bytes()),
                            Cow::Borrowed(&command),
                        );
                        stream
                            .send(&request)
                            .await
                            .expect("sending command should succeed");
                    }
                    Some(response) = stream.next() => {
                        let response = response.expect("response should be receivable");
                        match response.parsed() {
                            imap_proto::Response::Done { tag, status, code, information } => {
                                trace!("{tag:?} {status:?} {code:?}");
                                if let Some(information) = information {
                                    debug!("Done response information: {information}");
                                }
                                match status {
                                    imap_proto::Status::Ok => {
                                        inbound_tx.send(Ok(response))
                                            .await
                                            .expect("sending response out of network task should succeed");
                                    }
                                    imap_proto::Status::No => {
                                        inbound_tx.send(Err(TaggedResponseError::No{}))
                                            .await
                                            .expect("sending response out of network task should succeed");
                                    },
                                    imap_proto::Status::Bad => {
                                        inbound_tx.send(Err(TaggedResponseError::Bad{}))
                                            .await
                                            .expect("sending response out of network task should succeed");
                                    },
                                    imap_proto::Status::PreAuth => unreachable!("receiving tagged PreAuth response is not possible per specification"),
                                    imap_proto::Status::Bye => unreachable!("receiving tagged Bye response is not possible per specification"),
                                }
                            } ,
                            imap_proto::Response::Continue { code, information } => {
                                trace!("+ {code:?} {information:?}");
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

        Ok(Self {
            tag_generator: TagGenerator::default(),
            outbound_tx,
            inbound_rx,
        })
    }

    pub async fn send(&mut self, command: Vec<u8>) -> SendReturnValue {
        let tag = self.tag_generator.next();
        self.do_send(tag, command).await
    }

    pub async fn send_continuation(&mut self, data: Vec<u8>) -> SendReturnValue {
        self.do_send(String::new(), data).await
    }

    async fn do_send(&mut self, tag: String, data: Vec<u8>) -> SendReturnValue {
        self.outbound_tx
            .send((tag, data))
            .await
            .expect("sending request to io task should succeed");
        self.inbound_rx
            .recv()
            .await
            .expect("channel to network task should still be open")
    }
}

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("Cannot read server certificate file: {0}")]
    CertfileReadError(io::Error),
    #[error("Server certificate {0} not in pem format")]
    CertfileFormatInvalid(PathBuf),
    #[error(
        "Could not find native TLS implementation (see https://docs.rs/native-tls/latest/native_tls/index.html#how-is-this-implemented)"
    )]
    NativeTls,
    #[error("Server certificate could not be validated: {0}")]
    Validation(native_tls::Error),
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    TlsError(TlsError),
    #[error("Connecting to {host}:{port} failed: {cause}")]
    Connection {
        host: String,
        port: u16,
        cause: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, str::FromStr};

    use assertables::*;
    use rstest::*;

    use super::*;
    use testcontainers::{
        ContainerAsync, GenericImage, Healthcheck, ImageExt,
        core::{AccessMode, ContainerPort, Mount, WaitFor},
        runners::AsyncRunner,
    };

    const IMAPS_PORT: ContainerPort = ContainerPort::Tcp(31993);

    pub struct MockServer {
        server: ContainerAsync<GenericImage>,
    }

    impl MockServer {
        pub async fn hostname(&self) -> String {
            assert_ok!(self.server.get_host().await).to_string()
        }

        pub async fn port(&self) -> u16 {
            assert_ok!(self.server.get_host_port_ipv4(31993).await)
        }
    }

    #[fixture]
    pub async fn server() -> MockServer {
        MockServer {
            server: assert_ok!(
                GenericImage::new("dovecot/dovecot", "2.4.4-dev")
                    .with_exposed_port(IMAPS_PORT)
                    .with_wait_for(WaitFor::healthcheck())
                    .with_health_check(Healthcheck::cmd([
                        "nc",
                        "-z",
                        "-w",
                        "5",
                        "localhost",
                        &IMAPS_PORT.to_string(),
                    ]))
                    .with_mount(
                        Mount::bind_mount(
                            format!("{}/mock/certificate.crt", env!("CARGO_MANIFEST_DIR")),
                            "/etc/dovecot/ssl/tls.crt",
                        )
                        .with_access_mode(AccessMode::ReadOnly)
                    )
                    .with_mount(
                        Mount::bind_mount(
                            format!("{}/mock/private_key.pem", env!("CARGO_MANIFEST_DIR")),
                            "/etc/dovecot/ssl/tls.key",
                        )
                        .with_access_mode(AccessMode::ReadOnly)
                    )
                    .start()
                    .await
            ),
        }
    }

    #[rstest]
    #[awt]
    #[tokio::test]
    async fn test_connecting_to_server_should_succeed(#[future] server: MockServer) {
        let (tx, _) = mpsc::channel(1);
        let config = config::Connection::new(
            server.hostname().await,
            server.port().await,
            Some(assert_ok!(PathBuf::from_str(&format!(
                "{}/mock/certificate.crt",
                env!("CARGO_MANIFEST_DIR")
            )))),
        );
        let _connection = assert_ok!(Connection::start(&config, tx,).await);
    }
}
