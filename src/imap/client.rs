use std::sync::Arc;

use log::debug;
use tokio::sync::mpsc;

use crate::imap::{connection::Connection, imap_state::ImapState};

pub struct Client {
    connection: Connection,
    state: Arc<ImapState>,
}

impl Client {
    pub async fn start(host: &str, port: u16) -> Self {
        let (untagged_response_sender, mut untagged_response_receiver) = mpsc::channel(32);
        let connection = Connection::start(host, port, untagged_response_sender).await;
        let this = Self {
            connection,
            state: Arc::new(ImapState::default()),
        };
        let state = this.state.clone();

        tokio::spawn(async move {
            while let Some(response) = untagged_response_receiver.recv().await {
                state.handle_untagged_response(response.parsed());
            }
        });

        this
    }

    pub async fn login(&mut self, username: &str, password: &str) {
        debug!("LOGIN <user> <password>");
        let response = self
            .connection
            .send(&format!("LOGIN {username} {password}"))
            .await
            .expect("login should succeed");
        if let Some(imap_proto::ResponseCode::Capabilities(items)) =
            response.unsafe_get_tagged_response_code()
        {
            self.state.update_capabilities(items);
        } else {
            self.connection
                .send("CAPABILITY")
                .await
                .expect("capabilities should succeed");
        }
    }
}
