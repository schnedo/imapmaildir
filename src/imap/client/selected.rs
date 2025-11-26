use std::io::Write as _;
use std::mem::transmute;

use log::{debug, info, trace};
use tokio::sync::mpsc;

use crate::{
    imap::{
        ModSeq, Uid,
        client::capability::{Capabilities, Capability},
        codec::ResponseData,
        connection::Connection,
        mailbox::{RemoteContent, RemoteMail, RemoteMailMetadata, SequenceRange, SequenceSet},
    },
    maildir::{LocalMail, LocalMailMetadata},
    sync::Flag,
};

pub struct StoredMailInfo {
    metadata: LocalMailMetadata,
    uid: Uid,
}

impl StoredMailInfo {
    pub fn new(metadata: LocalMailMetadata, uid: Uid) -> Self {
        Self { metadata, uid }
    }

    pub fn unpack(self) -> (LocalMailMetadata, Uid) {
        (self.metadata, self.uid)
    }
}

#[derive(Debug)]
pub struct SelectedClient {
    connection: Connection,
}
impl SelectedClient {
    pub fn new(
        connection: Connection,
        capabilities: &Capabilities,
        mut untagged_response_receiver: mpsc::Receiver<ResponseData>,
        mail_tx: mpsc::Sender<RemoteMail>,
        highest_modseq_tx: mpsc::Sender<ModSeq>,
    ) -> Self {
        assert!(
            capabilities.contains(Capability::LiteralPlus),
            "server should support LITERAL+ capability"
        );
        assert!(
            capabilities.contains(Capability::UidPlus),
            "server should support UIDPLUS capability"
        );
        tokio::spawn(async move {
            while let Some(response) = untagged_response_receiver.recv().await {
                match response.parsed() {
                    imap_proto::Response::Fetch(_, attributes) => {
                        if let [
                            imap_proto::AttributeValue::Uid(uid),
                            imap_proto::AttributeValue::ModSeq(modseq),
                            imap_proto::AttributeValue::Flags(flags),
                            imap_proto::AttributeValue::Rfc822(content),
                        ] = attributes.as_slice()
                        {
                            trace!("{flags:?}");
                            let mail_flags = Flag::into_bitflags(flags);
                            let metadata = RemoteMailMetadata::new(
                                Uid::try_from(uid).expect("remote uid should be valid"),
                                mail_flags,
                                modseq.try_into().expect("received modseq should be valid"),
                            );

                            if let Some(content) = content {
                                let content =
                                // safe as long as the raw data is not dropped
                                    unsafe { transmute::<&[u8], &[u8]>(content.as_ref()) };
                                let content = RemoteContent::new(response.raw(), content);

                                let remote_mail = RemoteMail::new(metadata, content);
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
        let command = format!("UID FETCH {sequence_set} (UID, ModSeq, FLAGS, RFC822)");
        debug!("{command}");
        self.connection
            .send(command.into())
            .await
            .expect("fetching mails should succeed");
    }

    pub async fn fetch_all(&mut self) {
        // todo: move the initializing to appropriate location
        info!("initializing new imap repository");
        self.fetch_mail(&SequenceSet::all()).await;
    }

    pub async fn store(
        &mut self,
        mailbox: &str,
        mails: impl Iterator<Item = LocalMail>,
    ) -> impl Iterator<Item = StoredMailInfo> {
        let command = format!("APPEND {mailbox}");
        debug!("{command}");
        let mut command = command.into_bytes();
        let initial_length = command.len();

        let size_hint = mails.size_hint();
        let mut metadatas = Vec::with_capacity(size_hint.1.unwrap_or(size_hint.0));

        for mail in mails {
            if let Some(flags) = Flag::format(mail.metadata().flags()) {
                write!(command, " ({flags})")
                    .expect("appending formatted flags to APPEND command should succeed");
            }
            let (metadata, content) = mail.unpack();
            metadatas.push(metadata);
            // todo: use cached content length (and extend command with content)
            write!(command, " {{{}+}}\r\n", content.len())
                .expect("appending content length to APPEND command should succeed");
            command.extend(content.into_iter());
        }
        debug_assert!(
            command.len() > initial_length,
            "there should be mails when trying to append to mailbox"
        );

        let response = self
            .connection
            .send(command)
            .await
            .expect("storing new mail should succeed");

        if let Some(code) = response.unsafe_get_tagged_response_code() {
            if let imap_proto::ResponseCode::AppendUid(_uid_validity, uid_set_members) = code {
                let uid_ranges: Vec<SequenceRange> =
                    uid_set_members.iter().map(SequenceRange::from).collect();
                uid_ranges
                    .into_iter()
                    .flat_map(SequenceRange::into_iter)
                    .zip(metadatas.into_iter())
                    .map(|(uid, metadata)| StoredMailInfo::new(metadata, uid))
            } else {
                unreachable!("response code of APPEND should be AppendUid")
            }
        } else {
            unreachable!("response to APPEND should have a response code")
        }
    }
}
