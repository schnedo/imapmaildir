use futures::StreamExt;

use super::connection::Connection;

pub struct Session {
    connection: Connection,
}

impl Session {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

    pub async fn select(&mut self, mailbox: &str) {
        let command = format!("SELECT {mailbox}");
        dbg!(&command);
        let mut responses = self.connection.send(&command);
        while (responses.next().await).is_some() {}
    }
}
