use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::time::{Duration, Instant};

use crossfire::{
    AsyncRx, AsyncTx, MAsyncTx, mpsc,
    oneshot::oneshot,
    spsc::{self, Array},
};
use enumflags2::BitFlags;
use futures::FutureExt as _;

use crate::maildir::LocalChanges;
use crate::{
    imap::{
        RemoteContent, RemoteMail, RemoteMailMetadata,
        client::capability::{Capabilities, Capability},
        client::encoding,
        client::fetch::FetchAttributes,
        client::message_count::MessageCount,
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
    Rescan,
}

#[derive(Debug)]
pub struct SelectedClient {
    connection: Connection,
    mailbox_handler: MailboxHandler,
    idle_stop_rx: AsyncRx<Array<IdleStopReason>>,
    idle_stop_tx: Option<AsyncTx<Array<IdleStopReason>>>,
    multiappend: bool,
    idle: bool,
}
impl SelectedClient {
    pub fn new(
        connection: Connection,
        capabilities: &Capabilities,
        task_tx: MAsyncTx<mpsc::Array<Task>>,
        exists: u32,
    ) -> Self {
        assert!(
            capabilities.contains(Capability::LiteralPlus),
            "server should support LITERAL+ capability"
        );
        assert!(
            capabilities.contains(Capability::UidPlus),
            "server should support UIDPLUS capability"
        );
        let (idle_stop_tx, idle_stop_rx) = spsc::bounded_async(1);

        Self {
            connection,
            mailbox_handler: MailboxHandler {
                task_tx,
                message_count: MessageCount::new(exists),
            },
            idle_stop_rx,
            idle_stop_tx: Some(idle_stop_tx),
            multiappend: capabilities.contains(Capability::Multiappend),
            idle: capabilities.contains(Capability::Idle),
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

    pub async fn fetch_changes_since(&mut self, modseq: ModSeq) -> Vec<RemoteMailMetadata> {
        let command =
            format!("UID FETCH 1:* (UID, ModSeq, FLAGS) (CHANGEDSINCE {modseq} VANISHED)");
        self.collect_metadata(command).await
    }

    pub async fn fetch_metadata(&mut self, sequence_set: &SequenceSet) -> Vec<RemoteMailMetadata> {
        self.collect_metadata(format!("UID FETCH {sequence_set} (UID, ModSeq, FLAGS)"))
            .await
    }

    pub async fn fetch_all_metadata(&mut self) -> Vec<RemoteMailMetadata> {
        self.collect_metadata("UID FETCH 1:* (UID, ModSeq, FLAGS)".to_string())
            .await
    }

    async fn collect_metadata(&mut self, command: String) -> Vec<RemoteMailMetadata> {
        log::debug!("{command}");
        let mut handler = Chain(MetadataCollector::default(), &mut self.mailbox_handler);
        self.connection
            .send(command.into(), &mut handler)
            .await
            .expect("fetching mail metadata should succeed");
        let MetadataCollector {
            metadata,
            highest_sequence,
        } = handler.0;
        self.mailbox_handler
            .message_count
            .on_fetch(highest_sequence);

        metadata
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
        mails: impl Iterator<Item = LocalMail>,
    ) -> Vec<(Uid, NewLocalMailMetadata)> {
        let mailbox = encoding::mailbox(mailbox);
        let mut stored = Vec::new();
        let mut metadatas = Vec::new();
        if self.multiappend {
            let mut mails = mails.peekable();
            if mails.peek().is_some() {
                self.append(&mailbox, mails, &mut metadatas, &mut stored)
                    .await;
            }
        } else {
            for mail in mails {
                self.append(&mailbox, std::iter::once(mail), &mut metadatas, &mut stored)
                    .await;
            }
        }

        stored
    }

    async fn append(
        &mut self,
        mailbox: &str,
        mails: impl Iterator<Item = LocalMail>,
        metadatas: &mut Vec<NewLocalMailMetadata>,
        stored: &mut Vec<(Uid, NewLocalMailMetadata)>,
    ) {
        log::debug!("APPEND {mailbox}");
        let mut command = b"APPEND ".to_vec();
        command.extend_from_slice(mailbox.as_bytes());
        metadatas.extend(mails.map(|mail| mail.append_to(&mut command)));

        let response = self
            .connection
            .send(command, &mut self.mailbox_handler)
            .await
            // todo: check reason and move failing mails out of cur
            .expect("storing new mail should succeed");

        if let Some(imap_proto::ResponseCode::AppendUid(_uid_validity, uid_set_members)) =
            response.unsafe_get_tagged_response_code()
        {
            let appended = uid_set_members
                .iter()
                .map(|member| match member {
                    imap_proto::rfc4315::UidSetMember::UidRange(range) => {
                        range.end() - range.start() + 1
                    }
                    imap_proto::rfc4315::UidSetMember::Uid(_) => 1,
                })
                .sum();
            self.mailbox_handler.message_count.on_append(appended);

            let uids = uid_set_members.iter().flat_map(|member| {
                SequenceRange::try_from(member).expect("received uids should be valid")
            });
            stored.extend(uids.zip(metadatas.drain(..)));
        } else {
            unreachable!("response to APPEND should have AppendUid response code");
        }
    }

    pub async fn store_flag_change(
        &mut self,
        highest_modseq: ModSeq,
        change: FlagChange,
        sequence_set: &SequenceSet,
    ) -> HashSet<Uid> {
        let mut rejected = HashSet::new();
        let mut pending: HashMap<Uid, ModSeq> = sequence_set
            .iter()
            .map(|uid| (uid, highest_modseq))
            .collect();
        for (operation, flags) in [('+', change.added), ('-', change.removed)] {
            if flags.is_empty() {
                continue;
            }
            let mut groups: HashMap<ModSeq, Vec<Uid>> = HashMap::new();
            for (uid, modseq) in &pending {
                groups.entry(*modseq).or_default().push(*uid);
            }
            for (unchanged_since, uids) in groups {
                let set = SequenceSet::try_from(&uids).expect("grouped uids should not be empty");
                let (modified, modseqs) = self
                    .store_flags(unchanged_since, operation, flags, &set)
                    .await;
                for (uid, modseq) in modseqs {
                    if let Some(pending_modseq) = pending.get_mut(&uid) {
                        *pending_modseq = modseq;
                    }
                }
                for uid in modified {
                    pending.remove(&uid);
                    rejected.insert(uid);
                }
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
    ) -> (HashSet<Uid>, HashMap<Uid, ModSeq>) {
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

        (modified, handler.0.modseqs)
    }

    pub async fn delete(
        &mut self,
        highest_modseq: ModSeq,
        sequence_set: &SequenceSet,
    ) -> HashSet<Uid> {
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

        modified
    }

    pub async fn logout(mut self) {
        let command = "LOGOUT";
        log::debug!("{command}");
        self.connection
            .send(command.into(), &mut self.mailbox_handler)
            .await
            .expect("logging out should succeed");
    }

    /// Sender to stop idling with a local reason. Can only be taken once.
    ///
    /// # Panics
    /// If the sender already got taken
    pub fn take_idle_stop_tx(&mut self) -> AsyncTx<Array<IdleStopReason>> {
        self.idle_stop_tx
            .take()
            .expect("idle stop sender should only be taken once")
    }

    async fn poll(&self, deadline: Instant) -> IdleStopReason {
        match compio::time::timeout_at(deadline, self.idle_stop_rx.recv()).await {
            Ok(stop) => stop.expect("idle stop channel should still be open"),
            Err(_) => IdleStopReason::Remote,
        }
    }

    pub async fn idle(&mut self, timeout: Duration) -> IdleStopReason {
        if self.mailbox_handler.message_count.has_unknown() {
            log::debug!("not idling due to unfetched remote mail");
            return IdleStopReason::Remote;
        }
        let deadline = Instant::now() + timeout;
        if !self.idle {
            log::debug!("server does not support IDLE. Polling instead");
            return self.poll(deadline).await;
        }
        let command = "IDLE";
        log::debug!("{command}");
        let connection = &mut self.connection;
        let idle_stop_rx = &self.idle_stop_rx;
        let handler = &mut self.mailbox_handler;

        let tag = connection.write_command(command.into()).await;
        loop {
            let response = connection.next_response().await;
            match response.parsed() {
                imap_proto::Response::Continue(_) => {
                    log::trace!("idling for up to {} seconds", timeout.as_secs());
                    break;
                }
                imap_proto::Response::Done { .. } => {
                    log::warn!("server rejected IDLE. Polling instead");
                    return self.poll(deadline).await;
                }
                _ => handler.handle_or_log(response).await,
            }
        }

        let stop_reason = loop {
            if handler.message_count.has_unknown() {
                log::trace!("stopping idle");
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
                    handler.handle_or_log(response).await;
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
            .receive_completion(&tag, handler)
            .await
            .expect("idle should succeed");

        stop_reason
    }
}

enum IdleEvent {
    Stop(IdleStopReason),
    Response(ResponseData),
}

#[derive(Default)]
struct MetadataCollector {
    metadata: Vec<RemoteMailMetadata>,
    highest_sequence: u32,
}

impl ResponseHandler for MetadataCollector {
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>> {
        std::future::ready(
            if let imap_proto::Response::Fetch(sequence, attributes) = response.parsed()
                && let parsed = FetchAttributes::parse(attributes)
                && parsed.body.is_none()
                && let Some(metadata) = parsed.metadata()
            {
                self.highest_sequence = self.highest_sequence.max(*sequence);
                self.metadata.push(metadata);
                None
            } else {
                Some(response)
            },
        )
    }
}

#[derive(Default)]
struct StoreHandler {
    modseqs: HashMap<Uid, ModSeq>,
}

impl ResponseHandler for StoreHandler {
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>> {
        std::future::ready(
            if let imap_proto::Response::Fetch(_, attributes) = response.parsed()
                && let FetchAttributes {
                    uid: Some(uid),
                    modseq: Some(modseq),
                    flags: None,
                    body: None,
                } = FetchAttributes::parse(attributes)
            {
                self.modseqs.insert(
                    uid.try_into().expect("received uid should be valid"),
                    modseq.try_into().expect("received modseq should be valid"),
                );
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
    task_tx: MAsyncTx<mpsc::Array<Task>>,
    message_count: MessageCount,
}

impl ResponseHandler for MailboxHandler {
    async fn handle(&mut self, response: ResponseData) -> Option<ResponseData> {
        match response.parsed() {
            imap_proto::Response::Fetch(sequence, attributes) => {
                self.message_count.on_fetch(*sequence);
                let parsed = FetchAttributes::parse(attributes);
                log::trace!("FETCH {parsed:?}");
                let task = match (parsed.metadata(), parsed.body) {
                    (Some(metadata), Some(Some(content))) => {
                        let content = RemoteContent::new(response.bytes_of(content));
                        Task::NewMail(RemoteMail::new(metadata, content))
                    }
                    (Some(_), Some(None)) => unreachable!("mail without content"),
                    (Some(metadata), None) => Task::UpdateFlags(metadata),
                    (None, _) => return Some(response),
                };
                self.task_tx
                    .send(task)
                    .await
                    .expect("task channel should still be open");
                None
            }
            imap_proto::Response::Vanished { earlier, uids } => {
                log::trace!("VANISHED earlier {earlier:?} uids: {uids:?}");
                if !earlier {
                    let count = uids
                        .iter()
                        .map(|range| range.end() - range.start() + 1)
                        .sum();
                    self.message_count.on_expunge(count);
                }
                self.task_tx
                    .send(Task::Delete(
                        uids.try_into()
                            .expect("received uid ranges should start with valid uid"),
                    ))
                    .await
                    .expect("task channel should still be open");
                None
            }
            imap_proto::Response::MailboxData(imap_proto::MailboxDatum::Exists(exists)) => {
                log::trace!("mailbox contains {exists} messages");
                self.message_count.on_exists(*exists);
                None
            }
            imap_proto::Response::Expunge(_) => {
                self.message_count.on_expunge(1);
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
