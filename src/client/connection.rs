use std::{
    borrow::Cow,
    pin::Pin,
    task::{Context, Poll},
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

    // TODO: try to remove async, and move sending into polling of ResponseStream
    pub async fn send(&mut self, command: &str) -> ResponseStream {
        let tag = self.tag_generator.next();
        let request = Request(
            Cow::Borrowed(tag.as_bytes()),
            Cow::Borrowed(command.as_bytes()),
        );
        if (self.stream.send(&request).await).is_ok() {
            ResponseStream::new(&mut self.stream, tag)
        } else {
            todo!("handle connection error")
        }
    }
}

pub struct ResponseStream<'a> {
    inner_stream: &'a mut ImapStream,
    done: bool,
    tag: String,
}

impl<'a> ResponseStream<'a> {
    pub fn new(inner_stream: &'a mut ImapStream, tag: String) -> Self {
        Self {
            inner_stream,
            done: false,
            tag,
        }
    }
}

impl Stream for ResponseStream<'_> {
    type Item = <ImapCodec as Decoder>::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        let next_poll = self.inner_stream.try_poll_next_unpin(cx);
        match next_poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(data))) => {
                if let Some(tag) = data.request_id() {
                    self.done = true;
                    assert_eq!(
                        tag.0, self.tag,
                        "Response tag did not match request tag. This should never happen and indicates that something is seriously wrong."
                    );
                }
                if data.request_id().is_some() {
                    self.done = true;
                }
                Poll::Ready(Some(data))
            }
            Poll::Ready(Some(Err(_))) => todo!("handle connection errors"),
        }
    }
}
