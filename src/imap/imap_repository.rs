use crate::sync::Repository;
use anyhow::Result;

use super::{connection::ResponseData, Client, SendCommand, Session};

pub trait Connector {
    type Command: SendCommand;

    async fn connect_to(host: &str, port: u16) -> (Self::Command, ResponseData);
}

pub struct ImapRepository<T: SendCommand> {
    session: Session<T>,
}

impl<T: Connector> ImapRepository<T::Command> {
    pub async fn try_connect(host: &str, port: u16, user: &str, password: &str) -> Result<Self> {
        let (connection, _) = T::connect_to(host, port).await;
        let client = Client::new(connection);
        let session = client.login(user, password).await?;
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
