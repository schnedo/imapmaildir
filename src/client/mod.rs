mod codec;
mod tag_generator;

use std::{
    borrow::Cow,
    pin::Pin,
    task::{Context, Poll},
};

use codec::ImapCodec;
use futures::{stream::StreamExt, SinkExt, Stream, TryStreamExt};
use imap_proto::{Capability, Request, Response, ResponseCode, Status};
use tag_generator::TagGenerator;
use tokio::net::TcpStream;
use tokio_native_tls::{native_tls, TlsConnector, TlsStream};
use tokio_util::codec::{Decoder, Framed};

type Transport = Framed<TlsStream<TcpStream>, ImapCodec>;

pub struct Client {
    can_idle: bool,
    transport: Transport,
    tag_generator: TagGenerator,
}

impl Client {
    pub async fn connect(host: &str, port: u16) -> Self {
        let tls = native_tls::TlsConnector::new().expect("native tls should be available");
        let tls = TlsConnector::from(tls);
        let stream =
            (TcpStream::connect((host, port)).await).expect("connection to server should succeed");
        let stream = (tls.connect(host, stream).await).expect("upgrading to tls should succeed");

        let mut transport = Framed::new(stream, ImapCodec::default());

        let greeting = (transport.next().await)
            .expect("greeting should be present")
            .expect("greeting should be parsable");

        let can_idle = if let Response::Data {
            status: Status::Ok,
            code: Some(ResponseCode::Capabilities(capabilities)),
            information: _,
        } = greeting.parsed()
        {
            dbg!(&capabilities);
            capabilities.contains(&Capability::Atom(std::borrow::Cow::Borrowed("IDLE")))
        } else {
            dbg!(&greeting);
            todo!("greeting should have capabilities")
        };

        Client {
            can_idle,
            transport,
            tag_generator: TagGenerator::default(),
        }
    }

    pub async fn login(mut self, username: &str, password: &str) -> Session {
        let command = format!("LOGIN {username} {password}");
        dbg!(&command);
        let mut responses = self.send(&command).await;
        while responses.next().await.is_some() {}

        Session { client: self }
    }

    async fn send(&mut self, command: &str) -> ResponseStream {
        let tag = self.tag_generator.next();
        let request = Request(
            Cow::Borrowed(tag.as_bytes()),
            Cow::Borrowed(command.as_bytes()),
        );
        dbg!(&tag);
        if (self.transport.send(&request).await).is_ok() {
            ResponseStream::new(&mut self.transport, tag)
        } else {
            todo!("handle connection error")
        }
    }
}

pub struct ResponseStream<'a> {
    transport: &'a mut Transport,
    done: bool,
    tag: String,
}

impl<'a> ResponseStream<'a> {
    pub fn new(transport: &'a mut Transport, tag: String) -> Self {
        Self {
            transport,
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
        let next_poll = self.transport.try_poll_next_unpin(cx);
        dbg!(&next_poll);
        match next_poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(data))) => {
                dbg!(&data);
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

pub struct Session {
    client: Client,
}

impl Session {
    pub async fn select(&mut self, mailbox: &str) {
        let command = format!("SELECT {mailbox}");
        dbg!(&command);
        let mut responses = self.client.send(&command).await;
        while (responses.next().await).is_some() {}
    }
}
