use futures::StreamExt;
use imap_proto::{MailboxDatum, Status};
use log::{debug, trace, warn};

use crate::imap::connection::{ContinuationCommand as _, SendCommand};

pub async fn idle(connection: &mut impl SendCommand) {
    let command = "IDLE";
    debug!("{command}");
    let mut responses = connection.send(command);
    while let Some(response) = responses.next().await {
        let mut idle_data = IdleData::default();
        match response.parsed() {
            imap_proto::Response::Continue { .. } => {}
            imap_proto::Response::Done {
                status: Status::Ok, ..
            } => {
                trace!("IDLE stopped");
                return;
            }
            imap_proto::Response::Expunge(expunge) => {
                idle_data.expunge = *expunge;
            }
            imap_proto::Response::MailboxData(MailboxDatum::Exists(exists)) => {
                idle_data.exists = *exists;
                debug!("New mails on server. Quitting IDLE for fetch");
                responses.send("DONE").await;
            }
            response => {
                warn!("unhandled response to idle: {response:?}");
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct IdleData {
    exists: u32,
    expunge: u32,
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use imap_proto::{AttributeValue, MailboxDatum, Response};

    #[tokio::test]
    async fn foo() {
        let foo = [
            Response::Continue {
                code: None,
                information: Some(Cow::Borrowed("idling")),
            },
            Response::MailboxData(MailboxDatum::Exists(6081)),
            Response::Fetch(
                6081,
                vec![AttributeValue::Flags(vec![Cow::Borrowed("Junk")])],
            ),
        ];
    }
}
