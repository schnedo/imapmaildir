use log::{debug, trace, warn};
use tokio::sync::mpsc;

use crate::{
    imap::{
        client::{
            SelectedClient,
            capability::{Capabilities, Capability},
        },
        codec::ResponseData,
        connection::Connection,
        mailbox::{MailboxBuilder, RemoteMail},
    },
    state::State,
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
        Self {
            connection,
            untagged_response_receiver,
            capabilities,
        }
    }

    pub async fn select(
        self,
        state: State,
        mailbox: &str,
    ) -> (SelectedClient, mpsc::Receiver<RemoteMail>) {
        assert!(self.capabilities.contains(Capability::Condstore));
        let command = format!("SELECT {mailbox} (CONDSTORE)");
        let (mut client, mail_rx) = self.do_select(state, mailbox, &command).await;
        client.init().await;

        (client, mail_rx)
    }

    // todo: add optional qresync parameters
    pub async fn qresync_select(
        mut self,
        state: State,
        mailbox: &str,
    ) -> (SelectedClient, mpsc::Receiver<RemoteMail>) {
        assert!(self.capabilities.contains(Capability::QResync));
        let command = "ENABLE QRESYNC";
        debug!("{command}");
        self.connection
            .send(command)
            .await
            .expect("enabling qresync should succeed");
        let command = format!(
            "SELECT {mailbox} (QRESYNC ({} {}))",
            state.uid_validity().await,
            state.highest_modseq().await
        );
        self.do_select(state, mailbox, &command).await
    }

    async fn do_select(
        mut self,
        state: State,
        mailbox: &str,
        command: &str,
    ) -> (SelectedClient, mpsc::Receiver<RemoteMail>) {
        debug!("{command}");
        self.connection
            .send(command)
            .await
            .expect("selecting a mailbox should succeed");

        let mut new_mailbox = MailboxBuilder::default();
        new_mailbox.name(mailbox.to_string());

        while let Ok(response) = self.untagged_response_receiver.try_recv() {
            match response.parsed() {
                imap_proto::Response::MailboxData(mailbox_datum) => match mailbox_datum {
                    imap_proto::MailboxDatum::Flags(cows) => {
                        let mut flags = Vec::with_capacity(cows.len());
                        for cow in cows {
                            flags.push(cow.to_string());
                        }
                        new_mailbox.flags(flags);
                    }
                    imap_proto::MailboxDatum::Exists(exists) => {
                        new_mailbox.exists(*exists);
                    }
                    imap_proto::MailboxDatum::Recent(recent) => {
                        new_mailbox.recent(*recent);
                    }
                    _ => {
                        trace!(
                            "ignoring unknown mailbox data response to SELECT {mailbox_datum:?}"
                        );
                    }
                },
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
                    imap_proto::ResponseCode::Unseen(unseen) => {
                        new_mailbox.unseen(*unseen);
                    }
                    imap_proto::ResponseCode::PermanentFlags(cows) => {
                        let mut flags = Vec::with_capacity(cows.len());
                        for cow in cows {
                            flags.push(cow.to_string());
                        }
                        new_mailbox.permanent_flags(flags);
                    }
                    imap_proto::ResponseCode::UidNext(next) => {
                        new_mailbox
                            .uid_next(next.try_into().expect("server should send valid uidnext"));
                    }
                    imap_proto::ResponseCode::UidValidity(validity) => {
                        state.set_uid_validity(validity.into()).await;
                        new_mailbox.uid_validity(validity.into());
                    }
                    imap_proto::ResponseCode::HighestModSeq(modseq) => {
                        state
                            .set_highest_modseq(
                                modseq
                                    .try_into()
                                    .expect("received highest modseq should be legal"),
                            )
                            .await;
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
                _ => {
                    warn!("ignoring unknown response to SELECT");
                    trace!("{:?}", response.parsed());
                }
            }
        }

        let mailbox = new_mailbox
            .build()
            .expect("mailbox data should be all available at this point");
        trace!("selected_mailbox = {mailbox:?}");

        SelectedClient::new(
            self.connection,
            self.untagged_response_receiver,
            self.capabilities,
            state,
            mailbox,
        )
    }
}
