mod codec;
mod connection;
mod session;
mod tag_generator;

use connection::{Connection, ResponseStream};
use futures::stream::StreamExt;
use imap_proto::{Capability, Response, ResponseCode, Status};
use session::Session;
use tag_generator::TagGenerator;

pub struct Client {
    can_idle: bool,
    connection: Connection,
}

impl Client {
    pub async fn connect(host: &str, port: u16) -> Self {
        let (connection, greeting) = Connection::connect_to(host, port).await;

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
            connection,
        }
    }

    pub async fn login(mut self, username: &str, password: &str) -> Session {
        let command = format!("LOGIN {username} {password}");
        let mut responses = self.send(&command);
        while responses.next().await.is_some() {}
        Session::new(self)
    }

    fn send<'a>(&'a mut self, command: &'a str) -> ResponseStream<'a> {
        self.connection.send(command)
    }
}
