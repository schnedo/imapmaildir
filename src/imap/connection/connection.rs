use std::cell::RefCell;

use futures::StreamExt;
use log::{debug, trace};
use tokio::net::TcpStream;
use tokio_native_tls::{TlsConnector, TlsStream, native_tls};
use tokio_util::codec::Framed;

use crate::imap::imap_repository::Connector;

use super::{
    SendCommand,
    codec::{ImapCodec, ResponseData},
    response_stream::ResponseStream,
    tag_generator::TagGenerator,
};

pub type ImapStream = Framed<TlsStream<TcpStream>, ImapCodec>;

pub struct Connection {
    stream: RefCell<ImapStream>,
    tag_generator: TagGenerator,
}

impl Connector for Connection {
    type Connection = Self;

    async fn connect_to(host: &str, port: u16) -> (Self::Connection, ResponseData) {
        debug!("Connecting to server");
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
        trace!("greeting = {response_data:?}");

        (
            Self {
                stream: RefCell::new(stream),
                tag_generator: TagGenerator::default(),
            },
            response_data,
        )
    }
}

impl SendCommand for Connection {
    type Responses<'a> = ResponseStream<'a>;

    fn send(&self, command: String) -> Self::Responses<'_> {
        let stream = self.stream.borrow_mut();
        ResponseStream::new(stream, &self.tag_generator, command)
    }
}
