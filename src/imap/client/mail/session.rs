use futures::Stream;

use crate::{
    imap::connection::SendCommand,
    sync::{MailMetadata, Repository},
};

use super::{
    fetch::{fetch, fetch_metadata, RemoteMail, SequenceSet},
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

    pub fn fetch<'a>(
        &'a mut self,
        sequence_set: &SequenceSet,
    ) -> impl Stream<Item = RemoteMail> + use<'a, T> {
        if self.selected_mailbox.is_some() {
            fetch(&mut self.connection, sequence_set)
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

    fn list_all(&mut self) -> impl futures::Stream<Item = MailMetadata> {
        let validity = *(self.validity());
        fetch_metadata(
            &mut self.connection,
            &SequenceSet::range(0, validity.into()),
        )
    }

    fn get_all(&mut self) -> impl Stream<Item = impl crate::sync::Mail> {
        let validity = *(self.validity());
        fetch(
            &mut self.connection,
            &SequenceSet::range(0, validity.into()),
        )
    }
}
