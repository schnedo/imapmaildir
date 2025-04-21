use std::{
    borrow::Cow,
    pin::Pin,
    task::{ready, Context, Poll},
};

use futures::{SinkExt, Stream, StreamExt, TryStreamExt};
use imap_proto::Request;
use tokio::net::TcpStream;
use tokio_native_tls::{native_tls, TlsConnector, TlsStream};
use tokio_util::codec::{Decoder, Framed};

use super::{
    codec::{ImapCodec, ResponseData},
    TagGenerator,
};

type ImapStream = Framed<TlsStream<TcpStream>, ImapCodec>;
pub struct Connection {
    stream: ImapStream,
    tag_generator: TagGenerator,
}

impl Connection {
    pub async fn connect_to(host: &str, port: u16) -> (Self, ResponseData) {
        let tls = native_tls::TlsConnector::new().expect("native tls should be available");
        let tls = TlsConnector::from(tls);
        let stream =
            (TcpStream::connect((host, port)).await).expect("connection to server should succeed");
        let stream = (tls.connect(host, stream).await).expect("upgrading to tls should succeed");

        let mut stream = Framed::new(stream, ImapCodec::default());

        let response_data = stream
            .next()
            .await
            .expect("greeting should be present")
            .expect("greeting should be parsable");

        (
            Connection {
                stream,
                tag_generator: TagGenerator::default(),
            },
            response_data,
        )
    }

    pub fn send<'a>(&'a mut self, command: &'a str) -> ResponseStream<'a> {
        ResponseStream::new(&mut self.stream, &mut self.tag_generator, command)
    }
}

enum ResponseStreamState {
    Start,
    Sending,
    Receiving,
    Done,
}

pub struct ResponseStream<'a> {
    imap_stream: &'a mut ImapStream,
    state: ResponseStreamState,
    tag_generator: &'a mut TagGenerator,
    tag: String,
    command: &'a str,
}

impl<'a> ResponseStream<'a> {
    pub fn new(
        imap_stream: &'a mut ImapStream,
        tag_generator: &'a mut TagGenerator,
        command: &'a str,
    ) -> Self {
        Self {
            imap_stream,
            state: ResponseStreamState::Start,
            tag_generator,
            tag: String::with_capacity(0),
            command,
        }
    }
}

impl Stream for ResponseStream<'_> {
    type Item = <ImapCodec as Decoder>::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.state {
                ResponseStreamState::Start => {
                    ready!(self.imap_stream.poll_ready_unpin(cx))
                        .expect("imap sink should be ready for receiving data");
                    let tag = self.tag_generator.next();
                    let request = Request(
                        Cow::Borrowed(tag.as_bytes()),
                        Cow::Borrowed(self.command.as_bytes()),
                    );
                    self.imap_stream
                        .start_send_unpin(&request)
                        .expect("imap sink should be able to receive data");
                    self.tag = tag;
                    self.state = ResponseStreamState::Sending;
                }
                ResponseStreamState::Sending => {
                    ready!(self.imap_stream.poll_flush_unpin(cx))
                        .expect("imap sink should be able to flush data");
                    self.state = ResponseStreamState::Receiving;
                }
                ResponseStreamState::Receiving => {
                    match ready!(self.imap_stream.try_poll_next_unpin(cx)) {
                        None => return Poll::Ready(None),
                        Some(Ok(data)) => {
                            if let Some(tag) = data.request_id() {
                                self.state = ResponseStreamState::Done;
                                assert_eq!(
                        tag.0, self.tag,
                        "Response tag did not match request tag. This should never happen and indicates that something is seriously wrong."
                    );
                            }
                            return Poll::Ready(Some(data));
                        }
                        Some(Err(_)) => todo!("handle connection errors"),
                    }
                }
                ResponseStreamState::Done => return Poll::Ready(None),
            }
        }
    }
}
