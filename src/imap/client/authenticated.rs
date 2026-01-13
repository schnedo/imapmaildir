use log::{debug, trace, warn};
use tokio::sync::mpsc;

use crate::{
    imap::{
        RemoteChanges, RemoteMailMetadata, RemoteMailMetadataBuilder, Selection,
        client::{
            SelectedClient,
            capability::{Capabilities, Capability},
        },
        transport::{Connection, ResponseData},
    },
    repository::{Flag, MailboxMetadataBuilder, ModSeq, SequenceSet, UidValidity},
    sync::Task,
};

pub struct AuthenticatedClient {
    connection: Connection,
    untagged_response_receiver: mpsc::Receiver<ResponseData>,
    capabilities: Capabilities,
}

impl AuthenticatedClient {
    pub fn new(
        connection: Connection,
        capabilities: Capabilities,
        untagged_response_receiver: mpsc::Receiver<ResponseData>,
    ) -> Self {
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
            untagged_response_receiver,
            capabilities,
        }
    }

    pub async fn select(self, task_tx: mpsc::Sender<Task>, mailbox: &str) -> Selection {
        let command = format!("SELECT {mailbox} (CONDSTORE)");

        self.do_select(task_tx, &command, None).await
    }

    // todo: add optional qresync parameters
    pub async fn qresync_select(
        mut self,
        task_tx: mpsc::Sender<Task>,
        mailbox: &str,
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
    ) -> Selection {
        let command = "ENABLE QRESYNC";
        debug!("{command}");
        self.connection
            .send(command.into())
            .await
            .expect("enabling qresync should succeed");
        let command = format!("SELECT {mailbox} (QRESYNC ({uid_validity} {highest_modseq}))");

        self.do_select(task_tx, &command, Some(uid_validity)).await
    }

    #[expect(clippy::too_many_lines)]
    async fn do_select(
        mut self,
        task_tx: mpsc::Sender<Task>,
        command: &str,
        cached_uid_validity: Option<UidValidity>,
    ) -> Selection {
        debug!("{command}");
        self.connection
            .send(command.into())
            .await
            .expect("selecting a mailbox should succeed");

        let mut new_mailbox = MailboxMetadataBuilder::default();

        let mut updates: Vec<RemoteMailMetadata> = Vec::new();
        let mut deletions = None;

        while let Ok(response) = self.untagged_response_receiver.try_recv() {
            match response.parsed() {
                imap_proto::Response::MailboxData(mailbox_datum) => match mailbox_datum {
                    imap_proto::MailboxDatum::Exists(exists) => {
                        trace!("not handling MailboxData response Exists {exists:?}");
                    }
                    imap_proto::MailboxDatum::Flags(flags) => {
                        trace!("not handling MailboxData response Flags {flags:?}");
                    }
                    imap_proto::MailboxDatum::Recent(recent) => {
                        trace!("not handling MailboxData response Recent {recent:?}");
                    }
                    _ => {
                        warn!("ignoring unknown mailbox data response to SELECT {mailbox_datum:?}");
                    }
                },
                imap_proto::Response::Capabilities(caps) => {
                    for cap in caps {
                        match cap {
                            imap_proto::Capability::Atom(_) => self.capabilities.insert(cap),
                            _ => warn!("unexpected capability respone {cap:?}"),
                        }
                    }
                    trace!("updated capabilities to {:?}", self.capabilities);
                }
                imap_proto::Response::Data {
                    status: imap_proto::Status::Ok,
                    code: None,
                    information: Some(information),
                } => {
                    debug!("{information}");
                }
                imap_proto::Response::Data {
                    status: imap_proto::Status::Ok,
                    code: Some(code),
                    information,
                } => match code {
                    imap_proto::ResponseCode::UidValidity(validity) => {
                        let validity = validity
                            .try_into()
                            .expect("received uid validity should be spec compliant");
                        if let Some(cached) = cached_uid_validity {
                            assert_eq!(cached, validity);
                        }
                        new_mailbox.uid_validity(validity);
                    }
                    imap_proto::ResponseCode::HighestModSeq(modseq) => {
                        new_mailbox.highest_modseq(
                            (*modseq)
                                .try_into()
                                .expect("Project expects RFC 4551 compatible IMAP server"),
                        );
                    }
                    imap_proto::ResponseCode::PermanentFlags(flags) => {
                        trace!("not handling Data response PermanentFlags {flags:?}");
                    }
                    imap_proto::ResponseCode::UidNext(uid_next) => {
                        trace!("not handling Data response UidNext {uid_next:?}");
                    }
                    imap_proto::ResponseCode::Unseen(unseen) => {
                        trace!("not handling Data response Unseen {unseen:?}");
                    }
                    _ => {
                        warn!("ignoring unknown data response to SELECT");
                        if let Some(information) = information {
                            warn!("{information}");
                        }
                        trace!("{code:?}");
                    }
                },
                imap_proto::Response::Fetch(msg_num, attributes) => {
                    trace!("handling fetch with attributes {attributes:?}");
                    let mut metadata_builder = RemoteMailMetadataBuilder::default();
                    for attribute in attributes {
                        match attribute {
                            imap_proto::AttributeValue::Flags(flags) => {
                                metadata_builder.flags(Flag::into_bitflags(flags));
                            }
                            imap_proto::AttributeValue::ModSeq(modseq) => {
                                metadata_builder.modseq(
                                    modseq
                                        .try_into()
                                        .expect("received modseq should be nonzero"),
                                );
                            }
                            imap_proto::AttributeValue::Uid(uid) => {
                                metadata_builder
                                    .uid(uid.try_into().expect("received uid should be nonzero"));
                            }
                            _ => {
                                warn!("msg {msg_num} unhandled attribute {attribute:?}");
                            }
                        }
                    }
                    updates.push(
                        metadata_builder
                            .build()
                            .expect("fetch metadata should be complete"),
                    );
                }
                imap_proto::Response::Vanished { earlier, uids } => {
                    debug_assert!(
                        earlier,
                        "earlier should always be true during select (see https://datatracker.ietf.org/doc/html/rfc7162#section-3.2.10)"
                    );
                    deletions = Some(SequenceSet::from(uids));
                }
                _ => {
                    warn!("ignoring unknown response to SELECT");
                    trace!("{:?}", response.parsed());
                }
            }
        }

        let mailbox_data = new_mailbox
            .build()
            .expect("mailbox data should be all available at this point");
        trace!("selected_mailbox = {mailbox_data:?}");
        trace!("mail updates = {updates:?}");
        trace!("mail deletions = {deletions:?}");
        let client = SelectedClient::new(
            self.connection,
            &self.capabilities,
            self.untagged_response_receiver,
            task_tx,
        );

        Selection {
            client,
            remote_changes: RemoteChanges { updates, deletions },
            mailbox_data,
        }
    }
}
