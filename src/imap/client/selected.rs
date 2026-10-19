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

use crate::maildir::{LocalChanges, MaildirError};
use crate::{
    imap::{
        RemoteContent, RemoteMail, RemoteMailMetadata,
        client::Error,
        client::capability::{Capabilities, Capability},
        client::encoding,
        client::fetch::FetchAttributes,
        client::message_count::MessageCount,
        transport::{self, Chain, Connection, ResponseData, ResponseHandler},
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
    LocalError(MaildirError),
}

#[derive(Debug)]
pub struct SelectedClient {
    connection: Connection,
    mailbox_handler: MailboxHandler,
    idle_stop_rx: AsyncRx<Array<IdleStopReason>>,
    idle_stop_tx: Option<AsyncTx<Array<IdleStopReason>>>,
    multiappend: bool,
}
impl SelectedClient {
    pub fn new(
        connection: Connection,
        capabilities: &Capabilities,
        task_tx: MAsyncTx<mpsc::Array<Task>>,
        exists: u32,
    ) -> Result<Self, Error> {
        for capability in [Capability::LiteralPlus, Capability::UidPlus] {
            if !capabilities.contains(capability) {
                return Err(Error::MissingCapability(capability));
            }
        }
        let (idle_stop_tx, idle_stop_rx) = spsc::bounded_async(1);

        Ok(Self {
            connection,
            mailbox_handler: MailboxHandler {
                task_tx,
                message_count: MessageCount::new(exists),
            },
            idle_stop_rx,
            idle_stop_tx: Some(idle_stop_tx),
            multiappend: capabilities.contains(Capability::Multiappend),
        })
    }

    pub async fn fetch_mail(&mut self, sequence_set: &SequenceSet) -> Result<(), Error> {
        let command = format!("UID FETCH {sequence_set} (UID, ModSeq, FLAGS, BODY.PEEK[])");
        log::debug!("{command}");
        self.connection
            .send(command.into(), &mut self.mailbox_handler)
            .await?;

        Ok(())
    }

    pub async fn fetch_all(&mut self) -> Result<(), Error> {
        let command = "UID FETCH 1:* (UID, ModSeq, FLAGS, BODY.PEEK[])";
        log::debug!("{command}");
        self.connection
            .send(command.into(), &mut self.mailbox_handler)
            .await?;

        Ok(())
    }

    pub async fn fetch_changes_since(
        &mut self,
        modseq: ModSeq,
    ) -> Result<Vec<RemoteMailMetadata>, Error> {
        let command =
            format!("UID FETCH 1:* (UID, ModSeq, FLAGS) (CHANGEDSINCE {modseq} VANISHED)");
        self.collect_metadata(command).await
    }

    pub async fn fetch_metadata(
        &mut self,
        sequence_set: &SequenceSet,
    ) -> Result<Vec<RemoteMailMetadata>, Error> {
        self.collect_metadata(format!("UID FETCH {sequence_set} (UID, ModSeq, FLAGS)"))
            .await
    }

    pub async fn fetch_all_metadata(&mut self) -> Result<Vec<RemoteMailMetadata>, Error> {
        self.collect_metadata("UID FETCH 1:* (UID, ModSeq, FLAGS)".to_string())
            .await
    }

    async fn collect_metadata(
        &mut self,
        command: String,
    ) -> Result<Vec<RemoteMailMetadata>, Error> {
        log::debug!("{command}");
        let mut handler = Chain(MetadataCollector::default(), &mut self.mailbox_handler);
        self.connection.send(command.into(), &mut handler).await?;
        let MetadataCollector {
            metadata,
            highest_sequence,
        } = handler.0;
        self.mailbox_handler
            .message_count
            .on_fetch(highest_sequence);

        Ok(metadata)
    }

    pub async fn complete_initial_sync(&mut self, modseq: ModSeq) -> Result<(), Error> {
        let (done_tx, done_rx) = oneshot();
        self.mailbox_handler
            .task_tx
            .send(Task::CompleteInitialSync(modseq, done_tx))
            .await
            .map_err(|_| Error::TaskProcessingStopped)?;
        done_rx.await.map_err(|_| Error::TaskProcessingStopped)
    }

    pub async fn commit_highest_modseq(&mut self, modseq: ModSeq) -> Result<(), Error> {
        self.mailbox_handler
            .task_tx
            .send(Task::SetHighestModseq(modseq))
            .await
            .map_err(|_| Error::TaskProcessingStopped)
    }

    pub async fn store(
        &mut self,
        mailbox: &str,
        mails: Vec<LocalMail>,
    ) -> Result<Vec<(Uid, NewLocalMailMetadata)>, Error> {
        let mailbox = encoding::mailbox(mailbox);
        if self.multiappend && mails.len() > 1 {
            match self.append(&mailbox, &mails).await {
                Ok(uids) => return Ok(uids.into_iter().zip(Self::metadatas(mails)).collect()),
                Err(Error::Transport(transport::Error::No(reason))) => {
                    log::warn!(
                        "server rejected appending {} mails ({}), appending them one by one",
                        mails.len(),
                        reason.as_deref().unwrap_or("NO")
                    );
                }
                Err(e) => return Err(e),
            }
        }

        let mut stored = Vec::with_capacity(mails.len());
        for mail in mails {
            match self.append(&mailbox, std::slice::from_ref(&mail)).await {
                Ok(uids) => stored.extend(uids.into_iter().zip(Some(mail.unpack().0))),
                Err(Error::Transport(transport::Error::No(reason))) => {
                    log::warn!(
                        "server rejected mail {} ({}), skipping it until next sync",
                        mail.metadata(),
                        reason.as_deref().unwrap_or("NO")
                    );
                }
                Err(e) => return Err(e),
            }
        }

        Ok(stored)
    }

    fn metadatas(mails: Vec<LocalMail>) -> impl Iterator<Item = NewLocalMailMetadata> {
        mails.into_iter().map(|mail| mail.unpack().0)
    }

    async fn append(&mut self, mailbox: &str, mails: &[LocalMail]) -> Result<Vec<Uid>, Error> {
        log::debug!("APPEND {mailbox}");
        let mut headers = b"APPEND ".to_vec();
        headers.extend_from_slice(mailbox.as_bytes());
        let mut header_ends = Vec::with_capacity(mails.len());
        for mail in mails {
            mail.write_append_header(&mut headers);
            header_ends.push(headers.len());
        }
        let mut parts = Vec::with_capacity(mails.len() * 2);
        let mut header_start = 0;
        for (mail, header_end) in mails.iter().zip(header_ends) {
            parts.push(&headers[header_start..header_end]);
            parts.push(mail.content());
            header_start = header_end;
        }

        let response = self
            .connection
            .send_parts(&parts, &mut self.mailbox_handler)
            .await?;

        let Some(imap_proto::ResponseCode::AppendUid(_uid_validity, uid_set_members)) =
            response.unsafe_get_tagged_response_code()
        else {
            return Err(Error::MissingAppendUid);
        };
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

        let ranges = uid_set_members
            .iter()
            .map(SequenceRange::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ranges.into_iter().flatten().collect())
    }

    pub async fn store_flag_change(
        &mut self,
        highest_modseq: ModSeq,
        change: FlagChange,
        sequence_set: &SequenceSet,
    ) -> Result<HashSet<Uid>, Error> {
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
                    .await?;
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

        Ok(rejected)
    }

    async fn store_flags(
        &mut self,
        unchanged_since: ModSeq,
        operation: char,
        flags: BitFlags<Flag>,
        sequence_set: &SequenceSet,
    ) -> Result<(HashSet<Uid>, HashMap<Uid, ModSeq>), Error> {
        let flags = Flag::format(flags).unwrap_or_default();
        let command = format!(
            "UID STORE {sequence_set} (UNCHANGEDSINCE {unchanged_since}) {operation}FLAGS.SILENT ({flags})"
        );
        log::debug!("{command}");

        let mut handler = Chain(StoreHandler::default(), &mut self.mailbox_handler);
        let response = self
            .connection
            .send(command.into_bytes(), &mut handler)
            .await?;
        let modified = modified_uids(&response)?;
        if !modified.is_empty() {
            log::warn!("mails {modified:?} changed on server. Not updating their flags");
        }

        Ok((modified, handler.0.modseqs))
    }

    pub async fn delete(
        &mut self,
        highest_modseq: ModSeq,
        sequence_set: &SequenceSet,
    ) -> Result<HashSet<Uid>, Error> {
        let (modified, _) = self
            .store_flags(highest_modseq, '+', Flag::Deleted.into(), sequence_set)
            .await?;
        if !modified.is_empty() {
            log::warn!("mails {modified:?} changed on server. Not deleting them");
        }
        let command = format!("UID EXPUNGE {sequence_set}");
        log::debug!("{command}");
        self.connection
            .send(command.into_bytes(), &mut self.mailbox_handler)
            .await?;

        Ok(modified)
    }

    pub async fn logout(mut self) -> Result<(), Error> {
        let command = "LOGOUT";
        log::debug!("{command}");
        self.connection
            .send(command.into(), &mut self.mailbox_handler)
            .await?;

        Ok(())
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

    pub async fn idle(&mut self, timeout: Duration) -> Result<IdleStopReason, Error> {
        if self.mailbox_handler.message_count.has_unknown() {
            log::debug!("not idling due to unfetched remote mail");
            return Ok(IdleStopReason::Remote);
        }
        let command = "IDLE";
        log::debug!("{command}");
        let deadline = Instant::now() + timeout;
        let connection = &mut self.connection;
        let idle_stop_rx = &self.idle_stop_rx;
        let handler = &mut self.mailbox_handler;

        let tag = connection.write_command(command.into()).await?;
        loop {
            let response = connection.next_response().await?;
            match response.parsed() {
                imap_proto::Response::Continue(_) => {
                    log::trace!("idling for up to {} seconds", timeout.as_secs());
                    break;
                }
                imap_proto::Response::Done { .. } => return Err(Error::IdleRejected),
                _ => handler.handle_or_log(response).await?,
            }
        }

        let stop_reason = loop {
            if handler.message_count.has_unknown() {
                log::trace!("stopping idle");
                break IdleStopReason::Remote;
            }
            let event = compio::time::timeout_at(deadline, async {
                futures::select_biased! {
                    stop = idle_stop_rx.recv().fuse() => IdleEvent::Stop(stop.ok()),
                    response = connection.next_response().fuse() => IdleEvent::Response(response),
                }
            })
            .await;
            match event {
                Ok(IdleEvent::Stop(Some(reason))) => break reason,
                Ok(IdleEvent::Stop(None)) => return Err(Error::IdleStopClosed),
                Ok(IdleEvent::Response(response)) => {
                    handler.handle_or_log(response?).await?;
                }
                Err(_) => {
                    log::debug!("idle timed out");
                    break IdleStopReason::Timeout;
                }
            }
        };

        let command = "DONE";
        log::debug!("{command}");
        connection.write_continuation(command.into()).await?;
        connection.receive_completion(&tag, handler).await?;

        Ok(stop_reason)
    }
}

enum IdleEvent {
    Stop(Option<IdleStopReason>),
    Response(Result<ResponseData, transport::Error>),
}

#[derive(Default)]
struct MetadataCollector {
    metadata: Vec<RemoteMailMetadata>,
    highest_sequence: u32,
}

impl ResponseHandler for MetadataCollector {
    type Error = Error;

    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>> {
        std::future::ready(self.collect(response))
    }
}

impl MetadataCollector {
    fn collect(&mut self, response: ResponseData) -> Result<Option<ResponseData>, Error> {
        if let imap_proto::Response::Fetch(sequence, attributes) = response.parsed()
            && let parsed = FetchAttributes::parse(attributes)
            && parsed.body.is_none()
            && let Some(metadata) = parsed.metadata()?
        {
            self.highest_sequence = self.highest_sequence.max(*sequence);
            self.metadata.push(metadata);
            Ok(None)
        } else {
            Ok(Some(response))
        }
    }
}

#[derive(Default)]
struct StoreHandler {
    modseqs: HashMap<Uid, ModSeq>,
}

impl ResponseHandler for StoreHandler {
    type Error = Error;

    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>> {
        std::future::ready(self.collect(response))
    }
}

impl StoreHandler {
    fn collect(&mut self, response: ResponseData) -> Result<Option<ResponseData>, Error> {
        if let imap_proto::Response::Fetch(_, attributes) = response.parsed()
            && let FetchAttributes {
                uid: Some(uid),
                modseq: Some(modseq),
                flags: None,
                body: None,
            } = FetchAttributes::parse(attributes)
        {
            self.modseqs.insert(
                uid.try_into()?,
                modseq.try_into().map_err(Error::InvalidModSeq)?,
            );
            Ok(None)
        } else {
            Ok(Some(response))
        }
    }
}

fn modified_uids(response: &ResponseData) -> Result<HashSet<Uid>, Error> {
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
        Ok(HashSet::new())
    }
}

fn parse_uid_set(uids: &str) -> Result<HashSet<Uid>, Error> {
    let parse = |uid: &str| -> Result<u32, Error> {
        uid.parse()
            .map_err(|_| Error::InvalidUidSet(uids.to_string()))
    };
    let mut parsed = HashSet::new();
    for range in uids.split(',') {
        let (start, end) = range.split_once(':').unwrap_or((range, range));
        let (start, end) = (parse(start)?, parse(end)?);
        for uid in start.min(end)..=start.max(end) {
            parsed.insert(Uid::try_from(uid)?);
        }
    }

    Ok(parsed)
}

#[derive(Debug)]
struct MailboxHandler {
    task_tx: MAsyncTx<mpsc::Array<Task>>,
    message_count: MessageCount,
}

impl ResponseHandler for MailboxHandler {
    type Error = Error;

    async fn handle(&mut self, response: ResponseData) -> Result<Option<ResponseData>, Error> {
        match response.parsed() {
            imap_proto::Response::Fetch(sequence, attributes) => {
                self.message_count.on_fetch(*sequence);
                let parsed = FetchAttributes::parse(attributes);
                log::trace!("FETCH {parsed:?}");
                let task = match (parsed.metadata()?, parsed.body) {
                    (Some(metadata), Some(Some(content))) => {
                        let content = RemoteContent::new(response.bytes_of(content));
                        Task::NewMail(RemoteMail::new(metadata, content))
                    }
                    (Some(metadata), Some(None)) => {
                        return Err(Error::MissingContent(metadata.uid()));
                    }
                    (Some(metadata), None) => Task::UpdateFlags(metadata),
                    (None, _) => return Ok(Some(response)),
                };
                self.send(task).await?;
                Ok(None)
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
                self.send(Task::Delete(uids.try_into()?)).await?;
                Ok(None)
            }
            imap_proto::Response::MailboxData(imap_proto::MailboxDatum::Exists(exists)) => {
                log::trace!("mailbox contains {exists} messages");
                self.message_count.on_exists(*exists);
                Ok(None)
            }
            imap_proto::Response::Expunge(_) => {
                self.message_count.on_expunge(1);
                Ok(None)
            }
            _ => Ok(Some(response)),
        }
    }
}

impl MailboxHandler {
    async fn send(&self, task: Task) -> Result<(), Error> {
        self.task_tx
            .send(task)
            .await
            .map_err(|_| Error::TaskProcessingStopped)
    }
}

impl LocalMail {
    fn write_append_header(&self, command: &mut Vec<u8>) {
        if let Some(flags) = Flag::format(self.metadata().flags()) {
            write!(command, " ({flags})")
                .expect("appending formatted flags to APPEND command should succeed");
        }
        write!(command, " {{{}+}}\r\n", self.content().len())
            .expect("appending content length to APPEND command should succeed");
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

        assert_eq!(assertables::assert_ok!(parse_uid_set("1,3:5,9")), expected);
    }
}
