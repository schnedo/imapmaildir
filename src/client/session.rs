use futures::StreamExt;

use super::Client;

pub struct Session {
    client: Client,
}

impl Session {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub async fn select(&mut self, mailbox: &str) {
        let command = format!("SELECT {mailbox}");
        dbg!(&command);
        let mut responses = self.client.send(&command);
        while (responses.next().await).is_some() {}
    }
}
