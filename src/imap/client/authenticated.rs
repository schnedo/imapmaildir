use std::ops::RangeInclusive;

use crossfire::{MAsyncTx, mpsc::Array};

use crate::{
    imap::{
        RemoteChanges, RemoteMailMetadata, Selection,
        client::{
            Error, SelectedClient,
            capability::{Capabilities, Capability},
            encoding,
            fetch::FetchAttributes,
        },
        transport::{Connection, ResponseData, ResponseHandler, Unhandled},
    },
    repository::{MailboxMetadataBuilder, ModSeq, SequenceSet, SequenceSetBuilder, UidValidity},
    sync::Task,
};

pub struct AuthenticatedClient {
    connection: Connection,
    capabilities: Capabilities,
}

impl AuthenticatedClient {
    pub fn new(connection: Connection, capabilities: Capabilities) -> Result<Self, Error> {
        for capability in [
            Capability::Condstore,
            Capability::Enable,
            Capability::QResync,
        ] {
            if !capabilities.contains(capability) {
                return Err(Error::MissingCapability(capability));
            }
        }

        Ok(Self {
            connection,
            capabilities,
        })
    }

    pub fn require_idle(&self) -> Result<(), Error> {
        if self.capabilities.contains(Capability::Idle) {
            Ok(())
        } else {
            Err(Error::MissingCapability(Capability::Idle))
        }
    }

    pub async fn select(
        mut self,
        task_tx: MAsyncTx<Array<Task>>,
        mailbox: &str,
    ) -> Result<Selection, Error> {
        self.enable_qresync().await?;
        let command = format!("SELECT {} (CONDSTORE)", encoding::mailbox(mailbox));

        self.do_select(task_tx, &command, None).await
    }

    pub async fn qresync_select(
        mut self,
        task_tx: MAsyncTx<Array<Task>>,
        mailbox: &str,
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
    ) -> Result<Selection, Error> {
        self.enable_qresync().await?;
        let command = format!(
            "SELECT {} (QRESYNC ({uid_validity} {highest_modseq}))",
            encoding::mailbox(mailbox)
        );

        self.do_select(task_tx, &command, Some(uid_validity)).await
    }

    async fn enable_qresync(&mut self) -> Result<(), Error> {
        let command = "ENABLE QRESYNC";
        log::debug!("{command}");
        self.connection.send(command.into(), &mut Unhandled).await?;

        Ok(())
    }

    async fn do_select(
        mut self,
        task_tx: MAsyncTx<Array<Task>>,
        command: &str,
        cached_uid_validity: Option<UidValidity>,
    ) -> Result<Selection, Error> {
        log::debug!("{command}");

        let mut handler = SelectHandler {
            cached_uid_validity,
            capabilities: &mut self.capabilities,
            new_mailbox: MailboxMetadataBuilder::default(),
            updates: Vec::new(),
            deletions: None,
            exists: None,
        };
        self.connection.send(command.into(), &mut handler).await?;
        let SelectHandler {
            new_mailbox,
            updates,
            deletions,
            exists,
            ..
        } = handler;

        let mailbox_data = new_mailbox.build().map_err(Error::IncompleteMailboxData)?;
        log::trace!("selected_mailbox = {mailbox_data:?}");
        log::trace!("mail updates = {updates:?}");
        log::trace!("mail deletions = {deletions:?}");

        let client = SelectedClient::new(
            self.connection,
            &self.capabilities,
            task_tx,
            exists.ok_or(Error::MissingExists)?,
        )?;

        Ok(Selection {
            client,
            remote_changes: RemoteChanges { updates, deletions },
            mailbox_data,
        })
    }
}

struct SelectHandler<'a> {
    cached_uid_validity: Option<UidValidity>,
    capabilities: &'a mut Capabilities,
    new_mailbox: MailboxMetadataBuilder,
    updates: Vec<RemoteMailMetadata>,
    deletions: Option<SequenceSet>,
    exists: Option<u32>,
}

impl ResponseHandler for SelectHandler<'_> {
    type Error = Error;

    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>> {
        std::future::ready(self.handle_response(response))
    }
}

impl SelectHandler<'_> {
    fn add_deletions(&mut self, uids: &Vec<RangeInclusive<u32>>) -> Result<(), Error> {
        let vanished = SequenceSet::try_from(uids)?;
        self.deletions = Some(if let Some(deletions) = self.deletions.take() {
            let mut builder = SequenceSetBuilder::default();
            for uid in deletions.iter().chain(vanished.iter()) {
                builder.add(uid);
            }
            builder
                .build()
                .expect("merged deletions should not be empty")
        } else {
            vanished
        });

        Ok(())
    }

    fn handle_response(&mut self, response: ResponseData) -> Result<Option<ResponseData>, Error> {
        match response.parsed() {
            imap_proto::Response::MailboxData(mailbox_datum) => match mailbox_datum {
                imap_proto::MailboxDatum::Exists(exists) => {
                    log::trace!("mailbox contains {exists} messages");
                    self.exists = Some(*exists);
                }
                imap_proto::MailboxDatum::Flags(flags) => {
                    log::trace!("not handling MailboxData response Flags {flags:?}");
                }
                imap_proto::MailboxDatum::Recent(recent) => {
                    log::trace!("not handling MailboxData response Recent {recent:?}");
                }
                _ => {
                    log::warn!(
                        "ignoring unknown mailbox data response to SELECT {mailbox_datum:?}"
                    );
                }
            },
            imap_proto::Response::Capabilities(caps) => {
                for cap in caps {
                    match cap {
                        imap_proto::Capability::Atom(_) => self.capabilities.insert(cap),
                        _ => log::warn!("unexpected capability respone {cap:?}"),
                    }
                }
                log::trace!("updated capabilities to {:?}", self.capabilities);
            }
            imap_proto::Response::Data {
                status: imap_proto::Status::Ok,
                outcome:
                    imap_proto::Outcome {
                        code: None,
                        information: Some(information),
                    },
            } => {
                log::debug!("{information}");
            }
            imap_proto::Response::Data {
                status: imap_proto::Status::Ok,
                outcome:
                    imap_proto::Outcome {
                        code: Some(code),
                        information,
                    },
            } => match code {
                imap_proto::ResponseCode::UidValidity(validity) => {
                    let validity = validity.try_into().map_err(Error::InvalidUidValidity)?;
                    if let Some(cached) = self.cached_uid_validity
                        && cached != validity
                    {
                        return Err(Error::UidValidityMismatch {
                            local: cached,
                            remote: validity,
                        });
                    }
                    self.new_mailbox.uid_validity(validity);
                }
                imap_proto::ResponseCode::HighestModSeq(modseq) => {
                    self.new_mailbox
                        .highest_modseq(modseq.try_into().map_err(Error::InvalidModSeq)?);
                }
                imap_proto::ResponseCode::PermanentFlags(flags) => {
                    log::trace!("not handling Data response PermanentFlags {flags:?}");
                }
                imap_proto::ResponseCode::UidNext(uid_next) => {
                    log::trace!("not handling Data response UidNext {uid_next:?}");
                }
                imap_proto::ResponseCode::Unseen(unseen) => {
                    log::trace!("not handling Data response Unseen {unseen:?}");
                }
                _ => {
                    log::warn!("ignoring unknown data response to SELECT");
                    if let Some(information) = information {
                        log::warn!("{information}");
                    }
                    log::trace!("{code:?}");
                }
            },
            imap_proto::Response::Fetch(_, attributes) => {
                log::trace!("handling fetch with attributes {attributes:?}");
                self.updates.push(
                    FetchAttributes::parse(attributes)
                        .metadata()?
                        .ok_or(Error::IncompleteMetadata)?,
                );
            }
            imap_proto::Response::Vanished { earlier, uids } => {
                debug_assert!(
                    earlier,
                    "earlier should always be true during select (see https://datatracker.ietf.org/doc/html/rfc7162#section-3.2.10)"
                );
                self.add_deletions(uids)?;
            }
            _ => return Ok(Some(response)),
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;

    use super::*;

    #[test]
    fn test_select_handler_merges_vanished_responses() {
        let mut capabilities = Capabilities::default();
        let mut handler = SelectHandler {
            cached_uid_validity: None,
            capabilities: &mut capabilities,
            new_mailbox: MailboxMetadataBuilder::default(),
            updates: Vec::new(),
            deletions: None,
            exists: None,
        };

        assert_none!(assert_ok!(handler.handle_response(ResponseData::from_raw(
            b"* VANISHED (EARLIER) 1:3\r\n"
        ))));
        assert_none!(assert_ok!(handler.handle_response(ResponseData::from_raw(
            b"* VANISHED (EARLIER) 7\r\n"
        ))));

        let deletions: Vec<u32> = assert_some!(handler.deletions)
            .iter()
            .map(u32::from)
            .collect();
        assert_eq!(deletions, vec![1, 2, 3, 7]);
    }
}
