mod codec;
mod response_stream;
mod session;
mod tag_generator;

use std::borrow::Cow;

use codec::ImapCodec;
use futures::{stream::StreamExt, SinkExt};
use imap_proto::{Capability, Request, Response, ResponseCode, Status};
use response_stream::ResponseStream;
use session::Session;
use tag_generator::TagGenerator;
use tokio::net::TcpStream;
use tokio_native_tls::{native_tls, TlsConnector, TlsStream};
use tokio_util::codec::Framed;

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
        let mut responses = self.send(&command).await;
        while responses.next().await.is_some() {}
        Session::new(self)
    }

    async fn send(&mut self, command: &str) -> ResponseStream {
        let tag = self.tag_generator.next();
        let request = Request(
            Cow::Borrowed(tag.as_bytes()),
            Cow::Borrowed(command.as_bytes()),
        );
        if (self.transport.send(&request).await).is_ok() {
            ResponseStream::new(&mut self.transport, tag)
        } else {
            todo!("handle connection error")
        }
    }
}
