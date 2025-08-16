use futures::StreamExt;
use imap_proto::{MailboxDatum, Status};
use log::{debug, trace, warn};

use crate::imap::connection::{ContinuationCommand as _, SendCommand};

pub async fn idle(connection: &mut impl SendCommand) -> IdleData {
    let command = "IDLE";
    debug!("{command}");
    let mut responses = connection.send(command.into());
    let mut idle_data = IdleData::default();
    while let Some(response) = responses.next().await {
        match response.parsed() {
            imap_proto::Response::Continue { .. } => {}
            imap_proto::Response::Done {
                status: Status::Ok, ..
            } => {
                trace!("IDLE stopped");
            }
            imap_proto::Response::Expunge(expunge) => {
                idle_data.expunge = *expunge;
            }
            imap_proto::Response::MailboxData(MailboxDatum::Exists(exists)) => {
                idle_data.exists = *exists;
                debug!("Number of mails on server changed. Quitting IDLE for fetch");
                responses.send("DONE").await;
            }
            imap_proto::Response::MailboxData(MailboxDatum::Recent(recent)) => {
                idle_data.recent = *recent;
            }
            imap_proto::Response::Data {
                status: Status::Ok,
                code: None,
                information,
            } => {
                trace!("Received ok with information: {information:?}");
            }
            response => {
                warn!("unhandled response to idle: {response:?}");
            }
        }
    }

    idle_data
}

#[derive(Debug, Default)]
pub struct IdleData {
    exists: u32,
    expunge: u32,
    recent: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::borrow::Cow;

    use imap_proto::{AttributeValue, MailboxDatum, RequestId, Response};

    use crate::imap::connection::mock_connection::MockConnection;

    #[tokio::test]
    async fn should_return_data() {
        let responses = [
            vec![
                Response::Continue {
                    code: None,
                    information: Some(Cow::Borrowed("idling")),
                },
                Response::MailboxData(MailboxDatum::Exists(6081)),
                Response::Fetch(
                    6081,
                    vec![AttributeValue::Flags(vec![Cow::Borrowed("Junk")])],
                ),
                Response::MailboxData(MailboxDatum::Recent(1)),
            ],
            vec![Response::Done {
                tag: RequestId("0002".to_string()),
                status: imap_proto::Status::Ok,
                code: None,
                information: Some(Cow::Borrowed(
                    "Idle completed (17.100 + 17.096 + 17.099 secs).",
                )),
            }],
        ];
        let mut connection = MockConnection::new(responses);

        let idle_data = idle(&mut connection).await;

        assert!(matches!(
            idle_data,
            IdleData {
                exists: 6081,
                expunge: 0,
                recent: 1,
            }
        ));
    }
}
