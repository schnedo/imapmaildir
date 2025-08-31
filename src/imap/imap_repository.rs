use crate::sync::Repository;
use anyhow::Result;

use super::{
    Authenticator, SendCommand, SequenceSet, Session,
    client::{Mailbox, fetch, fetch_metadata},
    connection::ResponseData,
};

pub trait Connector {
    type Connection: SendCommand;

    async fn connect_to(host: &str, port: u16) -> (Self::Connection, ResponseData);
}

pub struct ImapRepository<T: SendCommand> {
    session: Session<T>,
    mailbox: Mailbox,
}

impl<T: SendCommand> ImapRepository<T> {
    pub async fn try_connect<C: Connector<Connection = T>>(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        mailbox: &str,
    ) -> Result<Self> {
        let (connection, _) = C::connect_to(host, port).await;
        let authenticator = Authenticator::new(user, password);
        let mut session = authenticator.authenticate(connection).await?;
        let mailbox = session.select(mailbox).await?;
        Ok(Self { session, mailbox })
    }
}

impl<T: SendCommand> Repository for ImapRepository<T> {
    fn validity(&self) -> super::UidValidity {
        self.mailbox.uid_validity()
    }

    fn list_all(&mut self) -> impl futures::Stream<Item = crate::sync::MailMetadata> {
        let sequence_set = SequenceSet::range(1, self.mailbox.uid_next().into());
        self.session.fetch_metadata(&sequence_set)
    }

    fn get_all(&mut self) -> impl futures::Stream<Item = impl crate::sync::Mail> {
        let sequence_set = SequenceSet::range(1, self.mailbox.uid_next().into());
        self.session
            .fetch(&SequenceSet::range(1, self.mailbox.uid_next().into()))
    }

    fn store(&self, mail: &impl crate::sync::Mail) {
        todo!()
    }
}
