use std::mem::transmute;

use log::{debug, trace};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{
    imap::{
        Uid,
        client::capability::Capabilities,
        codec::ResponseData,
        connection::Connection,
        mailbox::{Mailbox, RemoteMail, RemoteMailMetadata, SequenceSet},
    },
    sync::Flag,
};

pub struct SelectedClient {
    connection: Connection,
    capabilities: Capabilities,
    mailbox: Mailbox,
    mail_rx: mpsc::Receiver<RemoteMail>,
    metadata_rx: mpsc::Receiver<RemoteMailMetadata>,
}
impl SelectedClient {
    pub async fn new(
        connection: Connection,
        mut untagged_response_receiver: mpsc::Receiver<ResponseData>,
        capabilities: Capabilities,
        mailbox: Mailbox,
    ) -> Self {
        let (mail_tx, mail_rx) = mpsc::channel(32);
        let (metadata_tx, metadata_rx) = mpsc::channel(32);

        tokio::spawn(async move {
            while let Some(response) = untagged_response_receiver.recv().await {
                trace!("handle untagged response {:?}", response.parsed());
                match response.parsed() {
                    imap_proto::Response::Fetch(_, attributes) => {
                        if let [
                            imap_proto::AttributeValue::Uid(uid),
                            imap_proto::AttributeValue::Flags(flags),
                            imap_proto::AttributeValue::Rfc822(content),
                        ] = attributes.as_slice()
                        {
                            trace!("{flags:?}");
                            let mail_flags = flags
                                .iter()
                                .filter_map(|flag| {
                                    <&str as TryInto<Flag>>::try_into(flag.as_ref()).ok()
                                })
                                .collect();

                            let metadata =
                                RemoteMailMetadata::new(Uid::try_from(uid).ok(), mail_flags);

                            if let Some(content) = content {
                                let content =
                                    unsafe { transmute::<&[u8], &[u8]>(content.as_ref()) };
                                let remote_mail = RemoteMail::new(response, metadata, content);
                                mail_tx.send(remote_mail).await;
                            } else {
                                metadata_tx.send(metadata).await;
                            }
                        } else {
                            panic!(
                                "wrong format of FETCH response. check order of attributes in command"
                            );
                        }
                    }
                    _ => {
                        trace!(
                            "ignoring unhandled untagged response {:?}",
                            response.parsed()
                        );
                    }
                }
            }
        });

        Self {
            connection,
            capabilities,
            mailbox,
            mail_rx,
            metadata_rx,
        }
    }

    pub async fn fetch_mail(&mut self, sequence_set: &SequenceSet) {
        let command = format!("UID FETCH {sequence_set} (UID, FLAGS, RFC822)");
        debug!("{command}");
        self.connection
            .send(&command)
            .await
            .expect("fetching mails should succeed");
    }

    pub fn mail_rx(&mut self) -> &mut mpsc::Receiver<RemoteMail> {
        &mut self.mail_rx
    }
}

#[derive(Error, Debug)]
#[error("unknown flag {flag}")]
pub struct UnknownFlagError<'a> {
    flag: &'a str,
}

impl<'a> TryFrom<&'a str> for Flag {
    type Error = UnknownFlagError<'a>;

    fn try_from(value: &'a str) -> std::result::Result<Self, Self::Error> {
        match value {
            "\\Seen" => Ok(Flag::Seen),
            "\\Answered" => Ok(Flag::Answered),
            "\\Flagged" => Ok(Flag::Flagged),
            "\\Deleted" => Ok(Flag::Deleted),
            "\\Draft" => Ok(Flag::Draft),
            "\\Recent" => Ok(Flag::Recent),
            _ => {
                trace!("Encountered unhandled Flag {value}");
                Err(Self::Error { flag: value })
            }
        }
    }
}
