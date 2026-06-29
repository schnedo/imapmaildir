use std::mem::transmute;
use std::time::Duration;
use std::{io::Write as _, sync::Arc};

use log::{debug, trace};
use tokio::sync::{Mutex, mpsc};

use crate::maildir::LocalChanges;
use crate::{
    imap::{
        RemoteContent, RemoteMail, RemoteMailMetadata,
        client::capability::{Capabilities, Capability},
        transport::{Connection, ResponseData},
    },
    maildir::{LocalMail, MaildirFile, NewLocalMailMetadata},
    repository::{Flag, ModSeq, SequenceRange, SequenceSet, Uid},
    sync::Task,
};

pub enum IdleStopReason {
    Timeout,
    Remote,
    Local { changes: LocalChanges },
}

#[derive(Debug)]
pub struct SelectedClient {
    connection: Connection,
    idling: Arc<Mutex<bool>>,
    idle_stop_rx: mpsc::Receiver<IdleStopReason>,
    idle_stop_tx: mpsc::Sender<IdleStopReason>,
}
impl SelectedClient {
    #[expect(clippy::too_many_lines)]
    pub fn new(
        connection: Connection,
        capabilities: &Capabilities,
        mut untagged_response_receiver: mpsc::Receiver<ResponseData>,
        task_tx: mpsc::Sender<Task>,
    ) -> Self {
        assert!(
            capabilities.contains(Capability::LiteralPlus),
            "server should support LITERAL+ capability"
        );
        assert!(
            capabilities.contains(Capability::UidPlus),
            "server should support UIDPLUS capability"
        );
        let idling = Arc::new(Mutex::new(false));
        let is_idling = idling.clone();
        let (stop_tx, idle_stop_rx) = mpsc::channel(1);
        let idle_stop_tx = stop_tx.clone();

        tokio::spawn(async move {
            while let Some(response) = untagged_response_receiver.recv().await {
                match response.parsed() {
                    imap_proto::Response::Fetch(_, attributes) => {
                        match attributes.as_slice() {
                            [
                                imap_proto::AttributeValue::Uid(uid),
                                imap_proto::AttributeValue::ModSeq(modseq),
                                imap_proto::AttributeValue::Flags(flags),
                                imap_proto::AttributeValue::BodySection {
                                    section: _,
                                    index: _,
                                    data: content,
                                },
                            ] => {
                                trace!("FETCH uid {uid:?} modseq {modseq:?} flags {flags:?}");
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
                                    task_tx
                                        .send(Task::NewMail(remote_mail))
                                        .await
                                        .expect("task channel should still be open");
                                } else {
                                    unreachable!("mail without content")
                                }
                            }
                            [
                                imap_proto::AttributeValue::Uid(uid),
                                imap_proto::AttributeValue::ModSeq(modseq),
                            ] => {
                                trace!("FETCH uid {uid:?} modseq {modseq:?}");
                                task_tx
                                    .send(Task::UpdateModseq(
                                        uid.try_into().expect("uid should be nonzero"),
                                        modseq.try_into().expect("modseq shoud be nonzero"),
                                    ))
                                    .await
                                    .expect("task channel should still be open");
                            }
                            [
                                imap_proto::AttributeValue::Uid(uid),
                                imap_proto::AttributeValue::ModSeq(modseq),
                                imap_proto::AttributeValue::Flags(flags),
                            ] => {
                                trace!("FETCH uid {uid:?} modseq {modseq:?} flags {flags:?}");
                                task_tx
                                    .send(Task::UpdateFlags(RemoteMailMetadata::new(
                                        uid.try_into().expect("uid should be nonzero"),
                                        Flag::into_bitflags(flags),
                                        modseq.try_into().expect("modseq shoud be nonzero"),
                                    )))
                                    .await
                                    .expect("task channel should still be open");
                            }
                            _ => {
                                trace!("attributes {attributes:?}");
                                panic!(
                                    "wrong format of FETCH response. check order of attributes in command"
                                );
                            }
                        }
                    }
                    imap_proto::Response::Vanished { earlier, uids } => {
                        trace!("VANISHED earlier {earlier:?} uids: {uids:?}");
                        task_tx
                            .send(Task::Delete(
                                uids.try_into()
                                    .expect("received uid ranges should start with valid uid"),
                            ))
                            .await
                            .expect("deletion channel should still be open");
                    }
                    imap_proto::Response::Expunge(_) => {
                        let mut is_idling = is_idling.lock().await;
                        if *is_idling {
                            trace!("stopping idle");
                            *is_idling = false;
                            stop_tx
                                .send(IdleStopReason::Remote)
                                .await
                                .expect("idle stop channel should still be open");
                        } else {
                            trace!("ignoring response due to not idling");
                        }
                    }
                    imap_proto::Response::MailboxData(mailbox_datum) => match mailbox_datum {
                        imap_proto::MailboxDatum::Exists(_)
                        | imap_proto::MailboxDatum::Recent(_) => {
                            let mut is_idling = is_idling.lock().await;
                            if *is_idling {
                                trace!("stopping idle");
                                *is_idling = false;
                                stop_tx
                                    .send(IdleStopReason::Remote)
                                    .await
                                    .expect("idle stop channel should still be open");
                            } else {
                                trace!("ignoring response due to not idling");
                            }
                        }
                        _ => trace!(
                            "ignoring unhandled mailbox data response {:?}",
                            response.parsed()
                        ),
                    },
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
            idling,
            idle_stop_rx,
            idle_stop_tx,
        }
    }

    pub async fn fetch_mail(&mut self, sequence_set: &SequenceSet) {
        let command = format!("UID FETCH {sequence_set} (UID, ModSeq, FLAGS, BODY.PEEK[])");
        debug!("{command}");
        self.connection
            .send(command.into())
            .await
            .expect("fetching mails should succeed");
    }

    pub async fn fetch_all(&mut self) {
        let command = "UID FETCH 1:* (UID, ModSeq, FLAGS, BODY.PEEK[])";
        debug!("{command}");
        self.connection
            .send(command.into())
            .await
            .expect("fetching mails should succeed");
    }

    pub async fn fetch_since(&mut self, modseq: ModSeq) {
        let command =
            format!("UID FETCH 1:* (UID, ModSeq, FLAGS, BODY.PEEK[]) (CHANGEDSINCE {modseq})");
        debug!("{command}");
        self.connection
            .send(command.into())
            .await
            .expect("fetching mails should succeed");
    }

    pub async fn store(
        &mut self,
        mailbox: &str,
        mut mails: impl Iterator<Item = LocalMail>,
    ) -> mpsc::Receiver<(Uid, NewLocalMailMetadata)> {
        let (info_tx, mut info_rx) = mpsc::channel(32);
        if let Some(mail) = mails.next() {
            let command = format!("APPEND {mailbox}");
            debug!("{command}");
            let mut command = command.into_bytes();

            let size_hint = mails.size_hint();
            let mut metadatas = Vec::with_capacity(size_hint.1.unwrap_or(size_hint.0));
            metadatas.push(mail.append_to(&mut command));

            for mail in mails {
                metadatas.push(mail.append_to(&mut command));
            }

            let response = self
                .connection
                .send(command)
                .await
                // todo: check reason and move failing mails out of cur
                .expect("storing new mail should succeed");

            if let Some(code) = response.unsafe_get_tagged_response_code() {
                if let imap_proto::ResponseCode::AppendUid(_uid_validity, uid_set_members) = code {
                    let uid_ranges: Result<Vec<_>, _> = uid_set_members
                        .iter()
                        .map(SequenceRange::try_from)
                        .collect();
                    let uid_ranges = uid_ranges.expect("received uids should be valid");

                    tokio::spawn(async move {
                        futures::future::join_all(
                            uid_ranges
                                .into_iter()
                                .flat_map(SequenceRange::into_iter)
                                .zip(metadatas)
                                .map(|info| info_tx.send(info)),
                        )
                        .await
                    });
                } else {
                    unreachable!("response code of APPEND should be AppendUid");
                }
            } else {
                unreachable!("response to APPEND should have a response code");
            }
        } else {
            info_rx.close();
        }

        info_rx
    }

    pub async fn remove_flag(
        &mut self,
        highest_modseq: ModSeq,
        flag: Flag,
        sequence_set: &SequenceSet,
    ) {
        let command = format!(
            "UID STORE {sequence_set} (UNCHANGEDSINCE {highest_modseq}) -FLAGS.SILENT ({flag})"
        );
        debug!("{command}");

        self.connection
            .send(command.into_bytes())
            .await
            // todo: handle bad response when highest_modseq does not match
            .expect("sending of flag update should succeed");
    }

    pub async fn add_flag(
        &mut self,
        highest_modseq: ModSeq,
        flag: Flag,
        sequence_set: &SequenceSet,
    ) {
        let command = format!(
            "UID STORE {sequence_set} (UNCHANGEDSINCE {highest_modseq}) +FLAGS.SILENT ({flag})"
        );
        debug!("{command}");

        self.connection
            .send(command.into_bytes())
            .await
            .expect("sending of flag update should succeed");
    }

    pub async fn delete(&mut self, highest_modseq: ModSeq, sequence_set: &SequenceSet) {
        self.add_flag(highest_modseq, Flag::Deleted, sequence_set)
            .await;
        let command = format!("UID EXPUNGE {sequence_set}");
        debug!("{command}");
        self.connection
            .send(command.into_bytes())
            .await
            .expect("sending uid expunge should succeed");
    }

    pub fn idle_stop_tx(&self) -> mpsc::Sender<IdleStopReason> {
        self.idle_stop_tx.clone()
    }

    pub async fn idle(&mut self, timeout: Duration) -> IdleStopReason {
        let command = "IDLE";
        debug!("{command}");
        *self.idling.lock().await = true;
        let timeout_handle = tokio::spawn(tokio::time::sleep(timeout));
        let response = self
            .connection
            .send(command.into())
            .await
            .expect("sending idle should succeed");
        match response.parsed() {
            imap_proto::Response::Continue {
                code: _,
                information: _,
            } => trace!("idling for up to {} seconds", timeout.as_secs()),
            _ => todo!("handle idle no continuation"),
        }
        let stop_reason = tokio::select! {
            stop = self.idle_stop_rx.recv() => {
                stop.expect("idle stop channel should still be open")
            }
            timeout = timeout_handle => {
                timeout.expect("idle timeout should not fail");
                debug!("idle timed out");

                IdleStopReason::Timeout
            }
        };
        let command = "DONE";
        debug!("{command}");
        self.connection
            .send_continuation(command.into())
            .await
            .expect("sending idle done should succeed");
        *self.idling.lock().await = false;

        stop_reason
    }
}

impl LocalMail {
    fn append_to(self, command: &mut Vec<u8>) -> NewLocalMailMetadata {
        if let Some(flags) = Flag::format(self.metadata().flags()) {
            write!(command, " ({flags})")
                .expect("appending formatted flags to APPEND command should succeed");
        }
        let (metadata, content) = self.unpack();
        // todo: use cached content length (and extend command with content)
        write!(command, " {{{}+}}\r\n", content.len())
            .expect("appending content length to APPEND command should succeed");
        command.extend(content);

        metadata
    }
}
