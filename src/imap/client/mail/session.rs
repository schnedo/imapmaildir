use crate::{imap::connection::SendCommand, sync::Repository};

use super::{
    fetch::{fetch, RemoteMail, SequenceSet},
    idle::idle,
    mailbox::{Mailbox, UidValidity},
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

    pub async fn select<'a, 'b>(
        &'b mut self,
        mailbox: &'a str,
    ) -> Result<&'b UidValidity, SelectError<'a>> {
        match select(&mut self.connection, mailbox).await {
            Ok(mailbox) => {
                self.selected_mailbox = Some(mailbox);
                Ok(self.selected_mailbox.as_ref().unwrap().uid_validity())
            }
            Err(e) => {
                self.selected_mailbox = None;
                Err(e)
            }
        }
    }

    pub async fn idle(&mut self) {
        idle(&mut self.connection).await;
    }

    pub async fn fetch(&mut self, sequence_set: &SequenceSet) -> Vec<RemoteMail> {
        if self.selected_mailbox.is_some() {
            fetch(&mut self.connection, sequence_set).await
        } else {
            panic!("no mailbox selected");
        }
    }
}

impl<T> Repository for Session<T>
where
    T: SendCommand,
{
    fn validity(&self) -> &UidValidity {
        if let Some(mailbox) = &self.selected_mailbox {
            mailbox.uid_validity()
        } else {
            panic!("no mailbox selected");
        }
    }
}
