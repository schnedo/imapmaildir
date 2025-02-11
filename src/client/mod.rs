mod codec;

use codec::ImapCodec;
use futures::stream::StreamExt;
use imap_proto::{Capability, Response, ResponseCode, Status};
use tokio::net::TcpStream;
use tokio_native_tls::{native_tls, TlsConnector, TlsStream};
use tokio_util::codec::Framed;

use crate::config::Config;

pub struct Client {
    can_idle: bool,
    transport: Framed<TlsStream<TcpStream>, ImapCodec>,
}

impl Client {
    pub async fn connect(config: &Config) -> Self {
        let tls = native_tls::TlsConnector::new().expect("native tls should be available");
        let tls = TlsConnector::from(tls);
        let stream = (TcpStream::connect((config.host(), config.port)).await)
            .expect("connection to server should succeed");
        let stream =
            (tls.connect(config.host(), stream).await).expect("upgrading to tls should succeed");

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
        }
    }
}
