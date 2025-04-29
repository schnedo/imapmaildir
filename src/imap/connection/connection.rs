use futures::StreamExt;
use log::{debug, trace};
use tokio::net::TcpStream;
use tokio_native_tls::{native_tls, TlsConnector, TlsStream};
use tokio_util::codec::Framed;

use super::{
    codec::{ImapCodec, ResponseData},
    response_stream::ResponseStream,
    tag_generator::TagGenerator,
    SendCommand,
};

pub type ImapStream = Framed<TlsStream<TcpStream>, ImapCodec>;

pub struct Connection {
    stream: ImapStream,
    tag_generator: TagGenerator,
}

impl Connection {
    pub async fn connect_to(host: &str, port: u16) -> (Self, ResponseData) {
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
            Connection {
                stream,
                tag_generator: TagGenerator::default(),
            },
            response_data,
        )
    }
}

impl SendCommand for Connection {
    type Responses<'a> = ResponseStream<'a>;

    fn send<'a>(&'a mut self, command: &'a str) -> Self::Responses<'a> {
        ResponseStream::new(&mut self.stream, &mut self.tag_generator, command)
    }
}
