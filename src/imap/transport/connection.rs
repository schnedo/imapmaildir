use std::{fmt::Debug, io, path::PathBuf};

use asynchronous_codec::Framed;
use compio::{
    io::compat::AsyncStream,
    native_tls::{self, Certificate},
    net::TcpStream,
    tls::{TlsConnector, TlsStream},
};
use futures::{SinkExt, StreamExt};
use thiserror::Error;

use crate::{
    config,
    imap::transport::{
        ResponseHandler,
        codec::{ImapCodec, ResponseData},
        tag_generator::TagGenerator,
    },
};

const READ_BUFFER_SIZE: usize = 256 * 1024;

pub struct Connection {
    tag_generator: TagGenerator,
    stream: Framed<TlsStream<TcpStream>, ImapCodec>,
}

impl Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("tag_generator", &self.tag_generator)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct Tls(TlsConnector);

impl Tls {
    /// # Errors
    /// If the configured server certificate cannot be read or parsed, or no native TLS
    /// implementation is available
    pub fn new(connection_config: &config::Connection) -> Result<Self, Error> {
        log::debug!("Setting up TLS");
        let mut tls = native_tls::TlsConnector::builder();
        if let Some(cert_file) = connection_config.server_certificate_file() {
            let cert = std::fs::read(cert_file)
                .map_err(|e| Error::TlsError(TlsError::CertfileReadError(e)))?;
            tls.add_root_certificate(Certificate::from_pem(&cert).map_err(|_| {
                Error::TlsError(TlsError::CertfileFormatInvalid(cert_file.clone()))
            })?);
        }
        let tls = tls
            .build()
            .map_err(|_| Error::TlsError(TlsError::NativeTls))?;

        Ok(Self(TlsConnector::from(tls)))
    }
}

impl Connection {
    pub async fn start(connection_config: &config::Connection, tls: &Tls) -> Result<Self, Error> {
        log::debug!("Connecting to server");
        let tls = &tls.0;
        let host = connection_config.host().as_str();
        let port = connection_config.port();
        let stream =
            (TcpStream::connect((host, port)).await).map_err(|cause| Error::Connection {
                host: host.to_string(),
                port,
                cause,
            })?;
        let stream = (tls
            .connect_compat(host, AsyncStream::with_capacity(READ_BUFFER_SIZE, stream))
            .await)
            .map_err(|e| Error::TlsError(TlsError::Validation(e)))?;

        Ok(Self {
            tag_generator: TagGenerator::default(),
            stream: Framed::new(stream, ImapCodec::default()),
        })
    }

    pub async fn next_response(&mut self) -> Result<ResponseData, Error> {
        self.stream
            .next()
            .await
            .ok_or(Error::Closed)?
            .map_err(Error::Receive)
    }

    pub async fn send<H: ResponseHandler>(
        &mut self,
        command: Vec<u8>,
        handler: &mut H,
    ) -> Result<ResponseData, H::Error> {
        let tag = self.write_command(command).await?;
        self.receive_completion(&tag, handler).await
    }

    pub async fn write_command(&mut self, command: Vec<u8>) -> Result<String, Error> {
        let tag = self.tag_generator.next();
        self.write(&tag, &command).await?;
        Ok(tag)
    }

    pub async fn write_continuation(&mut self, data: Vec<u8>) -> Result<(), Error> {
        self.write("", &data).await
    }

    async fn write(&mut self, tag: &str, data: &[u8]) -> Result<(), Error> {
        log::trace!("{tag:?}: sending");
        self.stream
            .send((tag.as_bytes(), data))
            .await
            .map_err(Error::Send)
    }

    pub async fn receive_completion<H: ResponseHandler>(
        &mut self,
        tag: &str,
        handler: &mut H,
    ) -> Result<ResponseData, H::Error> {
        loop {
            let response = self.next_response().await?;
            match response.parsed() {
                imap_proto::Response::Done {
                    tag: response_tag,
                    status,
                    outcome: imap_proto::Outcome { code, information },
                } => {
                    log::trace!("{response_tag:?} {status:?} {code:?}");
                    if let Some(information) = information {
                        log::debug!("Done response information: {information}");
                    }
                    if response_tag.0 != tag {
                        return Err(Error::UnexpectedTag {
                            expected: tag.to_string(),
                            received: response_tag.0.clone(),
                        }
                        .into());
                    }
                    let information = information.as_ref().map(ToString::to_string);
                    return match status {
                        imap_proto::Status::Ok => Ok(response),
                        imap_proto::Status::No => Err(Error::No(information).into()),
                        imap_proto::Status::Bad => Err(Error::Bad(information).into()),
                        imap_proto::Status::PreAuth | imap_proto::Status::Bye => {
                            Err(Error::UnexpectedTaggedStatus(format!("{status:?}")).into())
                        }
                    };
                }
                imap_proto::Response::Continue(imap_proto::Outcome { code, information }) => {
                    log::warn!("ignoring unexpected continuation request {code:?} {information:?}");
                }
                _ => handler.handle_or_log(response).await?,
            }
        }
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
    Validation(io::Error),
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
    #[error("Connection closed by server")]
    Closed,
    #[error("Cannot receive response: {0}")]
    Receive(io::Error),
    #[error("Cannot send command: {0}")]
    Send(io::Error),
    #[error("Server rejected command: {}", .0.as_deref().unwrap_or("NO"))]
    No(Option<String>),
    #[error("Server reported protocol error: {}", .0.as_deref().unwrap_or("BAD"))]
    Bad(Option<String>),
    #[error("Received tagged response {received} while expecting {expected}")]
    UnexpectedTag { expected: String, received: String },
    #[error("Received invalid tagged {0} response")]
    UnexpectedTaggedStatus(String),
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
    };

    use assertables::*;
    use rstest::*;
    use tempfile::tempdir;

    use super::*;
    use testcontainers::{
        Container, GenericImage, ImageExt,
        core::{AccessMode, ContainerPort, Mount, WaitFor},
        runners::SyncRunner,
    };

    macro_rules! mock_path {
        ($($suffix:literal),*) => {
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock/", $($suffix),*)
        };
    }
    const IMAPS_PORT: ContainerPort = ContainerPort::Tcp(31993);
    const CERTIFICATE_PATH: &str = mock_path!("certificate.crt");

    pub struct MockServer {
        server: Container<GenericImage>,
    }

    impl MockServer {
        pub fn hostname(&self) -> String {
            assert_ok!(self.server.get_host()).to_string()
        }

        pub fn port(&self) -> u16 {
            assert_ok!(self.server.get_host_port_ipv4(31993))
        }
    }

    fn copy_dir(from: impl AsRef<Path>, to: impl AsRef<Path>) {
        let from = from.as_ref();
        let to = to.as_ref();
        assert_ok!(fs::create_dir_all(to));
        for entry in assert_ok!(from.read_dir()) {
            let entry = assert_ok!(entry);
            let ftype = assert_ok!(entry.file_type());
            if ftype.is_dir() {
                copy_dir(entry.path(), to.join(entry.file_name()));
            } else {
                if entry.file_name() == ".gitkeep" {
                    continue;
                }
                assert_ok!(fs::copy(entry.path(), to.join(entry.file_name())));
            }
        }
    }

    fn fix_permissions(path: &Path) {
        let mut permissions = assert_ok!(path.metadata()).permissions();
        permissions.set_mode(0o777);
        assert_ok!(fs::set_permissions(path, permissions));
        if path.is_dir() {
            for path in assert_ok!(fs::read_dir(path)) {
                let entry = assert_ok!(path);
                fix_permissions(&entry.path());
            }
        }
    }

    #[fixture]
    pub async fn server() -> MockServer {
        let password = "password".to_string();
        let tmp_dir = assert_ok!(tempdir());
        let client_base_path = tmp_dir.path().join("local");
        copy_dir(mock_path!("data/local"), &client_base_path);
        let server_dir = tmp_dir.path().join("remote");
        copy_dir(mock_path!("data/remote"), &server_dir);
        fix_permissions(&server_dir);
        MockServer {
            server: assert_ok!(
                GenericImage::new("dovecot/dovecot", "2.4.4-dev")
                    .with_exposed_port(IMAPS_PORT)
                    .with_wait_for(WaitFor::message_on_stdout("starting up for"))
                    .with_mount(
                        Mount::bind_mount(CERTIFICATE_PATH, "/etc/dovecot/ssl/tls.crt")
                            .with_access_mode(AccessMode::ReadOnly),
                    )
                    .with_mount(
                        Mount::bind_mount(
                            mock_path!("private_key.pem"),
                            "/etc/dovecot/ssl/tls.key"
                        )
                        .with_access_mode(AccessMode::ReadOnly),
                    )
                    .with_mount(
                        Mount::bind_mount(mock_path!("dovecot.conf"), "/etc/dovecot/dovecot.conf")
                            .with_access_mode(AccessMode::ReadOnly),
                    )
                    .with_mount(Mount::bind_mount(
                        server_dir.to_string_lossy(),
                        "/srv/vmail/user/mail"
                    ))
                    .with_env_var("USER_PASSWORD", &password)
                    .start()
            ),
        }
    }

    #[rstest]
    #[awt]
    #[compio::test]
    async fn test_connecting_to_server_should_succeed(#[future] server: MockServer) {
        let config = config::Connection::new(
            server.hostname(),
            server.port(),
            Some(PathBuf::from(CERTIFICATE_PATH)),
        );
        let tls = assert_ok!(Tls::new(&config));
        let _connection = assert_ok!(Connection::start(&config, &tls).await);
    }
}
