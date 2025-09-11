use tokio::sync::mpsc;

use crate::imap::{client::capability::Capabilities, codec::ResponseData, connection::Connection};

pub struct AuthenticatedClient {
    connection: Connection,
    untagged_response_receiver: mpsc::Receiver<ResponseData>,
    capabilities: Capabilities,
}

impl AuthenticatedClient {
    pub fn new(
        connection: Connection,
        capabilities: Capabilities,
        untagged_response_receiver: mpsc::Receiver<ResponseData>,
    ) -> Self {
        Self {
            connection,
            untagged_response_receiver,
            capabilities,
        }
    }
}
