use std::{borrow::Cow, num::NonZeroU64};

use futures::{Stream, StreamExt};
use imap_proto::{Capability, Response, Status};
use log::{trace, warn};
use rustix::path::Arg;

use crate::{
    imap::{
        client::mail::{fetch::RemoteMailMetadata, qresync_select},
        connection::SendCommand,
    },
    state::ModSeq,
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

    pub async fn qresync_select(
        &mut self,
        mailbox: &str,
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
    ) -> Result<Mailbox, SelectError> {
        qresync_select(&mut self.connection, mailbox, uid_validity, highest_modseq).await
    }

    pub async fn enable_qresync(&mut self) -> Result<(), &'static str> {
        let command = "ENABLE QRESYNC";
        let mut responses = self.connection.send(command.to_string());

        while let Some(response) = responses.next().await {
            match response.parsed() {
                Response::Capabilities(cows) => {
                    trace!("enabled {cows:?}");
                }
                Response::Done {
                    status: Status::Ok, ..
                } => {}
                Response::Done { information, .. } => {
                    if let Some(information) = information {
                        panic!("{information}");
                    } else {
                        panic!("bad FETCH");
                    }
                }
                _ => {
                    warn!("ignoring unknown response to ENABLE");
                    trace!("{:?}", response.parsed());
                }
            }
        }
        Ok(())
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
