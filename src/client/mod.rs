mod codec;
mod tag_generator;

use codec::ImapCodec;
use futures::stream::StreamExt;
use imap_proto::{Capability, Response, ResponseCode, Status};
use tag_generator::TagGenerator;
use tokio::net::TcpStream;
use tokio_native_tls::{native_tls, TlsConnector, TlsStream};
use tokio_util::codec::Framed;

pub struct Client {
    can_idle: bool,
    transport: Framed<TlsStream<TcpStream>, ImapCodec>,
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
}
