use std::num::NonZeroU64;

use crate::{
    imap::RemoteMail,
    state::State,
    sync::{Change, Mail, MailMetadata, Repository},
};
use anyhow::Result;

use super::{
    Authenticator, SendCommand, SequenceSet, Session, Uid,
    client::{Mailbox, fetch, fetch_metadata},
    connection::ResponseData,
};

pub trait Connector {
    type Connection: SendCommand;

    async fn connect_to(host: &str, port: u16) -> (Self::Connection, ResponseData);
}

pub struct ImapRepository<'a, T: SendCommand> {
    session: Session<T>,
    mailbox: Mailbox,
    state: &'a State,
}

impl<'a, T: SendCommand> ImapRepository<'a, T> {
    pub async fn init<C: Connector<Connection = T>>(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        mailbox: &str,
        state: &'a State,
    ) -> Result<Self> {
        let (session, mailbox) = Self::setup::<C>(host, port, user, password, mailbox).await?;
        state.set_uid_validity(mailbox.uid_validity());
        Ok(Self {
            session,
            mailbox,
            state,
        })
    }
    pub async fn try_connect<C: Connector<Connection = T>>(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        mailbox: &str,
        state: &'a State,
    ) -> Result<Self> {
        let (session, mailbox) = Self::setup::<C>(host, port, user, password, mailbox).await?;
        assert_eq!(mailbox.uid_validity(), state.uid_validity());
        Ok(Self {
            session,
            mailbox,
            state,
        })
    }

    async fn setup<C: Connector<Connection = T>>(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        mailbox: &str,
    ) -> Result<(Session<T>, Mailbox)> {
        let (connection, _) = C::connect_to(host, port).await;
        let authenticator = Authenticator::new(user, password);
        let mut session = authenticator.authenticate(connection).await?;
        let mailbox = session.select(mailbox).await?;
        Ok((session, mailbox))
    }
}

impl<T: SendCommand> Repository for ImapRepository<'_, T> {
    fn validity(&self) -> super::UidValidity {
        self.mailbox.uid_validity()
    }

    fn list_all(&self) -> impl futures::Stream<Item = impl crate::sync::MailMetadata> {
        let sequence_set = SequenceSet::range(1, self.mailbox.uid_next().into());
        self.session.fetch_metadata(&sequence_set)
    }

    fn get_all(&self) -> impl futures::Stream<Item = impl crate::sync::Mail> {
        let sequence_set = SequenceSet::range(1, self.mailbox.uid_next().into());
        self.session
            .fetch(&SequenceSet::range(1, self.mailbox.uid_next().into()))
    }

    fn store(&self, mail: &impl crate::sync::Mail) -> Option<Uid> {
        todo!()
    }

    fn detect_changes(&self) -> Vec<Change<impl Mail>> {
        todo!();
        #[expect(unreachable_code)]
        Vec::<Change<RemoteMail>>::new()
    }
}
