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
}

impl<T: SendCommand> Session<T> {
    pub fn new(connection: T) -> Self {
        Self { connection }
    }

    pub async fn select(&mut self, mailbox: &str) -> Result<Mailbox, SelectError> {
        select(&mut self.connection, mailbox).await
    }

    pub async fn idle(&mut self) {
        idle(&mut self.connection).await;
    }

    pub fn fetch<'a>(
        &'a mut self,
        sequence_set: &SequenceSet,
    ) -> impl Stream<Item = RemoteMail> + use<'a, T> {
        fetch(&mut self.connection, sequence_set)
    }

    pub fn fetch_metadata(
        &mut self,
        sequence_set: &SequenceSet,
    ) -> impl futures::Stream<Item = crate::sync::MailMetadata> + use<'_, T> {
        fetch_metadata(&mut self.connection, sequence_set)
    }
}

impl<T: SendCommand> SendCommand for Session<T> {
    type Responses<'a>
        = T::Responses<'a>
    where
        Self: 'a;

    fn send(&mut self, command: String) -> Self::Responses<'_> {
        self.connection.send(command)
    }
}
