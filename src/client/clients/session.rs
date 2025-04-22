use futures::StreamExt;
use imap_proto::{
    MailboxDatum::{Exists, Flags, Recent},
    Response::{Data, Done, MailboxData},
    ResponseCode::{PermanentFlags, ReadOnly, UidNext, UidValidity, Unseen},
    Status::{Bad, No, Ok},
};
use log::{debug, trace, warn};
use thiserror::Error;

use crate::client::connection::Connection;

use super::mailbox::Mailbox;
use super::mailbox::MailboxBuilder;
use super::mailbox::UidBuilder;

pub struct Session {
    connection: Connection,
    selected_mailbox: Option<Mailbox>,
}

impl Session {
    pub(super) fn new(connection: Connection) -> Self {
        Self {
            connection,
            selected_mailbox: None,
        }
    }

    pub async fn select<'a>(&mut self, mailbox: &'a str) -> Result<(), SelectError<'a>> {
        let command = format!("SELECT {mailbox}");
        debug!("{}", command);
        let mut responses = self.connection.send(&command);
        let mut new_mailbox = MailboxBuilder::default();
        new_mailbox.name(mailbox.to_string());
        let mut uid = UidBuilder::default();
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
                        self.selected_mailbox = Some(
                            new_mailbox
                                .build()
                                .expect("mailbox data should be all available at this point"),
                        );
                        trace!("selected_mailbox = {:?}", self.selected_mailbox);
                    }
                    No => {
                        self.selected_mailbox = None;
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

        Result::Ok(())
    }
}

#[derive(Error, Debug)]
#[error("cannot select mailbox {mailbox}. Going back to unselected.")]
pub struct SelectError<'a> {
    mailbox: &'a str,
}
