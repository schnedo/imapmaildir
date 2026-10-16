use std::io::Write as _;
use std::mem::transmute;
use std::time::{Duration, Instant};

use crossfire::{
    AsyncRx, MAsyncTx,
    mpsc::{self, Array},
};
use futures::FutureExt as _;

use crate::maildir::LocalChanges;
use crate::{
    imap::{
        RemoteContent, RemoteMail, RemoteMailMetadata,
        client::capability::{Capabilities, Capability},
        transport::{Chain, Connection, ResponseData, ResponseHandler},
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
    mailbox_handler: MailboxHandler,
    idle_stop_rx: AsyncRx<Array<IdleStopReason>>,
    idle_stop_tx: MAsyncTx<Array<IdleStopReason>>,
}
impl SelectedClient {
    pub fn new(
        connection: Connection,
        capabilities: &Capabilities,
        task_tx: MAsyncTx<Array<Task>>,
    ) -> Self {
        assert!(
            capabilities.contains(Capability::LiteralPlus),
            "server should support LITERAL+ capability"
        );
        assert!(
            capabilities.contains(Capability::UidPlus),
            "server should support UIDPLUS capability"
        );
        let (idle_stop_tx, idle_stop_rx) = mpsc::bounded_async(1);

        Self {
            connection,
            mailbox_handler: MailboxHandler { task_tx },
            idle_stop_rx,
            idle_stop_tx,
        }
    }

    pub async fn fetch_mail(&mut self, sequence_set: &SequenceSet) {
        let command = format!("UID FETCH {sequence_set} (UID, ModSeq, FLAGS, BODY.PEEK[])");
        log::debug!("{command}");
        self.connection
            .send(command.into(), &mut self.mailbox_handler)
            .await
            .expect("fetching mails should succeed");
    }

    pub async fn fetch_all(&mut self) {
        let command = "UID FETCH 1:* (UID, ModSeq, FLAGS, BODY.PEEK[])";
        log::debug!("{command}");
        self.connection
            .send(command.into(), &mut self.mailbox_handler)
            .await
            .expect("fetching mails should succeed");
    }

    pub async fn fetch_since(&mut self, modseq: ModSeq) {
        let command =
            format!("UID FETCH 1:* (UID, ModSeq, FLAGS, BODY.PEEK[]) (CHANGEDSINCE {modseq})");
        log::debug!("{command}");
        self.connection
            .send(command.into(), &mut self.mailbox_handler)
            .await
            .expect("fetching mails should succeed");
    }

    pub async fn store(
        &mut self,
        mailbox: &str,
        mut mails: impl Iterator<Item = LocalMail>,
    ) -> AsyncRx<Array<(Uid, NewLocalMailMetadata)>> {
        let (info_tx, info_rx) = mpsc::bounded_async(32);
        if let Some(mail) = mails.next() {
            let command = format!("APPEND {mailbox}");
            log::debug!("{command}");
            let mut command = command.into_bytes();

            let size_hint = mails.size_hint();
            let mut metadatas = Vec::with_capacity(size_hint.1.unwrap_or(size_hint.0));
            metadatas.push(mail.append_to(&mut command));

            for mail in mails {
                metadatas.push(mail.append_to(&mut command));
            }

            let response = self
                .connection
                .send(command, &mut self.mailbox_handler)
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

                    compio::runtime::spawn(async move {
                        futures::future::join_all(
                            uid_ranges
                                .into_iter()
                                .flat_map(SequenceRange::into_iter)
                                .zip(metadatas)
                                .map(|info| info_tx.send(info)),
                        )
                        .await
                    })
                    .detach();
                } else {
                    unreachable!("response code of APPEND should be AppendUid");
                }
            } else {
                unreachable!("response to APPEND should have a response code");
            }
        } else {
            drop(info_tx);
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
        log::debug!("{command}");

        self.connection
            .send(command.into_bytes(), &mut self.mailbox_handler)
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
        log::debug!("{command}");

        self.connection
            .send(command.into_bytes(), &mut self.mailbox_handler)
            .await
            .expect("sending of flag update should succeed");
    }

    pub async fn delete(&mut self, highest_modseq: ModSeq, sequence_set: &SequenceSet) {
        self.add_flag(highest_modseq, Flag::Deleted, sequence_set)
            .await;
        let command = format!("UID EXPUNGE {sequence_set}");
        log::debug!("{command}");
        self.connection
            .send(command.into_bytes(), &mut self.mailbox_handler)
            .await
            .expect("sending uid expunge should succeed");
    }

    pub async fn logout(mut self) {
        let command = "LOGOUT";
        log::debug!("{command}");
        self.connection
            .send(command.into(), &mut self.mailbox_handler)
            .await
            .expect("logging out should succeed");
    }

    pub fn idle_stop_tx(&self) -> MAsyncTx<Array<IdleStopReason>> {
        self.idle_stop_tx.clone()
    }

    pub async fn idle(&mut self, timeout: Duration) -> IdleStopReason {
        let command = "IDLE";
        log::debug!("{command}");
        let deadline = Instant::now() + timeout;
        let connection = &mut self.connection;
        let idle_stop_rx = &self.idle_stop_rx;
        let mut handler = Chain(IdleHandler::default(), &mut self.mailbox_handler);

        let tag = connection.write_command(command.into()).await;
        loop {
            let response = connection.next_response().await;
            match response.parsed() {
                imap_proto::Response::Continue(_) => {
                    log::trace!("idling for up to {} seconds", timeout.as_secs());
                    break;
                }
                imap_proto::Response::Done { .. } => todo!("handle idle no continuation"),
                _ => handle_unsolicited(&mut handler, response).await,
            }
        }

        let stop_reason = loop {
            if handler.0.remote_changed {
                break IdleStopReason::Remote;
            }
            let event = compio::time::timeout_at(deadline, async {
                futures::select_biased! {
                    stop = idle_stop_rx.recv().fuse() => {
                        IdleEvent::Stop(stop.expect("idle stop channel should still be open"))
                    }
                    response = connection.next_response().fuse() => IdleEvent::Response(response),
                }
            })
            .await;
            match event {
                Ok(IdleEvent::Stop(reason)) => break reason,
                Ok(IdleEvent::Response(response)) => {
                    handle_unsolicited(&mut handler, response).await;
                }
                Err(_) => {
                    log::debug!("idle timed out");
                    break IdleStopReason::Timeout;
                }
            }
        };

        let command = "DONE";
        log::debug!("{command}");
        connection.write_continuation(command.into()).await;
        connection
            .receive_completion(&tag, &mut handler)
            .await
            .expect("idle should succeed");

        stop_reason
    }
}

enum IdleEvent {
    Stop(IdleStopReason),
    Response(ResponseData),
}

async fn handle_unsolicited(handler: &mut impl ResponseHandler, response: ResponseData) {
    if let Some(response) = handler.handle(response).await {
        log::trace!("unhandled untagged response {:?}", response.parsed());
    }
}

/// Detects remote changes that should stop IDLE
#[derive(Default)]
struct IdleHandler {
    remote_changed: bool,
}

impl ResponseHandler for IdleHandler {
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>> {
        std::future::ready(match response.parsed() {
            // todo: do not interrupt idle, if mails only got deleted (but handle
            // highest modseq then)
            imap_proto::Response::Expunge(_)
            | imap_proto::Response::MailboxData(
                imap_proto::MailboxDatum::Exists(_) | imap_proto::MailboxDatum::Recent(_),
            ) => {
                log::trace!("stopping idle");
                self.remote_changed = true;
                None
            }
            _ => Some(response),
        })
    }
}

/// Turns mail data received in the selected state into sync tasks, regardless of the command
/// that triggered it
#[derive(Debug)]
struct MailboxHandler {
    task_tx: MAsyncTx<Array<Task>>,
}

impl ResponseHandler for MailboxHandler {
    async fn handle(&mut self, response: ResponseData) -> Option<ResponseData> {
        match response.parsed() {
            imap_proto::Response::Fetch(_, attributes) => {
                let task = match attributes.as_slice() {
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
                        log::trace!("FETCH uid {uid:?} modseq {modseq:?} flags {flags:?}");
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

                            Task::NewMail(RemoteMail::new(metadata, content))
                        } else {
                            unreachable!("mail without content")
                        }
                    }
                    [
                        imap_proto::AttributeValue::Uid(uid),
                        imap_proto::AttributeValue::ModSeq(modseq),
                    ] => {
                        log::trace!("FETCH uid {uid:?} modseq {modseq:?}");
                        Task::UpdateModseq(
                            uid.try_into().expect("uid should be nonzero"),
                            modseq.try_into().expect("modseq shoud be nonzero"),
                        )
                    }
                    [
                        imap_proto::AttributeValue::Uid(uid),
                        imap_proto::AttributeValue::ModSeq(modseq),
                        imap_proto::AttributeValue::Flags(flags),
                    ] => {
                        log::trace!("FETCH uid {uid:?} modseq {modseq:?} flags {flags:?}");
                        Task::UpdateFlags(RemoteMailMetadata::new(
                            uid.try_into().expect("uid should be nonzero"),
                            Flag::into_bitflags(flags),
                            modseq.try_into().expect("modseq shoud be nonzero"),
                        ))
                    }
                    _ => {
                        log::trace!("attributes {attributes:?}");
                        panic!(
                            "wrong format of FETCH response. check order of attributes in command"
                        );
                    }
                };
                self.task_tx
                    .send(task)
                    .await
                    .expect("task channel should still be open");
                None
            }
            imap_proto::Response::Vanished { earlier, uids } => {
                log::trace!("VANISHED earlier {earlier:?} uids: {uids:?}");
                self.task_tx
                    .send(Task::Delete(
                        uids.try_into()
                            .expect("received uid ranges should start with valid uid"),
                    ))
                    .await
                    .expect("task channel should still be open");
                None
            }
            _ => Some(response),
        }
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
