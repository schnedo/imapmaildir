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
        client::uid_map::UidMap,
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

/// Bounds the commands sent before receiving completions, so that neither side blocks on full
/// socket buffers while the other is still sending
const MAX_PIPELINED_COMMANDS: usize = 64;

#[derive(Debug)]
pub struct SelectedClient {
    connection: Connection,
    mailbox_handler: MailboxHandler,
    idle_stop_rx: AsyncRx<Array<IdleStopReason>>,
    idle_stop_tx: Option<AsyncTx<Array<IdleStopReason>>>,
    multiappend: bool,
    own_modseqs: OwnModSeqs,
}
impl SelectedClient {
    pub fn new(
        connection: Connection,
        capabilities: &Capabilities,
        qresync: bool,
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
                tracking: (!qresync).then(|| SequenceTracking::new(exists)),
            },
            idle_stop_rx,
            idle_stop_tx: Some(idle_stop_tx),
            multiappend: capabilities.contains(Capability::Multiappend),
            own_modseqs: OwnModSeqs::default(),
        })
    }

    pub fn supports_qresync(&self) -> bool {
        self.mailbox_handler.tracking.is_none()
    }

    /// Whether remote mails got expunged that could not be resolved to UIDs since the last call.
    /// Requires comparing [`Self::remote_uids`] with local mails to detect deletions.
    pub fn take_unknown_expunged(&mut self) -> bool {
        self.mailbox_handler
            .tracking
            .as_mut()
            .is_some_and(|tracking| std::mem::take(&mut tracking.unknown_expunged))
    }

    /// Fetches all UIDs of the selected mailbox in ascending order. Errors if the result is
    /// implausible, as it would delete all local mails. Pass them to [`Self::seed_remote_uids`]
    /// afterwards.
    pub async fn remote_uids(&mut self) -> Result<Vec<Uid>, Error> {
        let command = "UID SEARCH ALL";
        log::debug!("{command}");
        if let Some(tracking) = &mut self.mailbox_handler.tracking {
            tracking.unknown_expunged = false;
            tracking.uid_map.begin_command();
        }
        let mut handler = Chain(SearchCollector::default(), &mut self.mailbox_handler);
        self.connection.send(command.into(), &mut handler).await?;
        let uids = handler.0.into_sorted_uids();

        plausible_search_result(uids, self.mailbox_handler.message_count.exists())
    }

    /// Replaces the sequence to UID map with the result of [`Self::remote_uids`], taking ownership
    /// to avoid copying it. Must be called before any other command, as the result is only
    /// consistent with the responses received up to the search.
    pub fn seed_remote_uids(&mut self, uids: Vec<Uid>) {
        let exists = self.mailbox_handler.message_count.exists();
        self.mailbox_handler.seed_uids(uids, exists);
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
        let vanished = if self.supports_qresync() {
            " VANISHED"
        } else {
            ""
        };
        let command =
            format!("UID FETCH 1:* (UID, ModSeq, FLAGS) (CHANGEDSINCE {modseq}{vanished})");
        if let Some(tracking) = &mut self.mailbox_handler.tracking {
            tracking.unsolicited_change = false;
        }
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
        let collector = MetadataCollector {
            sequences: self.mailbox_handler.needs_uids().then(Vec::new),
            ..MetadataCollector::default()
        };
        if let Some(tracking) = &mut self.mailbox_handler.tracking {
            tracking.uid_map.begin_command();
        }
        let mut handler = Chain(collector, &mut self.mailbox_handler);
        self.connection.send(command.into(), &mut handler).await?;
        let MetadataCollector {
            metadata,
            highest_sequence,
            sequences,
        } = handler.0;
        self.mailbox_handler
            .message_count
            .on_fetch(highest_sequence);
        if let Some(sequences) = sequences {
            self.mailbox_handler.apply_fetched_uids(sequences);
        }

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
        self.own_modseqs.prune(modseq);
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
        let mut pending = self
            .own_modseqs
            .unchanged_since(highest_modseq, sequence_set);
        let mut rejected = HashSet::new();
        for (operation, flags) in [('+', change.added), ('-', change.removed)] {
            if flags.is_empty() {
                continue;
            }
            rejected.extend(
                self.store_flags_grouped(&mut pending, operation, flags)
                    .await?,
            );
        }

        Ok(rejected)
    }

    /// Stores flags of `pending` mails grouped by their UNCHANGEDSINCE modseq, pipelining the
    /// commands of all groups. Updates the modseqs of stored mails and removes rejected ones,
    /// returning them.
    async fn store_flags_grouped(
        &mut self,
        pending: &mut HashMap<Uid, ModSeq>,
        operation: char,
        flags: BitFlags<Flag>,
    ) -> Result<HashSet<Uid>, Error> {
        let mut groups: HashMap<ModSeq, Vec<Uid>> = HashMap::new();
        for (uid, modseq) in pending.iter() {
            groups.entry(*modseq).or_default().push(*uid);
        }
        let groups: Vec<_> = groups
            .into_iter()
            .map(|(unchanged_since, uids)| {
                let set = SequenceSet::try_from(&uids).expect("grouped uids should not be empty");
                (unchanged_since, set)
            })
            .collect();
        let mut rejected = HashSet::new();
        for groups in groups.chunks(MAX_PIPELINED_COMMANDS) {
            let (modified, modseqs) = self.store_flags(groups, operation, flags).await?;
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

        Ok(rejected)
    }

    /// Pipelines a `UID STORE` per group of UNCHANGEDSINCE modseq and UIDs. Pipelining is safe,
    /// as the groups are disjoint.
    async fn store_flags(
        &mut self,
        groups: &[(ModSeq, SequenceSet)],
        operation: char,
        flags: BitFlags<Flag>,
    ) -> Result<(HashSet<Uid>, HashMap<Uid, ModSeq>), Error> {
        let flags = Flag::format(flags).unwrap_or_default();
        let commands = groups
            .iter()
            .map(|(unchanged_since, sequence_set)| {
                let command = format!(
                    "UID STORE {sequence_set} (UNCHANGEDSINCE {unchanged_since}) {operation}FLAGS.SILENT ({flags})"
                );
                log::debug!("{command}");
                command.into_bytes()
            })
            .collect();

        let store_handler = StoreHandler {
            sequences: self.mailbox_handler.needs_uids().then(Vec::new),
            ..StoreHandler::default()
        };
        if let Some(tracking) = &mut self.mailbox_handler.tracking {
            tracking.uid_map.begin_command();
        }
        let mut handler = Chain(store_handler, &mut self.mailbox_handler);
        let responses = self
            .connection
            .send_pipelined(commands, &mut handler)
            .await?;
        let StoreHandler { modseqs, sequences } = handler.0;
        self.own_modseqs.record(&modseqs);
        // learning UIDs of e.g. own uploads lets a following UID EXPUNGE resolve without rescan
        if let Some(sequences) = sequences {
            self.mailbox_handler.apply_fetched_uids(sequences);
        }
        let mut modified = HashSet::new();
        for response in &responses {
            modified.extend(modified_uids(response)?);
        }
        if !modified.is_empty() {
            log::warn!("mails {modified:?} changed on server. Not updating their flags");
        }

        Ok((modified, modseqs))
    }

    pub async fn delete(
        &mut self,
        highest_modseq: ModSeq,
        sequence_set: &SequenceSet,
    ) -> Result<HashSet<Uid>, Error> {
        let mut pending = self
            .own_modseqs
            .unchanged_since(highest_modseq, sequence_set);
        let modified = self
            .store_flags_grouped(&mut pending, '+', Flag::Deleted.into())
            .await?;
        if !modified.is_empty() {
            log::warn!("mails {modified:?} changed on server. Not deleting them");
        }
        let mut accepted: Vec<Uid> = pending.into_keys().collect();
        accepted.sort_unstable();
        let Ok(accepted) = SequenceSet::from_sorted(accepted) else {
            return Ok(modified);
        };
        let command = format!("UID EXPUNGE {accepted}");
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
        if self.mailbox_handler.needs_remote_sync() {
            log::debug!("not idling due to unsynced remote changes");
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
            if handler.needs_remote_sync() {
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
    /// Only collected while tracking the sequence to UID map
    sequences: Option<Vec<(u32, Uid)>>,
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
            if let Some(sequences) = &mut self.sequences {
                sequences.push((*sequence, metadata.uid()));
            }
            self.metadata.push(metadata);
            Ok(None)
        } else {
            Ok(Some(response))
        }
    }
}

fn plausible_search_result(uids: Vec<Uid>, exists: u32) -> Result<Vec<Uid>, Error> {
    if uids.is_empty() && exists > 0 {
        Err(Error::ImplausibleSearchResult(exists))
    } else {
        Ok(uids)
    }
}

#[derive(Default)]
struct SearchCollector {
    uids: Vec<Uid>,
}

impl ResponseHandler for SearchCollector {
    type Error = Error;

    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>> {
        std::future::ready(self.collect(response))
    }
}

impl SearchCollector {
    /// SEARCH results are unordered and may be split across multiple responses
    fn into_sorted_uids(mut self) -> Vec<Uid> {
        self.uids.sort_unstable();
        self.uids.dedup();
        self.uids
    }

    fn collect(&mut self, mut response: ResponseData) -> Result<Option<ResponseData>, Error> {
        let Some(uids) = response.take_search() else {
            return Ok(Some(response));
        };
        // collects in place, as Uid has the layout of u32
        let uids = uids
            .into_iter()
            .map(Uid::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        if self.uids.is_empty() {
            self.uids = uids;
        } else {
            self.uids.extend(uids);
        }

        Ok(None)
    }
}

/// Modseqs of mails changed by own STOREs that may exceed the committed highest modseq. Using
/// them as UNCHANGEDSINCE avoids rejecting follow-up changes to the same mail, as the server does
/// not report own changes. Session-only, as selecting commits the server's highest modseq.
#[derive(Debug, Default)]
struct OwnModSeqs {
    modseqs: HashMap<Uid, ModSeq>,
}

impl OwnModSeqs {
    fn record(&mut self, modseqs: &HashMap<Uid, ModSeq>) {
        self.modseqs.extend(modseqs);
    }

    /// Forgets modseqs covered by `highest_modseq`
    fn prune(&mut self, highest_modseq: ModSeq) {
        self.modseqs.retain(|_, modseq| *modseq > highest_modseq);
    }

    fn unchanged_since(
        &self,
        highest_modseq: ModSeq,
        sequence_set: &SequenceSet,
    ) -> HashMap<Uid, ModSeq> {
        sequence_set
            .iter()
            .map(|uid| {
                let own = self.modseqs.get(&uid).copied();
                (
                    uid,
                    own.map_or(highest_modseq, |own| own.max(highest_modseq)),
                )
            })
            .collect()
    }
}

#[derive(Default)]
struct StoreHandler {
    modseqs: HashMap<Uid, ModSeq>,
    /// Only collected while tracking the sequence to UID map
    sequences: Option<Vec<(u32, Uid)>>,
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
        if let imap_proto::Response::Fetch(sequence, attributes) = response.parsed()
            && let FetchAttributes {
                uid: Some(uid),
                modseq: Some(modseq),
                flags: None,
                body: None,
            } = FetchAttributes::parse(attributes)
        {
            let uid = uid.try_into()?;
            self.modseqs
                .insert(uid, modseq.try_into().map_err(Error::InvalidModSeq)?);
            if let Some(sequences) = &mut self.sequences {
                sequences.push((*sequence, uid));
            }
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
    /// Only tracked without QRESYNC, as VANISHED and FETCH responses carry UIDs otherwise
    tracking: Option<SequenceTracking>,
}

/// Resolves sequence numbers of unsolicited responses to UIDs without QRESYNC
#[derive(Debug)]
struct SequenceTracking {
    uid_map: UidMap,
    /// Remote mails got expunged that could not be resolved to UIDs
    unknown_expunged: bool,
    /// Remote flags changed on mails that could not be resolved to UIDs
    unsolicited_change: bool,
}

impl SequenceTracking {
    fn new(exists: u32) -> Self {
        Self {
            uid_map: UidMap::new(exists),
            unknown_expunged: false,
            unsolicited_change: false,
        }
    }

    fn needs_remote_sync(&self) -> bool {
        self.unknown_expunged || self.unsolicited_change
    }
}

impl ResponseHandler for MailboxHandler {
    type Error = Error;

    async fn handle(&mut self, response: ResponseData) -> Result<Option<ResponseData>, Error> {
        match response.parsed() {
            imap_proto::Response::Fetch(sequence, attributes) => {
                self.message_count.on_fetch(*sequence);
                let attributes = FetchAttributes::parse(attributes);
                log::trace!("FETCH {attributes:?}");
                let metadata = if let Some(tracking) = &mut self.tracking {
                    let uid = attributes.uid.map(Uid::try_from).transpose()?;
                    let resolved = tracking.uid_map.on_fetch(*sequence, uid);
                    let metadata = attributes.metadata_with_uid(resolved.or(uid))?;
                    if metadata.is_none() && attributes.uid.is_none() && attributes.flags.is_some()
                    {
                        log::debug!("cannot resolve uid of mail {sequence} with changed flags");
                        tracking.unsolicited_change = true;
                        return Ok(None);
                    }
                    metadata
                } else {
                    attributes.metadata()?
                };
                let task = match (metadata, attributes.body) {
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
                if let Some(tracking) = &mut self.tracking {
                    tracking.uid_map.on_exists(*exists);
                }
                Ok(None)
            }
            imap_proto::Response::Expunge(sequence) => {
                self.message_count.on_expunge(1);
                let Some(tracking) = &mut self.tracking else {
                    log::warn!("unexpected EXPUNGE {sequence} with QRESYNC enabled");
                    return Ok(None);
                };
                if let Some(uid) = tracking.uid_map.on_expunge(*sequence) {
                    log::trace!("EXPUNGE {sequence} resolved to uid {uid}");
                    self.send(Task::Delete(uid.into())).await?;
                } else {
                    log::debug!("cannot resolve uid of expunged mail {sequence}");
                    tracking.unknown_expunged = true;
                }
                Ok(None)
            }
            _ => Ok(Some(response)),
        }
    }
}

impl MailboxHandler {
    /// Records UIDs of FETCH responses collected during the last command. Skipped if mails got
    /// expunged meanwhile, as the collected sequence numbers may be stale then.
    fn apply_fetched_uids(&mut self, sequences: Vec<(u32, Uid)>) {
        let Some(SequenceTracking { uid_map, .. }) = &mut self.tracking else {
            return;
        };
        if uid_map.expunged_in_command() > 0 {
            log::debug!("not recording fetched uids due to concurrent expunge");
            return;
        }
        for (sequence, uid) in sequences {
            uid_map.on_fetch(sequence, Some(uid));
        }
    }

    /// Replaces the sequence to UID map with the UIDs of a `UID SEARCH ALL`. Skipped if mails got
    /// expunged meanwhile, as the result may predate them while new mails keep the count
    /// matching. The current map stays consistent then, as it processed the expunges itself.
    fn seed_uids(&mut self, uids: Vec<Uid>, exists: u32) {
        let Some(SequenceTracking { uid_map, .. }) = &mut self.tracking else {
            return;
        };
        if uid_map.expunged_in_command() > 0 {
            log::debug!("not seeding sequence to uid map due to concurrent expunge");
            return;
        }
        uid_map.seed(uids, exists);
    }

    /// Whether UIDs of fetched mails should be collected to resolve later responses
    fn needs_uids(&self) -> bool {
        self.tracking
            .as_ref()
            .is_some_and(|tracking| tracking.uid_map.needs_uids())
    }

    fn needs_remote_sync(&self) -> bool {
        self.message_count.has_unknown()
            || self
                .tracking
                .as_ref()
                .is_some_and(SequenceTracking::needs_remote_sync)
    }

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

    fn mailbox_handler(exists: u32) -> (MailboxHandler, crossfire::AsyncRx<mpsc::Array<Task>>) {
        let (task_tx, task_rx) = mpsc::bounded_async(16);
        let handler = MailboxHandler {
            task_tx,
            message_count: MessageCount::new(exists),
            tracking: Some(SequenceTracking::new(exists)),
        };

        (handler, task_rx)
    }

    fn uid(uid: u32) -> Uid {
        assertables::assert_ok!(Uid::try_from(uid))
    }

    async fn handle(handler: &mut MailboxHandler, raw: &'static [u8]) {
        assertables::assert_none!(assertables::assert_ok!(
            handler.handle(ResponseData::from_raw(raw)).await
        ));
    }

    #[test]
    fn test_search_collector_collects_uids() {
        let mut collector = SearchCollector::default();

        assertables::assert_none!(assertables::assert_ok!(
            collector.collect(ResponseData::from_raw(b"* SEARCH 1 4 7\r\n"))
        ));

        assert_eq!(collector.uids, vec![uid(1), uid(4), uid(7)]);
    }

    #[compio::test]
    async fn test_store_handler_uids_resolve_following_expunge() {
        let (mut handler, task_rx) = mailbox_handler(2);
        let mut store_handler = StoreHandler {
            sequences: Some(Vec::new()),
            ..StoreHandler::default()
        };
        assertables::assert_some!(handler.tracking.as_mut())
            .uid_map
            .begin_command();

        assertables::assert_none!(assertables::assert_ok!(
            store_handler.collect(ResponseData::from_raw(b"* 2 FETCH (UID 7 MODSEQ (9))\r\n"))
        ));
        handler.apply_fetched_uids(assertables::assert_some!(store_handler.sequences));
        handle(&mut handler, b"* 2 EXPUNGE\r\n").await;

        assert_eq!(store_handler.modseqs.len(), 1);
        let Ok(Task::Delete(deleted)) = task_rx.try_recv() else {
            panic!("expunge should be resolved to deletion");
        };
        assert_eq!(deleted.iter().collect::<Vec<_>>(), vec![uid(7)]);
        assert!(!handler.needs_remote_sync());
    }

    fn modseq(modseq: i64) -> ModSeq {
        assertables::assert_ok!(ModSeq::try_from(modseq))
    }

    #[test]
    fn test_own_modseqs_raise_unchanged_since_until_pruned() {
        let mut own = OwnModSeqs::default();
        own.record(&HashMap::from([(uid(1), modseq(7)), (uid(2), modseq(2))]));
        let set = assertables::assert_ok!(SequenceSet::try_from(&vec![uid(1), uid(2), uid(3)]));

        assert_eq!(
            own.unchanged_since(modseq(5), &set),
            HashMap::from([
                (uid(1), modseq(7)),
                (uid(2), modseq(5)),
                (uid(3), modseq(5))
            ])
        );

        own.prune(modseq(7));
        assert_eq!(own.unchanged_since(modseq(5), &set)[&uid(1)], modseq(5));
    }

    #[test]
    fn test_plausible_search_result_rejects_empty_result_for_nonempty_mailbox() {
        assert!(matches!(
            plausible_search_result(Vec::new(), 3),
            Err(Error::ImplausibleSearchResult(3))
        ));
        assert_eq!(
            assertables::assert_ok!(plausible_search_result(Vec::new(), 0)),
            Vec::new()
        );
        assert_eq!(
            assertables::assert_ok!(plausible_search_result(vec![uid(2)], 3)),
            vec![uid(2)]
        );
    }

    #[test]
    fn test_search_collector_sorts_and_dedups_uids() {
        let mut collector = SearchCollector::default();

        assertables::assert_none!(assertables::assert_ok!(
            collector.collect(ResponseData::from_raw(b"* SEARCH 9 4 7\r\n"))
        ));
        assertables::assert_none!(assertables::assert_ok!(
            collector.collect(ResponseData::from_raw(b"* SEARCH 4 1\r\n"))
        ));

        assert_eq!(
            collector.into_sorted_uids(),
            vec![uid(1), uid(4), uid(7), uid(9)]
        );
    }

    #[compio::test]
    async fn test_mailbox_handler_resolves_expunge_to_uid() {
        let (mut handler, task_rx) = mailbox_handler(2);
        handle(&mut handler, b"* 1 FETCH (UID 3 MODSEQ (1) FLAGS ())\r\n").await;
        handle(&mut handler, b"* 2 FETCH (UID 5 MODSEQ (1) FLAGS ())\r\n").await;
        while task_rx.try_recv().is_ok() {}

        handle(&mut handler, b"* 2 EXPUNGE\r\n").await;

        let Ok(Task::Delete(deleted)) = task_rx.try_recv() else {
            panic!("expunge should be resolved to deletion");
        };
        assert_eq!(deleted.iter().collect::<Vec<_>>(), vec![uid(5)]);
        assert!(!handler.needs_remote_sync());
    }

    #[compio::test]
    async fn test_mailbox_handler_skips_fetched_uids_after_concurrent_expunge() {
        let (mut handler, task_rx) = mailbox_handler(1);
        handle(&mut handler, b"* 1 FETCH (UID 1 MODSEQ (1) FLAGS ())\r\n").await;
        handle(&mut handler, b"* 3 EXISTS\r\n").await;
        while task_rx.try_recv().is_ok() {}
        let uid_map = &mut assertables::assert_some!(handler.tracking.as_mut()).uid_map;
        uid_map.begin_command();

        // UID 3 was fetched as sequence 2, then sequence 1 got expunged, shifting UID 3 to 1
        handle(&mut handler, b"* 1 EXPUNGE\r\n").await;
        handler.apply_fetched_uids(vec![(2, uid(3))]);

        let Ok(Task::Delete(deleted)) = task_rx.try_recv() else {
            panic!("expunge should be resolved to deletion");
        };
        assert_eq!(deleted.iter().collect::<Vec<_>>(), vec![uid(1)]);
        handle(&mut handler, b"* 2 EXPUNGE\r\n").await;
        assert!(task_rx.try_recv().is_err());
        assert!(assertables::assert_some!(&handler.tracking).unknown_expunged);
    }

    #[compio::test]
    async fn test_mailbox_handler_skips_seed_after_concurrent_expunge() {
        let (mut handler, task_rx) = mailbox_handler(3);
        handler.seed_uids(vec![uid(1), uid(2), uid(3)], 3);
        assertables::assert_some!(handler.tracking.as_mut())
            .uid_map
            .begin_command();

        // SEARCH returned UIDs 1 to 3, then UID 1 got expunged and UID 4 arrived
        handle(&mut handler, b"* 1 EXPUNGE\r\n").await;
        handle(&mut handler, b"* 3 EXISTS\r\n").await;
        handler.seed_uids(vec![uid(1), uid(2), uid(3)], 3);

        let Ok(Task::Delete(deleted)) = task_rx.try_recv() else {
            panic!("expunge should be resolved to deletion");
        };
        assert_eq!(deleted.iter().collect::<Vec<_>>(), vec![uid(1)]);
        handle(&mut handler, b"* 1 EXPUNGE\r\n").await;
        let Ok(Task::Delete(deleted)) = task_rx.try_recv() else {
            panic!("expunge should be resolved to deletion");
        };
        assert_eq!(deleted.iter().collect::<Vec<_>>(), vec![uid(2)]);
    }

    #[compio::test]
    async fn test_mailbox_handler_applies_fetched_uids() {
        let (mut handler, task_rx) = mailbox_handler(2);
        assertables::assert_some!(handler.tracking.as_mut())
            .uid_map
            .begin_command();

        handler.apply_fetched_uids(vec![(1, uid(3)), (2, uid(5))]);
        handle(&mut handler, b"* 2 EXPUNGE\r\n").await;

        let Ok(Task::Delete(deleted)) = task_rx.try_recv() else {
            panic!("expunge should be resolved to deletion");
        };
        assert_eq!(deleted.iter().collect::<Vec<_>>(), vec![uid(5)]);
    }

    #[compio::test]
    async fn test_mailbox_handler_flags_unresolved_expunge() {
        let (mut handler, task_rx) = mailbox_handler(2);

        handle(&mut handler, b"* 2 EXPUNGE\r\n").await;

        assert!(task_rx.try_recv().is_err());
        assert!(assertables::assert_some!(&handler.tracking).unknown_expunged);
        assert!(handler.needs_remote_sync());
    }

    #[compio::test]
    async fn test_mailbox_handler_resolves_uidless_flag_change() {
        let (mut handler, task_rx) = mailbox_handler(1);
        handle(&mut handler, b"* 1 FETCH (UID 3 MODSEQ (1) FLAGS ())\r\n").await;
        while task_rx.try_recv().is_ok() {}

        handle(&mut handler, b"* 1 FETCH (FLAGS (\\Seen) MODSEQ (5))\r\n").await;

        let Ok(Task::UpdateFlags(metadata)) = task_rx.try_recv() else {
            panic!("flag change should be resolved to update");
        };
        assert_eq!(metadata.uid(), uid(3));
        assert_eq!(metadata.flags(), Flag::Seen);
        assert!(!handler.needs_remote_sync());
    }

    #[compio::test]
    async fn test_mailbox_handler_flags_unresolved_flag_change() {
        let (mut handler, task_rx) = mailbox_handler(1);

        handle(&mut handler, b"* 1 FETCH (FLAGS (\\Seen) MODSEQ (5))\r\n").await;

        assert!(task_rx.try_recv().is_err());
        assert!(assertables::assert_some!(&handler.tracking).unsolicited_change);
        assert!(handler.needs_remote_sync());
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
