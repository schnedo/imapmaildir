mod codec;
mod connection;
mod session;
mod tag_generator;

use connection::{Connection, ResponseStream};
use futures::stream::StreamExt;
use session::Session;
use tag_generator::TagGenerator;

pub struct Client {
    connection: Connection,
}

impl Client {
    pub async fn connect(host: &str, port: u16) -> Self {
        let (connection, _) = Connection::connect_to(host, port).await;

        Client { connection }
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
