use crate::imap::connection::SendCommand;

use super::{
    mailbox::Mailbox,
    select::{select, SelectError},
};

pub struct Session<T: SendCommand> {
    connection: T,
    selected_mailbox: Option<Mailbox>,
}

impl<T: SendCommand> Session<T> {
    pub fn new(connection: T) -> Self {
        Self {
            connection,
            selected_mailbox: None,
        }
    }

    pub async fn select<'a>(&mut self, mailbox: &'a str) -> Result<(), SelectError<'a>> {
        match select(&mut self.connection, mailbox).await {
            Ok(mailbox) => {
                self.selected_mailbox = Some(mailbox);
                Ok(())
            }
            Err(e) => {
                self.selected_mailbox = None;
                Err(e)
            }
        }
    }
}
