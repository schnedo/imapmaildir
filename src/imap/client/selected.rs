use tokio::sync::mpsc;

use crate::imap::{
    client::capability::Capabilities, codec::ResponseData, connection::Connection, mailbox::Mailbox,
};

pub struct SelectedClient {
    connection: Connection,
    untagged_response_receiver: mpsc::Receiver<ResponseData>,
    capabilities: Capabilities,
    mailbox: Mailbox,
}
impl SelectedClient {
    pub fn new(
        connection: Connection,
        untagged_response_receiver: mpsc::Receiver<ResponseData>,
        capabilities: Capabilities,
        mailbox: Mailbox,
    ) -> Self {
        Self {
            connection,
            untagged_response_receiver,
            capabilities,
            mailbox,
        }
    }
}
