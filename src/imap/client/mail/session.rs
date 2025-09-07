use std::num::NonZeroU64;

use futures::Stream;

use crate::{
    imap::{client::mail::fetch::RemoteMailMetadata, connection::SendCommand},
    sync::{MailMetadata, Repository},
};

use super::{
    fetch::{RemoteMail, SequenceSet, fetch, fetch_metadata},
    idle::idle,
    mailbox::{Mailbox, UidValidity},
    select::{SelectError, select},
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
        &'a self,
        sequence_set: &SequenceSet,
    ) -> impl Stream<Item = RemoteMail> + use<'a, T> {
        fetch(&self.connection, sequence_set)
    }

    pub fn fetch_metadata<'a>(
        &'a self,
        sequence_set: &SequenceSet,
    ) -> impl futures::Stream<Item = RemoteMailMetadata> + use<'a, T> {
        fetch_metadata(&self.connection, sequence_set)
    }
}

impl<T: SendCommand> SendCommand for Session<T> {
    type Responses<'a>
        = T::Responses<'a>
    where
        Self: 'a;

    fn send(&self, command: String) -> Self::Responses<'_> {
        self.connection.send(command)
    }
}
