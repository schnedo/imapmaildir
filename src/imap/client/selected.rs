use std::fmt::Write as _;
use std::mem::transmute;

use log::{debug, info, trace};
use tokio::sync::mpsc;

use crate::{
    imap::{
        ModSeq, Uid,
        codec::ResponseData,
        connection::Connection,
        mailbox::{Content, RemoteMail, RemoteMailMetadata, SequenceSet},
    },
    maildir::LocalMail,
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
                                let content = Content::new(response.raw(), content);

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
            .send(&command)
            .await
            .expect("fetching mails should succeed");
    }

    pub async fn fetch_all(&mut self) {
        // todo: move the initializing to appropriate location
        info!("initializing new imap repository");
        self.fetch_mail(&SequenceSet::all()).await;
    }

    // todo: use rfc3502 MULTIAPPEND
    pub async fn store(&mut self, mailbox: &str, mail: &LocalMail) {
        let mut command = format!("APPEND {mailbox}");
        if let Some(flags) = Flag::format(mail.metadata().flags()) {
            write!(command, " ({flags})")
                .expect("appending formatted flags to APPEND command should succeed");
        }
        let content = mail.content();
        // todo: use cached content length (and extend command with content)
        write!(command, " {{{}}}", content.len())
            .expect("appending content length to APPEND command should succeed");
        debug!("{command}");
        self.connection
            .send(&command)
            .await
            .expect("storing new mail should succeed");

        debug!("<content>");
        self.connection
            .send_continuation(content)
            .await
            .expect("sending mail content should succeed");
    }
}
