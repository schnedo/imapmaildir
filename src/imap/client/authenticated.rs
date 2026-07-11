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

    pub async fn qresync_select(
        mut self,
        task_tx: mpsc::Sender<Task>,
        mailbox: &str,
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
    ) -> Selection {
        let command = "ENABLE QRESYNC";
        log::debug!("{command}");
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
        log::debug!("{command}");

        let (send_done_tx, mut send_done_rx) = mpsc::channel::<()>(1);
        let receive_handle = tokio::spawn(async move {
            log::trace!("running receive task for select response");
            let mut new_mailbox = MailboxMetadataBuilder::default();

            let mut updates: Vec<RemoteMailMetadata> = Vec::new();
            let mut deletions = None;

            let mut handle_response = |response: ResponseData| match response.parsed() {
                imap_proto::Response::MailboxData(mailbox_datum) => match mailbox_datum {
                    imap_proto::MailboxDatum::Exists(exists) => {
                        log::trace!("not handling MailboxData response Exists {exists:?}");
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
                    code: None,
                    information: Some(information),
                } => {
                    log::debug!("{information}");
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
                imap_proto::Response::Fetch(msg_num, attributes) => {
                    log::trace!("handling fetch with attributes {attributes:?}");
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
                                log::warn!("msg {msg_num} unhandled attribute {attribute:?}");
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
                    deletions = Some(
                        SequenceSet::try_from(uids)
                            .expect("received ranges should start with valid uid"),
                    );
                }
                _ => {
                    log::warn!("ignoring unknown response to SELECT");
                    log::trace!("{:?}", response.parsed());
                }
            };
            loop {
                tokio::select! {
                    None = send_done_rx.recv() => {
                        break;
                    }
                    Some(response) = self.untagged_response_receiver.recv() => {
                        handle_response(response);
                    }
                }
            }
            while !self.untagged_response_receiver.is_empty()
                && let Some(response) = self.untagged_response_receiver.recv().await
            {
                handle_response(response);
            }

            let mailbox_data = new_mailbox
                .build()
                .expect("mailbox data should be all available at this point");
            log::trace!("selected_mailbox = {mailbox_data:?}");
            log::trace!("mail updates = {updates:?}");
            log::trace!("mail deletions = {deletions:?}");

            (
                updates,
                deletions,
                mailbox_data,
                self.capabilities,
                self.untagged_response_receiver,
            )
        });

        self.connection
            .send(command.into())
            .await
            .expect("selecting a mailbox should succeed");
        drop(send_done_tx);
        let (updates, deletions, mailbox_data, capabilities, untagged_response_receiver) =
            receive_handle
                .await
                .expect("waiting for receive task should succeed");

        let client = SelectedClient::new(
            self.connection,
            &capabilities,
            untagged_response_receiver,
            task_tx,
        );

        Selection {
            client,
            remote_changes: RemoteChanges { updates, deletions },
            mailbox_data,
        }
    }
}
