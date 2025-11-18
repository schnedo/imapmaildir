use std::mem::transmute;

use log::{debug, info, trace};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{
    imap::{
        ModSeq, Uid,
        codec::ResponseData,
        connection::Connection,
        mailbox::{RemoteMail, RemoteMailMetadata, SequenceSet},
    },
    sync::Flag,
};

#[derive(Debug)]
pub struct SelectedClient {
    connection: Connection,
}
impl SelectedClient {
    pub fn new(
        connection: Connection,
        mut untagged_response_receiver: mpsc::Receiver<ResponseData>,
        mail_tx: mpsc::Sender<RemoteMail>,
        highest_modseq_tx: mpsc::Sender<ModSeq>,
    ) -> Self {
        tokio::spawn(async move {
            while let Some(response) = untagged_response_receiver.recv().await {
                match response.parsed() {
                    imap_proto::Response::Fetch(_, attributes) => {
                        if let [
                            imap_proto::AttributeValue::Uid(uid),
                            imap_proto::AttributeValue::Flags(flags),
                            imap_proto::AttributeValue::Rfc822(content),
                        ] = attributes.as_slice()
                        {
                            trace!("{flags:?}");
                            let mail_flags = Flag::into_bitflags(flags);
                            let metadata =
                                // todo: check for modseq in fetch response
                                RemoteMailMetadata::new(Uid::try_from(uid).ok(), mail_flags, None);

                            if let Some(content) = content {
                                let content =
                                    unsafe { transmute::<&[u8], &[u8]>(content.as_ref()) };
                                let remote_mail = RemoteMail::new(response, metadata, content);
                                mail_tx
                                    .send(remote_mail)
                                    .await
                                    .expect("mail channel should still be open");
                            } else {
                                todo!("handle mail without content")
                            }
                        } else {
                            panic!(
                                "wrong format of FETCH response. check order of attributes in command"
                            );
                        }
                    }
                    imap_proto::Response::Data {
                        code: Some(imap_proto::ResponseCode::HighestModSeq(modseq)),
                        ..
                    } => {
                        highest_modseq_tx
                            .send(
                                modseq
                                    .try_into()
                                    .expect("received highest_modseq should be valid"),
                            )
                            .await
                            .expect("channel should be open");
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

        Self { connection }
    }

    pub async fn fetch_mail(&mut self, sequence_set: &SequenceSet) {
        let command = format!("UID FETCH {sequence_set} (UID, FLAGS, RFC822)");
        debug!("{command}");
        self.connection
            .send(&command)
            .await
            .expect("fetching mails should succeed");
    }

    pub async fn fetch_all(&mut self) {
        info!("initializing new imap repository");
        self.fetch_mail(&SequenceSet::all()).await;
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
