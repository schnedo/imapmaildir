use futures::StreamExt as _;
use imap_proto::{
    MailboxDatum::{Exists, Flags, Recent},
    Response::{Data, Done, MailboxData},
    ResponseCode::{PermanentFlags, ReadOnly, UidNext, UidValidity, Unseen},
    Status::{Bad, No, Ok},
};
use log::{debug, trace, warn};
use thiserror::Error;

use crate::imap::{
    client::mail::mailbox::{MailboxBuilder, UidBuilder},
    connection::SendCommand,
};

use super::mailbox::Mailbox;

pub async fn select<'a>(
    connection: &mut impl SendCommand,
    mailbox: &'a str,
) -> Result<Mailbox, SelectError<'a>> {
    let command = format!("SELECT {mailbox}");
    debug!("{}", command);
    let mut responses = connection.send(&command);
    let mut new_mailbox = MailboxBuilder::default();
    new_mailbox.name(mailbox.to_string());
    let mut uid = UidBuilder::default();
    while let Some(response) = responses.next().await {
        dbg!(response.parsed());
        match response.parsed() {
            MailboxData(mailbox_datum) => match mailbox_datum {
                Flags(cows) => {
                    let mut flags = Vec::with_capacity(cows.len());
                    for cow in cows {
                        flags.push(cow.to_string());
                    }
                    new_mailbox.flags(flags);
                }
                Exists(exists) => {
                    new_mailbox.exists(*exists);
                }
                Recent(recent) => {
                    new_mailbox.recent(*recent);
                }
                _ => {
                    warn!("ignoring unknown mailbox data response to SELECT");
                    trace!("{:?}", mailbox_datum);
                }
            },
            Data {
                status: Ok,
                code: None,
                information: Some(information),
            } => {
                debug!("{}", information);
            }
            Data {
                status: Ok,
                code: Some(code),
                information,
            } => match code {
                Unseen(unseen) => {
                    new_mailbox.unseen(*unseen);
                }
                PermanentFlags(cows) => {
                    let mut flags = Vec::with_capacity(cows.len());
                    for cow in cows {
                        flags.push(cow.to_string());
                    }
                    new_mailbox.permanent_flags(flags);
                }
                UidNext(next) => {
                    uid.next(*next);
                }
                UidValidity(validity) => {
                    uid.validity(*validity);
                }
                _ => {
                    warn!("ignoring unknown data response to SELECT");
                    if let Some(information) = information {
                        warn!("{}", information);
                    }
                    trace!("{:?}", code);
                }
            },
            Done { status, code, .. } => match status {
                Ok => {
                    if let Some(ReadOnly) = code {
                        new_mailbox.readonly(true);
                    }
                    if let Result::Ok(uid) = uid.build() {
                        new_mailbox.uid(uid);
                    }
                    break;
                }
                No => {
                    return Err(SelectError { mailbox });
                }
                Bad => panic!("Bad status response to select. This is a code issue."),
                _ => panic!("select status can only ever be Ok, No or Bad"),
            },
            _ => {
                warn!("ignoring unknown response to SELECT");
                trace!("{:?}", response.parsed());
            }
        }
    }

    let selected_mailbox = new_mailbox
        .build()
        .expect("mailbox data should be all available at this point");
    trace!("selected_mailbox = {:?}", selected_mailbox);
    Result::Ok(selected_mailbox)
}

#[derive(Error, Debug)]
#[error("cannot select mailbox {mailbox}. Going back to unselected.")]
pub struct SelectError<'a> {
    mailbox: &'a str,
}

#[cfg(test)]
mod tests {
    use crate::imap::connection::mock_connection::MockConnection;

    use super::*;

    use std::borrow::Cow;

    use imap_proto::*;

    #[tokio::test]
    async fn should() {
        let exists = 6084;
        let recent = 4;
        let uid_validity = 1234214;
        let uid_next = 4321;
        let responses = [
            Response::MailboxData(Flags(vec![
                Cow::Borrowed("\\Answered"),
                Cow::Borrowed("\\Flagged"),
                Cow::Borrowed("\\Deleted"),
                Cow::Borrowed("\\Seen"),
                Cow::Borrowed("\\Draft"),
            ])),
            Response::Data {
                status: Ok,
                code: Some(PermanentFlags(vec![
                    Cow::Borrowed("\\Answered"),
                    Cow::Borrowed("\\Flagged"),
                    Cow::Borrowed("\\Deleted"),
                    Cow::Borrowed("\\Seen"),
                    Cow::Borrowed("\\Draft"),
                    Cow::Borrowed("\\*"),
                ])),
                information: Some(Cow::Borrowed("Flags permitted.")),
            },
            Response::MailboxData(Exists(exists)),
            Response::MailboxData(Recent(recent)),
            Response::Data {
                status: Ok,
                code: Some(UidValidity(uid_validity)),
                information: Some(Cow::Borrowed("UIDs valid")),
            },
            Response::Data {
                status: Ok,
                code: Some(UidNext(uid_next)),
                information: Some(Cow::Borrowed("Predicted next UID")),
            },
            Response::Data {
                status: Ok,
                code: Some(ResponseCode::HighestModSeq(70500)),
                information: Some(Cow::Borrowed("")),
            },
            Response::Done {
                tag: RequestId("0001".to_string()),
                status: Ok,
                code: Some(ResponseCode::ReadWrite),
                information: Some(Cow::Borrowed("Select completed (0.001 + 0.000 secs).")),
            },
        ];
        let mut mock_connection = MockConnection::new(responses);

        let mailbox_name = "foo";

        let result = select(&mut mock_connection, mailbox_name).await;

        assert!(result.is_ok());
        let mailbox = result.unwrap();
        assert_eq!(mailbox.name(), mailbox_name);
        assert_eq!(mailbox.readonly(), &false);
        assert_eq!(
            mailbox.flags(),
            &vec!["\\Answered", "\\Flagged", "\\Deleted", "\\Seen", "\\Draft",]
        );
        assert_eq!(mailbox.exists(), &exists);
        assert_eq!(mailbox.recent(), &recent);
        assert!(mailbox.unseen().is_none());
        assert_eq!(
            mailbox.permanent_flags(),
            &vec![
                "\\Answered",
                "\\Flagged",
                "\\Deleted",
                "\\Seen",
                "\\Draft",
                "\\*",
            ]
        );
        assert!(mailbox.uid().is_some());
        if let Some(uid) = mailbox.uid() {
            assert_eq!(uid.validity(), &uid_validity);
            assert_eq!(uid.next(), &uid_next);
        }
    }
}
