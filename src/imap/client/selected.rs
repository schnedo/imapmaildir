use std::collections::HashSet;
use std::io::Write as _;
use std::mem::transmute;
use std::time::{Duration, Instant};

use crossfire::{
    AsyncRx, MAsyncTx,
    mpsc::{self, Array},
    oneshot::oneshot,
};
use enumflags2::BitFlags;
use futures::FutureExt as _;

use crate::maildir::LocalChanges;
use crate::{
    imap::{
        RemoteContent, RemoteMail, RemoteMailMetadata,
        client::capability::{Capabilities, Capability},
        transport::{Chain, Connection, ResponseData, ResponseHandler},
    },
    maildir::{FlagChange, LocalMail, MaildirFile, NewLocalMailMetadata},
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
        let command = format!(
            "UID FETCH 1:* (UID, ModSeq, FLAGS, BODY.PEEK[]) (CHANGEDSINCE {modseq} VANISHED)"
        );
        log::debug!("{command}");
        let mut handler = Chain(ModSeqTracker::default(), &mut self.mailbox_handler);
        self.connection
            .send(command.into(), &mut handler)
            .await
            .expect("fetching mails should succeed");
        if let Some(highest_modseq) = handler.0.highest_modseq {
            self.commit_highest_modseq(highest_modseq).await;
        }
    }

    pub async fn fetch_all_metadata(&mut self) -> Vec<RemoteMailMetadata> {
        let command = "UID FETCH 1:* (UID, ModSeq, FLAGS)";
        log::debug!("{command}");
        let mut handler = Chain(MetadataCollector::default(), &mut self.mailbox_handler);
        self.connection
            .send(command.into(), &mut handler)
            .await
            .expect("fetching mail metadata should succeed");

        handler.0.metadata
    }

    pub async fn complete_initial_sync(&mut self, modseq: ModSeq) {
        let (done_tx, done_rx) = oneshot();
        self.mailbox_handler
            .task_tx
            .send(Task::CompleteInitialSync(modseq, done_tx))
            .await
            .expect("task channel should still be open");
        done_rx
            .await
            .expect("task processing should signal completion of initial sync");
    }

    pub async fn commit_highest_modseq(&mut self, modseq: ModSeq) {
        self.mailbox_handler
            .task_tx
            .send(Task::SetHighestModseq(modseq))
            .await
            .expect("task channel should still be open");
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

    pub async fn store_flag_change(
        &mut self,
        highest_modseq: ModSeq,
        change: FlagChange,
        sequence_set: &SequenceSet,
    ) -> HashSet<Uid> {
        let mut rejected = HashSet::new();
        let mut pending: Vec<Uid> = sequence_set.iter().collect();
        let mut unchanged_since = highest_modseq;
        for (operation, flags) in [('+', change.added), ('-', change.removed)] {
            if flags.is_empty() {
                continue;
            }
            let Ok(set) = SequenceSet::try_from(&pending) else {
                break;
            };
            let (modified, modseq) = self
                .store_flags(unchanged_since, operation, flags, &set)
                .await;
            pending.retain(|uid| !modified.contains(uid));
            rejected.extend(modified);
            if let Some(modseq) = modseq {
                unchanged_since = unchanged_since.max(modseq);
            }
        }

        rejected
    }

    async fn store_flags(
        &mut self,
        unchanged_since: ModSeq,
        operation: char,
        flags: BitFlags<Flag>,
        sequence_set: &SequenceSet,
    ) -> (HashSet<Uid>, Option<ModSeq>) {
        let flags = Flag::format(flags).unwrap_or_default();
        let command = format!(
            "UID STORE {sequence_set} (UNCHANGEDSINCE {unchanged_since}) {operation}FLAGS.SILENT ({flags})"
        );
        log::debug!("{command}");

        let mut handler = Chain(StoreHandler::default(), &mut self.mailbox_handler);
        let response = self
            .connection
            .send(command.into_bytes(), &mut handler)
            .await
            .expect("sending of flag update should succeed");
        let modified = modified_uids(&response);
        if !modified.is_empty() {
            log::warn!("mails {modified:?} changed on server. Not updating their flags");
        }

        (modified, handler.0.highest_modseq)
    }

    pub async fn delete(&mut self, highest_modseq: ModSeq, sequence_set: &SequenceSet) {
        let (modified, _) = self
            .store_flags(highest_modseq, '+', Flag::Deleted.into(), sequence_set)
            .await;
        if !modified.is_empty() {
            log::warn!("mails {modified:?} changed on server. Not deleting them");
        }
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

#[derive(Default)]
struct MetadataCollector {
    metadata: Vec<RemoteMailMetadata>,
}

impl ResponseHandler for MetadataCollector {
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>> {
        std::future::ready(
            if let imap_proto::Response::Fetch(_, attributes) = response.parsed()
                && let [
                    imap_proto::AttributeValue::Uid(_),
                    imap_proto::AttributeValue::ModSeq(_),
                    imap_proto::AttributeValue::Flags(_),
                ] = attributes.as_slice()
            {
                self.metadata
                    .push(RemoteMailMetadata::from_fetch(attributes));
                None
            } else {
                Some(response)
            },
        )
    }
}

#[derive(Default)]
struct ModSeqTracker {
    highest_modseq: Option<ModSeq>,
}

impl ResponseHandler for ModSeqTracker {
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>> {
        if let imap_proto::Response::Fetch(_, attributes) = response.parsed() {
            for attribute in attributes {
                if let imap_proto::AttributeValue::ModSeq(modseq) = attribute {
                    let modseq = modseq.try_into().expect("received modseq should be valid");
                    self.highest_modseq = self.highest_modseq.max(Some(modseq));
                }
            }
        }
        std::future::ready(Some(response))
    }
}

#[derive(Default)]
struct StoreHandler {
    highest_modseq: Option<ModSeq>,
}

impl ResponseHandler for StoreHandler {
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>> {
        std::future::ready(
            if let imap_proto::Response::Fetch(_, attributes) = response.parsed()
                && let [
                    imap_proto::AttributeValue::Uid(_),
                    imap_proto::AttributeValue::ModSeq(modseq),
                ] = attributes.as_slice()
            {
                let modseq = modseq.try_into().expect("received modseq should be valid");
                self.highest_modseq = self.highest_modseq.max(Some(modseq));
                None
            } else {
                Some(response)
            },
        )
    }
}

fn modified_uids(response: &ResponseData) -> HashSet<Uid> {
    if let imap_proto::Response::Done {
        outcome:
            imap_proto::Outcome {
                code: None,
                information: Some(information),
            },
        ..
    } = response.parsed()
        && let Some(uids) = information
            .strip_prefix("[MODIFIED ")
            .and_then(|rest| rest.split_once(']'))
            .map(|(uids, _)| uids)
    {
        parse_uid_set(uids)
    } else {
        HashSet::new()
    }
}

fn parse_uid_set(uids: &str) -> HashSet<Uid> {
    let parse = |uid: &str| -> u32 { uid.parse().expect("received uid should be a number") };
    uids.split(',')
        .flat_map(|range| {
            let (start, end) = range.split_once(':').unwrap_or((range, range));
            let (start, end) = (parse(start), parse(end));
            start.min(end)..=start.max(end)
        })
        .map(|uid| Uid::try_from(uid).expect("received uid should be nonzero"))
        .collect()
}

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
                        imap_proto::AttributeValue::Uid(_),
                        imap_proto::AttributeValue::ModSeq(_),
                    ] => {
                        return Some(response);
                    }
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
                            let content = unsafe { transmute::<&[u8], &[u8]>(content.as_ref()) };
                            let content = RemoteContent::new(response.raw(), content);

                            Task::NewMail(RemoteMail::new(metadata, content))
                        } else {
                            unreachable!("mail without content")
                        }
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn test_imap_proto_keeps_modified_code_in_information() {
        let (_, response) = assertables::assert_ok!(imap_proto::Response::parse(
            b"0001 OK [MODIFIED 7,9] Conditional STORE failed\r\n"
        ));
        let imap_proto::Response::Done {
            outcome: imap_proto::Outcome { code, information },
            ..
        } = response
        else {
            panic!("response should be tagged");
        };
        assert!(code.is_none());
        assert_eq!(
            information.as_deref(),
            Some("[MODIFIED 7,9] Conditional STORE failed")
        );
    }

    #[test]
    fn test_parse_uid_set_parses_ranges() {
        let expected: HashSet<Uid> = [1, 3, 4, 5, 9]
            .into_iter()
            .map(|uid: u32| assertables::assert_ok!(Uid::try_from(uid)))
            .collect();

        assert_eq!(parse_uid_set("1,3:5,9"), expected);
    }
}
