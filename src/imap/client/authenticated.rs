use std::ops::RangeInclusive;

use crossfire::{MAsyncTx, mpsc::Array};

use crate::{
    imap::{
        RemoteChanges, RemoteMailMetadata, Selection,
        client::{
            SelectedClient,
            capability::{Capabilities, Capability},
            encoding,
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
    pub fn new(connection: Connection, capabilities: Capabilities) -> Self {
        assert!(
            capabilities.contains(Capability::Condstore),
            "server should support CONDSTORE capability"
        );
        assert!(
            capabilities.contains(Capability::Enable),
            "server should support ENABLE capability"
        );
        assert!(
            capabilities.contains(Capability::QResync),
            "server should support QRESYNC capability"
        );
        Self {
            connection,
            capabilities,
        }
    }

    pub async fn select(mut self, task_tx: MAsyncTx<Array<Task>>, mailbox: &str) -> Selection {
        self.enable_qresync().await;
        let command = format!("SELECT {} (CONDSTORE)", encoding::mailbox(mailbox));

        self.do_select(task_tx, &command, None).await
    }

    pub async fn qresync_select(
        mut self,
        task_tx: MAsyncTx<Array<Task>>,
        mailbox: &str,
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
    ) -> Selection {
        self.enable_qresync().await;
        let command = format!(
            "SELECT {} (QRESYNC ({uid_validity} {highest_modseq}))",
            encoding::mailbox(mailbox)
        );

        self.do_select(task_tx, &command, Some(uid_validity)).await
    }

    async fn enable_qresync(&mut self) {
        let command = "ENABLE QRESYNC";
        log::debug!("{command}");
        self.connection
            .send(command.into(), &mut Unhandled)
            .await
            .expect("enabling qresync should succeed");
    }

    async fn do_select(
        mut self,
        task_tx: MAsyncTx<Array<Task>>,
        command: &str,
        cached_uid_validity: Option<UidValidity>,
    ) -> Selection {
        log::debug!("{command}");

        let mut handler = SelectHandler {
            cached_uid_validity,
            capabilities: &mut self.capabilities,
            new_mailbox: MailboxMetadataBuilder::default(),
            updates: Vec::new(),
            deletions: None,
            exists: None,
        };
        self.connection
            .send(command.into(), &mut handler)
            .await
            .expect("selecting a mailbox should succeed");
        let SelectHandler {
            new_mailbox,
            updates,
            deletions,
            exists,
            ..
        } = handler;

        let mailbox_data = new_mailbox
            .build()
            .expect("mailbox data should be all available at this point");
        log::trace!("selected_mailbox = {mailbox_data:?}");
        log::trace!("mail updates = {updates:?}");
        log::trace!("mail deletions = {deletions:?}");

        let client = SelectedClient::new(
            self.connection,
            &self.capabilities,
            task_tx,
            exists.expect("server should send EXISTS on SELECT"),
        );

        Selection {
            client,
            remote_changes: RemoteChanges { updates, deletions },
            mailbox_data,
        }
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
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>> {
        std::future::ready(self.handle_response(response))
    }
}

impl SelectHandler<'_> {
    fn add_deletions(&mut self, uids: &Vec<RangeInclusive<u32>>) {
        let vanished =
            SequenceSet::try_from(uids).expect("received ranges should start with valid uid");
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
    }

    fn handle_response(&mut self, response: ResponseData) -> Option<ResponseData> {
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
                    let validity = validity
                        .try_into()
                        .expect("received uid validity should be spec compliant");
                    if let Some(cached) = self.cached_uid_validity {
                        assert_eq!(cached, validity);
                    }
                    self.new_mailbox.uid_validity(validity);
                }
                imap_proto::ResponseCode::HighestModSeq(modseq) => {
                    self.new_mailbox.highest_modseq(
                        modseq
                            .try_into()
                            .expect("Project expects RFC 4551 compatible IMAP server"),
                    );
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
                self.updates
                    .push(RemoteMailMetadata::from_fetch(attributes));
            }
            imap_proto::Response::Vanished { earlier, uids } => {
                debug_assert!(
                    earlier,
                    "earlier should always be true during select (see https://datatracker.ietf.org/doc/html/rfc7162#section-3.2.10)"
                );
                self.add_deletions(uids);
            }
            _ => return Some(response),
        }
        None
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

        assert_none!(
            handler.handle_response(ResponseData::from_raw(b"* VANISHED (EARLIER) 1:3\r\n"))
        );
        assert_none!(
            handler.handle_response(ResponseData::from_raw(b"* VANISHED (EARLIER) 7\r\n"))
        );

        let deletions: Vec<u32> = assert_some!(handler.deletions)
            .iter()
            .map(u32::from)
            .collect();
        assert_eq!(deletions, vec![1, 2, 3, 7]);
    }
}
