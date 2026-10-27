use std::{fmt::Debug, io, path::PathBuf, time::Duration};

use asynchronous_codec::Framed;
use compio::{
    BufResult,
    buf::{IntoInner as _, IoBuf as _},
    fs::File,
    io::AsyncReadAtExt as _,
    io::compat::AsyncStream,
    native_tls::{self, Certificate},
    net::TcpStream,
    tls::{TlsConnector, TlsStream},
};
use derive_getters::Getters;
use futures::{AsyncWrite, AsyncWriteExt as _, SinkExt, StreamExt};
use thiserror::Error;

use crate::imap::transport::{
    ResponseHandler,
    codec::{ImapCodec, ResponseData},
    tag_generator::TagGenerator,
};

const READ_BUFFER_SIZE: usize = 256 * 1024;

/// Size of the chunks, in which commands with file contents get written
const STREAM_CHUNK_SIZE: usize = 256 * 1024;

/// Part of a command, whose file contents get streamed instead of being held in memory
#[derive(Clone, Copy)]
pub enum CommandPart<'a> {
    Bytes(&'a [u8]),
    /// The first `len` bytes of `file`. Errors if the file got shorter meanwhile, as the server
    /// expects exactly `len` bytes then.
    File {
        file: &'a File,
        len: usize,
    },
    /// Waits for the server's continuation request before sending the following parts, as
    /// required after synchronizing literal headers
    Continuation,
}

#[derive(Getters, Debug)]
pub struct Config {
    host: String,
    port: u16,
    server_certificate_file: Option<PathBuf>,
}

impl Config {
    #[must_use]
    pub fn new(host: String, port: u16, server_certificate_file: Option<PathBuf>) -> Self {
        Self {
            host,
            port,
            server_certificate_file,
        }
    }
}

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
    pub fn new(connection_config: &Config) -> Result<Self, Error> {
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
    pub async fn start(connection_config: &Config, tls: &Tls) -> Result<Self, Error> {
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
        Self::detect_dead_connections(&stream).map_err(|cause| Error::Connection {
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

    /// Makes reads and writes fail on connections, that broke silently, e.g. due to NAT timeouts,
    /// which would otherwise hang while idling or waiting for responses
    fn detect_dead_connections(stream: &TcpStream) -> io::Result<()> {
        rustix::net::sockopt::set_socket_keepalive(stream, true)?;
        rustix::net::sockopt::set_tcp_keepidle(stream, Duration::from_mins(1))?;
        rustix::net::sockopt::set_tcp_keepintvl(stream, Duration::from_secs(10))?;
        rustix::net::sockopt::set_tcp_keepcnt(stream, 6)?;
        // unacknowledged writes are not covered by keepalive
        rustix::net::sockopt::set_tcp_user_timeout(stream, 2 * 60 * 1000)?;

        Ok(())
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

    /// Like [`Self::send_parts`], but streams file contents in chunks instead of buffering the
    /// whole command. Waits for continuation requests at [`CommandPart::Continuation`]. Returns
    /// early if the server completes the command instead, e.g. rejecting a literal.
    pub async fn send_streamed<H: ResponseHandler>(
        &mut self,
        parts: &[CommandPart<'_>],
        handler: &mut H,
    ) -> Result<ResponseData, H::Error> {
        let tag = self.tag_generator.next();
        let prefix = format!("{tag} ");
        let mut command = Vec::with_capacity(parts.len() + 2);
        command.push(CommandPart::Bytes(prefix.as_bytes()));
        command.extend_from_slice(parts);
        command.push(CommandPart::Bytes(b"\r\n"));
        let mut segments = command
            .split(|part| matches!(part, CommandPart::Continuation))
            .peekable();
        while let Some(segment) = segments.next() {
            self.write_streamed(&tag, segment).await?;
            if segments.peek().is_some()
                && let Continued::Done(completion) = self.receive_continued(&tag, handler).await?
            {
                log::debug!("{tag:?}: completed before sending literal");
                return Ok(completion);
            }
        }
        self.receive_completion(&tag, handler).await
    }

    async fn write_streamed(&mut self, tag: &str, parts: &[CommandPart<'_>]) -> Result<(), Error> {
        log::trace!("{tag:?}: streaming");
        // the stream gets written directly, which must not overtake buffered commands
        SinkExt::flush(&mut self.stream)
            .await
            .map_err(Error::Send)?;
        write_parts(&mut *self.stream, parts, STREAM_CHUNK_SIZE)
            .await
            .map_err(Error::Send)
    }

    /// Passes untagged responses to `handler` until receiving a continuation request or the
    /// completion of the command with `tag`
    pub async fn receive_continued<H: ResponseHandler>(
        &mut self,
        tag: &str,
        handler: &mut H,
    ) -> Result<Continued, H::Error> {
        match self.receive_next(handler).await? {
            Received::Continuation(text) => Ok(Continued::Request(text)),
            Received::Done(received, _) if received != tag => Err(Error::UnexpectedTag {
                expected: tag.to_string(),
                received,
            }
            .into()),
            Received::Done(_, completion) => Ok(Continued::Done(completion?)),
        }
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
        self.send_pipelined_each(commands, handler)
            .await?
            .into_iter()
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    /// Like [`Self::send_pipelined`], but with the completion of each command, e.g. as some
    /// may succeed despite others getting rejected
    pub async fn send_pipelined_each<H: ResponseHandler>(
        &mut self,
        commands: Vec<Vec<u8>>,
        handler: &mut H,
    ) -> Result<Vec<Result<ResponseData, Error>>, H::Error> {
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

        Ok(completions.into_results())
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
            match self.receive_next(handler).await? {
                Received::Continuation(_) => {
                    log::warn!("ignoring unexpected continuation request");
                }
                Received::Done(tag, completion) => return Ok((tag, completion)),
            }
        }
    }

    /// Passes untagged responses to `handler` until receiving a tagged response or continuation
    /// request
    async fn receive_next<H: ResponseHandler>(
        &mut self,
        handler: &mut H,
    ) -> Result<Received, H::Error> {
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
                    log::trace!("continuation request {code:?} {information:?}");
                    return Ok(Received::Continuation(
                        information.as_ref().map(ToString::to_string),
                    ));
                }
                _ => {
                    handler.handle_or_log(response).await?;
                    continue;
                }
            };

            return Ok(Received::Done(tag, error.map_or(Ok(response), Err)));
        }
    }
}

/// Reaction of the server to a command waiting for continuation requests
pub enum Continued {
    /// Continuation request with its text, e.g. a SASL challenge
    Request(Option<String>),
    /// Completion with OK. NO and BAD become errors.
    Done(ResponseData),
}

enum Received {
    /// Continuation request with its text, e.g. a SASL challenge
    Continuation(Option<String>),
    /// Tag and the response if OK
    Done(String, Result<ResponseData, Error>),
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

    fn into_results(self) -> Vec<Result<ResponseData, Error>> {
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

impl Error {
    /// Whether the error is caused by a broken connection or temporary server condition, so that
    /// reconnecting may succeed
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Connection { .. } | Self::Closed => true,
            // unparseable responses repeat after reconnecting
            Self::Receive(e) | Self::Send(e) => e.kind() != io::ErrorKind::InvalidData,
            Self::TlsError(TlsError::Validation(e)) => is_transient_io(e),
            Self::No(information) => information.as_deref().is_some_and(is_transient_no),
            Self::TlsError(_)
            | Self::Bad(_)
            | Self::UnexpectedTag { .. }
            | Self::UnexpectedTaggedStatus(_) => false,
        }
    }
}

/// Whether `e` broke the connection instead of e.g. rejecting the server certificate
fn is_transient_io(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::TimedOut
    )
}

/// Whether a NO response reports a temporary failure (RFC 5530). imap-proto keeps these codes
/// in the information text.
pub fn is_transient_no(information: &str) -> bool {
    ["[UNAVAILABLE]", "[INUSE]", "[LIMIT]"]
        .iter()
        .any(|code| information.starts_with(code))
}

/// Writes `parts` to `writer` in chunks of `chunk_size`. Reads the next chunk while writing the
/// current one, so that slow storage and network do not wait for each other.
async fn write_parts(
    writer: &mut (impl AsyncWrite + Unpin),
    parts: &[CommandPart<'_>],
    chunk_size: usize,
) -> io::Result<()> {
    let mut filler = ChunkFiller {
        parts,
        index: 0,
        offset: 0,
        chunk_size,
    };
    let mut current = filler.fill(Vec::with_capacity(chunk_size)).await?;
    let mut spare = Vec::with_capacity(chunk_size);
    while let Some(chunk) = current {
        let (written, next) = futures::join!(writer.write_all(&chunk), filler.fill(spare));
        written?;
        current = next?;
        spare = chunk;
    }
    writer.flush().await
}

/// Fills chunks with the consecutive bytes of command parts, reading file contents directly
/// into them
struct ChunkFiller<'a, 'b> {
    parts: &'a [CommandPart<'b>],
    index: usize,
    /// Bytes of the current part already in chunks
    offset: usize,
    chunk_size: usize,
}

impl ChunkFiller<'_, '_> {
    /// Fills `chunk` with the next bytes. Returns `None` after all parts got filled in.
    async fn fill(&mut self, mut chunk: Vec<u8>) -> io::Result<Option<Vec<u8>>> {
        chunk.clear();
        while chunk.len() < self.chunk_size
            && let Some(part) = self.parts.get(self.index)
        {
            let start = chunk.len();
            match *part {
                CommandPart::Bytes(bytes) => {
                    let end = bytes.len().min(self.offset + self.chunk_size - start);
                    chunk.extend_from_slice(&bytes[self.offset..end]);
                }
                CommandPart::File { file, len } => {
                    let end = start + (len - self.offset).min(self.chunk_size - start);
                    let BufResult(read, slice) = file
                        .read_exact_at(chunk.slice(start..end), self.offset as u64)
                        .await;
                    chunk = slice.into_inner();
                    read?;
                }
                CommandPart::Continuation => {
                    unreachable!("continuations should be split off before writing")
                }
            }
            self.offset += chunk.len() - start;
            let part_len = match *part {
                CommandPart::Bytes(bytes) => bytes.len(),
                CommandPart::File { len, .. } => len,
                CommandPart::Continuation => 0,
            };
            if self.offset == part_len {
                self.index += 1;
                self.offset = 0;
            }
        }

        Ok((!chunk.is_empty()).then_some(chunk))
    }
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
    async fn file_with(dir: &Path, name: &str, content: &[u8]) -> File {
        let path = dir.join(name);
        assert_ok!(fs::write(&path, content));
        assert_ok!(File::open(path).await)
    }

    /// Writes `parts` with tag `A1` and returns the written bytes and number of writes
    async fn written(parts: &[CommandPart<'_>], chunk_size: usize) -> io::Result<(Vec<u8>, usize)> {
        #[derive(Default)]
        struct Recorder(Vec<u8>, usize);
        impl AsyncWrite for Recorder {
            fn poll_write(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                buf: &[u8],
            ) -> std::task::Poll<io::Result<usize>> {
                self.0.extend_from_slice(buf);
                self.1 += 1;
                std::task::Poll::Ready(Ok(buf.len()))
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_close(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        let mut recorder = Recorder::default();
        let mut command = vec![CommandPart::Bytes(b"A1 ")];
        command.extend_from_slice(parts);
        command.push(CommandPart::Bytes(b"\r\n"));
        write_parts(&mut recorder, &command, chunk_size).await?;

        Ok((recorder.0, recorder.1))
    }

    #[compio::test]
    async fn test_write_parts_streams_files_across_chunks() {
        let dir = assert_ok!(tempdir());
        let first = file_with(dir.path(), "first", b"0123456789").await;
        let second = file_with(dir.path(), "second", b"abcdefghijklmnopqrstuvwxyz").await;
        let parts = [
            CommandPart::Bytes(b"APPEND INBOX {10+}\r\n"),
            CommandPart::File {
                file: &first,
                len: 10,
            },
            CommandPart::Bytes(b" {26+}\r\n"),
            CommandPart::File {
                file: &second,
                len: 26,
            },
        ];

        let (bytes, writes) = assert_ok!(written(&parts, 8).await);

        assert_eq!(
            String::from_utf8_lossy(&bytes),
            "A1 APPEND INBOX {10+}\r\n0123456789 {26+}\r\nabcdefghijklmnopqrstuvwxyz\r\n"
        );
        assert_gt!(writes, 1);
    }

    #[compio::test]
    async fn test_write_parts_handles_empty_files_and_tiny_chunks() {
        let dir = assert_ok!(tempdir());
        let empty = file_with(dir.path(), "empty", b"").await;
        let mail = file_with(dir.path(), "mail", b"hello").await;
        let parts = [
            CommandPart::Bytes(b"X {0+}\r\n"),
            CommandPart::File {
                file: &empty,
                len: 0,
            },
            CommandPart::Bytes(b" {5+}\r\n"),
            CommandPart::File {
                file: &mail,
                len: 5,
            },
        ];

        let (bytes, writes) = assert_ok!(written(&parts, 1).await);

        assert_eq!(bytes, b"A1 X {0+}\r\n {5+}\r\nhello\r\n");
        assert_eq!(writes, bytes.len());
    }

    #[compio::test]
    async fn test_write_parts_sends_only_announced_length() {
        let dir = assert_ok!(tempdir());
        let file = file_with(dir.path(), "grown", b"0123456789").await;

        let (bytes, _) = assert_ok!(
            written(
                &[CommandPart::File {
                    file: &file,
                    len: 4
                }],
                64
            )
            .await
        );

        assert_eq!(bytes, b"A1 0123\r\n");
    }

    #[compio::test]
    async fn test_write_parts_errors_on_shrunk_file() {
        let dir = assert_ok!(tempdir());
        let file = file_with(dir.path(), "shrunk", b"0123").await;

        let error = assert_err!(
            written(
                &[CommandPart::File {
                    file: &file,
                    len: 10
                }],
                64
            )
            .await
        );

        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
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

        let tags: Vec<_> = completions
            .into_results()
            .into_iter()
            .map(|response| match assert_ok!(response).parsed() {
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
    fn test_completions_keep_failures_of_each_command() {
        let mut completions = expecting(&["a1", "a2", "a3"]);

        assert_ok!(completions.complete("a3".to_string(), Err(Error::Bad(None))));
        assert_ok!(completions.complete("a1".to_string(), Ok(done(b"a1 OK done\r\n"))));
        assert_ok!(completions.complete("a2".to_string(), Err(Error::No(None))));

        assert_matches!(
            completions.into_results().as_slice(),
            [Ok(_), Err(Error::No(None)), Err(Error::Bad(None))]
        );
    }

    #[rstest]
    #[awt]
    #[compio::test]
    async fn test_connecting_to_server_should_succeed(#[future] server: MockServer) {
        let config = Config::new(
            server.hostname(),
            server.port(),
            Some(PathBuf::from(CERTIFICATE_PATH)),
        );
        let tls = assert_ok!(Tls::new(&config));
        let _connection = assert_ok!(Connection::start(&config, &tls).await);
    }
}

#[cfg(test)]
mod transient_tests {
    use super::*;

    #[test]
    fn test_broken_connections_are_transient() {
        assert!(Error::Closed.is_transient());
        assert!(Error::Receive(io::Error::from(io::ErrorKind::TimedOut)).is_transient());
        assert!(Error::Send(io::Error::from(io::ErrorKind::BrokenPipe)).is_transient());
        assert!(
            Error::TlsError(TlsError::Validation(io::Error::from(
                io::ErrorKind::ConnectionReset
            )))
            .is_transient()
        );
    }

    #[test]
    fn test_unparseable_response_is_permanent() {
        assert!(!Error::Receive(io::Error::from(io::ErrorKind::InvalidData)).is_transient());
    }

    #[test]
    fn test_rejected_certificate_is_permanent() {
        assert!(
            !Error::TlsError(TlsError::Validation(io::Error::other(
                "invalid certificate"
            )))
            .is_transient()
        );
    }

    #[test]
    fn test_no_is_transient_only_for_temporary_codes() {
        assert!(Error::No(Some("[UNAVAILABLE] try later".to_string())).is_transient());
        assert!(Error::No(Some("[LIMIT] too many connections".to_string())).is_transient());
        assert!(!Error::No(Some("[OVERQUOTA] mailbox full".to_string())).is_transient());
        assert!(!Error::No(None).is_transient());
        assert!(!Error::Bad(None).is_transient());
    }

    #[test]
    fn test_imap_proto_keeps_rfc5530_codes_in_information() {
        let (_, response) = imap_proto::Response::parse(
            b"a1 NO [AUTHENTICATIONFAILED] Invalid credentials (Failure)\r\n",
        )
        .expect("response should parse");
        let imap_proto::Response::Done {
            outcome: imap_proto::Outcome { code, information },
            ..
        } = response
        else {
            panic!("response should be tagged");
        };
        assert!(code.is_none());
        assert_eq!(
            information.as_deref(),
            Some("[AUTHENTICATIONFAILED] Invalid credentials (Failure)")
        );
    }
}
