use std::num::NonZeroU64;

use futures::StreamExt as _;
use imap_proto::{
    MailboxDatum::{Exists, Flags, Recent},
    Response::{Data, Done, MailboxData},
    ResponseCode, Status,
};
use log::{debug, trace, warn};
use thiserror::Error;

use crate::{
    imap::{
        UidValidity,
        client::mail::mailbox::MailboxBuilder,
        connection::{self, SendCommand},
    },
    state::ModSeq,
};

use super::mailbox::Mailbox;

pub async fn select(
    connection: &mut impl SendCommand,
    mailbox: &str,
) -> Result<Mailbox, SelectError> {
    let command = format!("SELECT {mailbox}");
    do_select(connection, mailbox, command).await
}

// todo: add known uids and message number to uid mapping (see rfc 7162)
pub async fn qresync_select(
    connection: &mut impl SendCommand,
    mailbox: &str,
    uid_validity: UidValidity,
    highest_modseq: ModSeq,
) -> Result<Mailbox, SelectError> {
    let command = format!("SELECT {mailbox} (QRESYNC ({uid_validity} {highest_modseq}))");
    do_select(connection, mailbox, command).await
}

async fn do_select(
    connection: &mut impl SendCommand,
    mailbox: &str,
    command: String,
) -> Result<Mailbox, SelectError> {
    debug!("{command}");
    let mut responses = connection.send(command);
    let mut new_mailbox = MailboxBuilder::default();
    new_mailbox.name(mailbox.to_string());

    while let Some(response) = responses.next().await {
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
                    trace!("{mailbox_datum:?}");
                }
            },
            Data {
                status: Status::Ok,
                code: None,
                information: Some(information),
            } => {
                debug!("{information}");
            }
            Data {
                status: Status::Ok,
                code: Some(code),
                information,
            } => match code {
                ResponseCode::Unseen(unseen) => {
                    new_mailbox.unseen(*unseen);
                }
                ResponseCode::PermanentFlags(cows) => {
                    let mut flags = Vec::with_capacity(cows.len());
                    for cow in cows {
                        flags.push(cow.to_string());
                    }
                    new_mailbox.permanent_flags(flags);
                }
                ResponseCode::UidNext(next) => {
                    new_mailbox
                        .uid_next(next.try_into().expect("server should send valid uidnext"));
                }
                ResponseCode::UidValidity(validity) => {
                    new_mailbox.uid_validity((*validity).into());
                }
                ResponseCode::HighestModSeq(modseq) => {
                    new_mailbox.highest_modseq(
                        (*modseq)
                            .try_into()
                            .expect("Project expects RFC 4551 compatible IMAP server"),
                    );
                }
                _ => {
                    warn!("ignoring unknown data response to SELECT");
                    if let Some(information) = information {
                        warn!("{information}");
                    }
                    trace!("{code:?}");
                }
            },
            Done { status, code, .. } => match status {
                Status::Ok => {
                    if let Some(ResponseCode::ReadOnly) = code {
                        new_mailbox.readonly(true);
                    }
                    break;
                }
                Status::No => {
                    return Err(SelectError {
                        mailbox: mailbox.to_string(),
                    });
                }
                Status::Bad => panic!("Bad status response to select. This is a code issue."),
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
    trace!("selected_mailbox = {selected_mailbox:?}");
    Result::Ok(selected_mailbox)
}

#[derive(Error, Debug)]
#[error("cannot select mailbox {mailbox}. Going back to unselected.")]
pub struct SelectError {
    mailbox: String,
}

#[cfg(test)]
mod tests {
    use crate::imap::{client::mail::mailbox, connection::mock_connection::MockConnection};

    use super::*;

    use std::borrow::Cow;

    use imap_proto::*;

    #[tokio::test]
    async fn should_return_data() {
        let exists = 6084;
        let recent = 4;
        let uid_validity = 1_234_214;
        let uid_next = 4321;
        let expected_highest_modseq = 70500;

        let responses = [[
            Response::MailboxData(Flags(vec![
                Cow::Borrowed("\\Answered"),
                Cow::Borrowed("\\Flagged"),
                Cow::Borrowed("\\Deleted"),
                Cow::Borrowed("\\Seen"),
                Cow::Borrowed("\\Draft"),
            ])),
            Response::Data {
                status: Status::Ok,
                code: Some(ResponseCode::PermanentFlags(vec![
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
                status: Status::Ok,
                code: Some(ResponseCode::UidValidity(uid_validity)),
                information: Some(Cow::Borrowed("UIDs valid")),
            },
            Response::Data {
                status: Status::Ok,
                code: Some(ResponseCode::UidNext(uid_next)),
                information: Some(Cow::Borrowed("Predicted next UID")),
            },
            Response::Data {
                status: Status::Ok,
                code: Some(ResponseCode::HighestModSeq(expected_highest_modseq)),
                information: Some(Cow::Borrowed("")),
            },
            Response::Done {
                tag: RequestId("0001".to_string()),
                status: Status::Ok,
                code: Some(ResponseCode::ReadWrite),
                information: Some(Cow::Borrowed("Select completed (0.001 + 0.000 secs).")),
            },
        ]];
        let mut mock_connection = MockConnection::new(responses);

        let mailbox_name = "foo";

        let result = select(&mut mock_connection, mailbox_name).await;

        assert!(result.is_ok());
        let mailbox = result.unwrap();
        assert_eq!(mailbox.name(), mailbox_name);
        assert_eq!(
            mailbox.highest_modseq(),
            NonZeroU64::new(expected_highest_modseq).expect("HighestModSeq should be non zero")
        );
        assert!(!mailbox.readonly());
        assert_eq!(
            mailbox.flags(),
            &vec!["\\Answered", "\\Flagged", "\\Deleted", "\\Seen", "\\Draft",]
        );
        assert_eq!(mailbox.exists(), exists);
        assert_eq!(mailbox.recent(), recent);
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
        assert_eq!(
            mailbox.uid_validity(),
            mailbox::UidValidity::new(uid_validity)
        );
    }
}
