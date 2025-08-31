use crate::sync::Repository;
use anyhow::Result;

use super::{connection::ResponseData, Authenticator, SendCommand, Session};

pub trait Connector {
    type Connection: SendCommand;

    async fn connect_to(host: &str, port: u16) -> (Self::Connection, ResponseData);
}

pub struct ImapRepository<T: SendCommand> {
    session: Session<T>,
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
        let client = Authenticator::new(connection);
        let mut session = client.login(user, password).await?;
        session.select(mailbox).await?;
        Ok(Self { session })
    }
}

impl<T: SendCommand> Repository for ImapRepository<T> {
    fn validity(&self) -> super::UidValidity {
        self.session.validity()
    }

    fn list_all(&mut self) -> impl futures::Stream<Item = crate::sync::MailMetadata> {
        self.session.list_all()
    }

    fn get_all(&mut self) -> impl futures::Stream<Item = impl crate::sync::Mail> {
        self.session.get_all()
    }

    fn store(&self, mail: &impl crate::sync::Mail) {
        self.session.store(mail);
    }
}
