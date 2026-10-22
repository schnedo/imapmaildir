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
        self.send_parts(&[&command], handler).await
    }

    pub async fn send_parts<H: ResponseHandler>(
        &mut self,
        parts: &[&[u8]],
        handler: &mut H,
    ) -> Result<ResponseData, H::Error> {
        let tag = self.tag_generator.next();
        self.write(&tag, parts).await?;
        self.receive_completion(&tag, handler).await
    }

    pub async fn write_command(&mut self, command: Vec<u8>) -> Result<String, Error> {
        let tag = self.tag_generator.next();
        self.write(&tag, &[&command]).await?;
        Ok(tag)
    }

    pub async fn write_continuation(&mut self, data: Vec<u8>) -> Result<(), Error> {
        self.write("", &[&data]).await
    }

    async fn write(&mut self, tag: &str, parts: &[&[u8]]) -> Result<(), Error> {
        log::trace!("{tag:?}: sending");
        self.stream
            .send((tag.as_bytes(), parts))
            .await
            .map_err(Error::Send)
    }

    /// Sends `commands` without waiting for completions in between, saving a round trip per
    /// command. Only for commands that may run concurrently (RFC 3501 5.5). Returns completions in
    /// the order of `commands` after receiving all of them, failing with the first failed one.
    pub async fn send_pipelined<H: ResponseHandler>(
        &mut self,
        commands: Vec<Vec<u8>>,
        handler: &mut H,
    ) -> Result<Vec<ResponseData>, H::Error> {
        let mut completions = Completions::default();
        for command in commands {
            let tag = self.tag_generator.next();
            log::trace!("{tag:?}: sending pipelined");
            self.stream
                .feed((tag.as_bytes(), &[command.as_slice()][..]))
                .await
                .map_err(Error::Send)?;
            completions.expect(tag);
        }
        self.stream.flush().await.map_err(Error::Send)?;
        while !completions.is_complete() {
            let (tag, completion) = self.receive_done(handler).await?;
            completions.complete(tag, completion)?;
        }

        completions.into_responses().map_err(Into::into)
    }

    pub async fn receive_completion<H: ResponseHandler>(
        &mut self,
        tag: &str,
        handler: &mut H,
    ) -> Result<ResponseData, H::Error> {
        let (received, completion) = self.receive_done(handler).await?;
        if received != tag {
            return Err(Error::UnexpectedTag {
                expected: tag.to_string(),
                received,
            }
            .into());
        }

        completion.map_err(Into::into)
    }

    /// Passes untagged responses to `handler` until receiving a tagged one. Returns its tag and
    /// the response if OK.
    async fn receive_done<H: ResponseHandler>(
        &mut self,
        handler: &mut H,
    ) -> Result<(String, Result<ResponseData, Error>), H::Error> {
        loop {
            let response = self.next_response().await?;
            let (tag, error) = match response.parsed() {
                imap_proto::Response::Done {
                    tag: response_tag,
                    status,
                    outcome: imap_proto::Outcome { code, information },
                } => {
                    log::trace!("{response_tag:?} {status:?} {code:?}");
                    if let Some(information) = information {
                        log::debug!("Done response information: {information}");
                    }
                    let information = || information.as_ref().map(ToString::to_string);
                    let error = match status {
                        imap_proto::Status::Ok => None,
                        imap_proto::Status::No => Some(Error::No(information())),
                        imap_proto::Status::Bad => Some(Error::Bad(information())),
                        imap_proto::Status::PreAuth | imap_proto::Status::Bye => {
                            Some(Error::UnexpectedTaggedStatus(format!("{status:?}")))
                        }
                    };
                    (response_tag.0.clone(), error)
                }
                imap_proto::Response::Continue(imap_proto::Outcome { code, information }) => {
                    log::warn!("ignoring unexpected continuation request {code:?} {information:?}");
                    continue;
                }
                _ => {
                    handler.handle_or_log(response).await?;
                    continue;
                }
            };

            return Ok((tag, error.map_or(Ok(response), Err)));
        }
    }
}

/// Completions of pipelined commands, which servers may send in any order
#[derive(Default)]
struct Completions {
    tags: Vec<String>,
    results: Vec<Option<Result<ResponseData, Error>>>,
    pending: usize,
}

impl Completions {
    fn expect(&mut self, tag: String) {
        self.tags.push(tag);
        self.results.push(None);
        self.pending += 1;
    }

    fn is_complete(&self) -> bool {
        self.pending == 0
    }

    fn complete(
        &mut self,
        tag: String,
        completion: Result<ResponseData, Error>,
    ) -> Result<(), Error> {
        let Some(slot) = self
            .tags
            .iter()
            .position(|expected| *expected == tag)
            .map(|index| &mut self.results[index])
            .filter(|slot| slot.is_none())
        else {
            return Err(Error::UnexpectedTag {
                expected: self.tags.join(", "),
                received: tag,
            });
        };
        *slot = Some(completion);
        self.pending -= 1;

        Ok(())
    }

    fn into_responses(self) -> Result<Vec<ResponseData>, Error> {
        self.results
            .into_iter()
            .map(|completion| completion.expect("all pipelined commands should be complete"))
            .collect()
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

    fn done(raw: &'static [u8]) -> ResponseData {
        ResponseData::from_raw(raw)
    }

    fn expecting(tags: &[&str]) -> Completions {
        let mut completions = Completions::default();
        for tag in tags {
            completions.expect((*tag).to_string());
        }
        completions
    }

    #[test]
    fn test_completions_returns_out_of_order_completions_in_command_order() {
        let mut completions = expecting(&["a1", "a2"]);

        assert_ok!(completions.complete("a2".to_string(), Ok(done(b"a2 OK done\r\n"))));
        assert!(!completions.is_complete());
        assert_ok!(completions.complete("a1".to_string(), Ok(done(b"a1 OK done\r\n"))));
        assert!(completions.is_complete());

        let responses = assert_ok!(completions.into_responses());
        let tags: Vec<_> = responses
            .iter()
            .map(|response| match response.parsed() {
                imap_proto::Response::Done { tag, .. } => tag.0.clone(),
                other => panic!("unexpected response {other:?}"),
            })
            .collect();
        assert_eq!(tags, vec!["a1", "a2"]);
    }

    #[test]
    fn test_completions_rejects_unknown_and_repeated_tags() {
        let mut completions = expecting(&["a1", "a2"]);

        assert_matches!(
            completions.complete("b1".to_string(), Ok(done(b"b1 OK done\r\n"))),
            Err(Error::UnexpectedTag { .. })
        );
        assert_ok!(completions.complete("a1".to_string(), Ok(done(b"a1 OK done\r\n"))));
        assert_matches!(
            completions.complete("a1".to_string(), Ok(done(b"a1 OK done\r\n"))),
            Err(Error::UnexpectedTag { .. })
        );
        assert!(!completions.is_complete());
    }

    #[test]
    fn test_completions_fails_with_first_failed_command() {
        let mut completions = expecting(&["a1", "a2", "a3"]);

        assert_ok!(completions.complete("a3".to_string(), Err(Error::Bad(None))));
        assert_ok!(completions.complete("a1".to_string(), Ok(done(b"a1 OK done\r\n"))));
        assert_ok!(completions.complete("a2".to_string(), Err(Error::No(None))));

        assert_matches!(completions.into_responses(), Err(Error::No(None)));
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
