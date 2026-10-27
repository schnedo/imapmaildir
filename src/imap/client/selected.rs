use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::RangeInclusive;
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
        client::encoding::{self, CommandBuilder},
        client::fetch::FetchAttributes,
        client::message_count::MessageCount,
        client::uid_map::UidMap,
        transport::{self, Chain, Connection, ResponseData, ResponseHandler},
    },
    maildir::{FlagChange, LocalMail, MaildirFile, NewLocalMailMetadata},
    repository::{
        Flag, KeywordSource, Keywords, ModSeq, SequenceRange, SequenceSet, Uid, UidValidity,
    },
    sync::{ByteBudget, Task},
};

pub enum IdleStopReason {
    Timeout,
    Remote,
    Local {
        changes: LocalChanges,
    },
    Rescan,
    /// Tags changed in notmuch
    Tags,
    LocalError(MaildirError),
    /// Watching notmuch for tag changes stopped
    TagsError(crate::notmuch::Error),
    Shutdown,
}

/// Bounds the commands sent before receiving completions, so that neither side blocks on full
/// socket buffers while the other is still sending
const MAX_PIPELINED_COMMANDS: usize = 64;

/// Keywords the server stores permanently for the selected mailbox (RFC 3501 7.1)
#[derive(Debug, Clone)]
pub struct PermanentKeywords {
    /// Whether new keywords can be created (`\*`)
    pub any: bool,
    /// Existing keywords, that can be stored
    pub existing: Keywords,
}

impl Default for PermanentKeywords {
    /// Servers not reporting permanent flags let clients store all flags
    fn default() -> Self {
        Self {
            any: true,
            existing: Keywords::default(),
        }
    }
}

impl PermanentKeywords {
    /// Whether the server can store `keyword` of `source` when `added` to or removed from mails.
    /// Warns otherwise.
    fn can_store(&self, keyword: &str, source: KeywordSource, added: bool) -> bool {
        let storable = Keywords::is_storable(keyword, source)
            && (!added
                || source != KeywordSource::Flags
                || self.any
                || self.existing.contains(keyword));
        if !storable {
            log::warn!("server cannot store keyword {keyword}, not syncing it");
        }
        storable
    }
}

/// Maximum length of sequence sets in commands, as servers limit the length of command lines.
/// RFC 7162 4 recommends limiting them to 8192 octets.
const MAX_SEQUENCE_SET_LEN: usize = 8000;

#[derive(Debug)]
pub struct SelectedClient {
    connection: Connection,
    mailbox_handler: MailboxHandler,
    idle_stop_rx: AsyncRx<Array<IdleStopReason>>,
    idle_stop_tx: Option<AsyncTx<Array<IdleStopReason>>>,
    own_modseqs: OwnModSeqs,
    uid_validity: UidValidity,
    permanent_keywords: PermanentKeywords,
}
impl SelectedClient {
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        connection: Connection,
        capabilities: Capabilities,
        qresync: bool,
        task_tx: MAsyncTx<mpsc::Array<Task>>,
        budget: ByteBudget,
        exists: u32,
        uid_validity: UidValidity,
        permanent_keywords: PermanentKeywords,
    ) -> Result<Self, Error> {
        if !capabilities.contains(Capability::UidPlus) {
            return Err(Error::MissingCapability(Capability::UidPlus));
        }
        let (idle_stop_tx, idle_stop_rx) = spsc::bounded_async(1);

        Ok(Self {
            connection,
            mailbox_handler: MailboxHandler {
                task_tx,
                budget,
                message_count: MessageCount::new(exists),
                tracking: (!qresync).then(|| SequenceTracking::new(exists)),
                capabilities,
                unknown_keywords: false,
            },
            idle_stop_rx,
            idle_stop_tx: Some(idle_stop_tx),
            own_modseqs: OwnModSeqs::default(),
            uid_validity,
            permanent_keywords,
        })
    }

    /// Where the server keeps keywords of mails
    pub fn keyword_source(&self) -> KeywordSource {
        self.mailbox_handler.capabilities.keyword_source()
    }

    /// `change` without keywords, that the server cannot store, so that they do not get
    /// applied as if they were stored
    pub fn storable(&self, change: &FlagChange) -> FlagChange {
        let source = self.keyword_source();
        let mut storable = change.clone();
        storable
            .keywords
            .added
            .retain(|keyword| self.permanent_keywords.can_store(keyword, source, true));
        storable
            .keywords
            .removed
            .retain(|keyword| self.permanent_keywords.can_store(keyword, source, false));

        storable
    }

    /// Items of FETCH commands for metadata of mails
    fn metadata_items(&self) -> &'static str {
        match self.keyword_source() {
            KeywordSource::Flags => "UID MODSEQ FLAGS",
            KeywordSource::GmailLabels => "UID MODSEQ FLAGS X-GM-LABELS",
        }
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

    /// Fetches all UIDs of the selected mailbox in ascending order and replaces the sequence to
    /// UID map with them. Errors if the result is implausible, as it would delete all local mails.
    ///
    /// Uses a non-UID FETCH instead of `UID SEARCH`, as servers must not send EXPUNGE during it
    /// (RFC 3501 7.4.1) and its responses carry sequence numbers of the client's view. SEARCH
    /// results may reflect changes not announced yet, which would shift the map.
    pub async fn remote_uids(&mut self) -> Result<Vec<Uid>, Error> {
        if let Some(tracking) = &mut self.mailbox_handler.tracking {
            tracking.unknown_expunged = false;
            tracking.uid_map.begin_command();
        }
        let exists = self.mailbox_handler.message_count.exists();
        let fetched = if exists == 0 {
            // 1:* is invalid for an empty mailbox
            Vec::new()
        } else {
            let command = "FETCH 1:* (UID)";
            log::debug!("{command}");
            let mut handler = Chain(UidCollector::default(), &mut self.mailbox_handler);
            self.connection.send(command.into(), &mut handler).await?;
            handler.0.fetched
        };
        let mut uids: Vec<Uid> = fetched.iter().map(|(_, uid)| *uid).collect();
        // responses are usually in ascending sequence and therefore UID order
        if !uids.is_sorted() {
            uids.sort_unstable();
        }
        uids.dedup();
        let uids = plausible_uids(uids, self.mailbox_handler.message_count.exists())?;
        self.mailbox_handler
            .seed_uids(&fetched, self.mailbox_handler.message_count.exists());

        Ok(uids)
    }

    pub async fn fetch_mail(&mut self, sequence_set: &SequenceSet) -> Result<(), Error> {
        for sequence_set in sequence_set.chunks(MAX_SEQUENCE_SET_LEN) {
            let command = format!(
                "UID FETCH {sequence_set} ({} BODY.PEEK[])",
                self.metadata_items()
            );
            log::debug!("{command}");
            self.connection
                .send(command.into(), &mut self.mailbox_handler)
                .await?;
        }

        Ok(())
    }

    pub async fn fetch_all(&mut self) -> Result<(), Error> {
        let command = format!("UID FETCH 1:* ({} BODY.PEEK[])", self.metadata_items());
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
        let command = format!(
            "UID FETCH 1:* ({}) (CHANGEDSINCE {modseq}{vanished})",
            self.metadata_items()
        );
        if let Some(tracking) = &mut self.mailbox_handler.tracking {
            tracking.unsolicited_change = false;
        }
        self.mailbox_handler.unknown_keywords = false;
        self.collect_metadata(command, None).await
    }

    pub async fn fetch_metadata(
        &mut self,
        sequence_set: &SequenceSet,
    ) -> Result<Vec<RemoteMailMetadata>, Error> {
        let mut metadata = Vec::new();
        for sequence_set in sequence_set.chunks(MAX_SEQUENCE_SET_LEN) {
            metadata.extend(
                self.collect_metadata(
                    format!("UID FETCH {sequence_set} ({})", self.metadata_items()),
                    Some(sequence_set.iter().collect()),
                )
                .await?,
            );
        }

        Ok(metadata)
    }

    pub async fn fetch_all_metadata(&mut self) -> Result<Vec<RemoteMailMetadata>, Error> {
        self.collect_metadata(format!("UID FETCH 1:* ({})", self.metadata_items()), None)
            .await
    }

    /// Collects metadata of FETCH responses to `command`. Without `requested` UIDs, the command
    /// covers every message, e.g. `1:*`, which makes all messages existing at its start known.
    /// Otherwise unsolicited changes of other mails are handled like during IDLE.
    async fn collect_metadata(
        &mut self,
        command: String,
        requested: Option<HashSet<Uid>>,
    ) -> Result<Vec<RemoteMailMetadata>, Error> {
        log::debug!("{command}");
        let covers_all = requested.is_none();
        let collector = MetadataCollector {
            sequences: self.mailbox_handler.needs_uids().then(Vec::new),
            requested,
            keyword_source: self.keyword_source(),
            ..MetadataCollector::default()
        };
        let exists = self.mailbox_handler.message_count.begin_command();
        if let Some(tracking) = &mut self.mailbox_handler.tracking {
            tracking.uid_map.begin_command();
        }
        let mut handler = Chain(collector, &mut self.mailbox_handler);
        self.connection.send(command.into(), &mut handler).await?;
        let MetadataCollector {
            metadata,
            highest_sequence,
            sequences,
            ..
        } = handler.0;
        self.mailbox_handler
            .message_count
            .on_command_fetch(highest_sequence, covers_all.then_some(exists));
        if let Some(sequences) = sequences {
            self.mailbox_handler.apply_fetched_uids(sequences);
        }

        Ok(metadata)
    }

    pub async fn complete_initial_sync(&mut self, modseq: ModSeq) -> Result<(), Error> {
        let (done_tx, done_rx) = oneshot();
        let source = self.keyword_source();
        self.mailbox_handler
            .task_tx
            .send(Task::CompleteInitialSync(modseq, source, done_tx))
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
        mut mails: Vec<LocalMail>,
    ) -> Result<Vec<(Uid, NewLocalMailMetadata)>, Error> {
        let mailbox = encoding::mailbox(mailbox);
        if let Some(limit) = self.mailbox_handler.capabilities.append_limit() {
            mails.retain(|mail| {
                let fits = u64::try_from(mail.size()).is_ok_and(|size| size <= limit.get());
                if !fits {
                    log::warn!(
                        "mail {} with {} bytes exceeds the server's append limit of {limit} bytes, \
                        skipping it until next sync",
                        mail.metadata(),
                        mail.size()
                    );
                }
                fits
            });
        }
        if self
            .mailbox_handler
            .capabilities
            .contains(Capability::Multiappend)
            && mails.len() > 1
        {
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
                Ok(uids) => stored.extend(uids.into_iter().zip(Some(mail.into_metadata()))),
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
        mails.into_iter().map(LocalMail::into_metadata)
    }

    async fn append(&mut self, mailbox: &str, mails: &[LocalMail]) -> Result<Vec<Uid>, Error> {
        log::debug!("APPEND {mailbox}");
        let mut command =
            CommandBuilder::new(self.mailbox_handler.capabilities.literal_mode(), "APPEND ");
        command.push_str(mailbox);
        for mail in mails {
            if let Some(flags) = Flag::format(mail.metadata().flags()) {
                command.push_str(&format!(" ({flags})"));
            }
            command.push_str(" ");
            command.push_file_literal(mail.file(), mail.size());
        }

        let response = self
            .connection
            .send_streamed(&command.parts(), &mut self.mailbox_handler)
            .await?;

        let Some(imap_proto::ResponseCode::AppendUid(uid_validity, uid_set_members)) =
            response.unsafe_get_tagged_response_code()
        else {
            return Err(Error::MissingAppendUid);
        };
        // UIDs of a recreated mailbox do not refer to the synced mails
        if *uid_validity != u32::from(self.uid_validity) {
            return Err(Error::UidValidityMismatch {
                local: self.uid_validity,
                remote: UidValidity::try_from(uid_validity).map_err(Error::InvalidUidValidity)?,
            });
        }
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

    /// Stores each change on its mails unchanged since `highest_modseq`. Returns the UIDs of
    /// each change rejected due to remote changes and the stored change, which lacks keywords
    /// the server refused, e.g. as they are too long.
    pub async fn store_flag_changes(
        &mut self,
        highest_modseq: ModSeq,
        changes: Vec<(FlagChange, &SequenceSet)>,
    ) -> Result<Vec<(HashSet<Uid>, FlagChange)>, Error> {
        let changes = changes
            .into_iter()
            .map(|(change, sequence_set)| {
                let pending = self
                    .own_modseqs
                    .unchanged_since(highest_modseq, sequence_set);
                (pending, change)
            })
            .collect();
        self.store_flag_changes_since(changes).await
    }

    /// Stores each change on its mails unchanged since their modseq in its `pending`. These
    /// must be disjoint. Returns the UIDs of each change rejected due to remote changes and the
    /// stored change, which lacks keywords the server refused, e.g. as they are too long.
    pub async fn store_flag_changes_since(
        &mut self,
        changes: Vec<(HashMap<Uid, ModSeq>, FlagChange)>,
    ) -> Result<Vec<(HashSet<Uid>, FlagChange)>, Error> {
        let source = self.keyword_source();
        let mut stores: Vec<ChangeStore> = changes
            .into_iter()
            .map(|(pending, change)| ChangeStore {
                steps: store_items(&change, source, &self.permanent_keywords)
                    .into_iter()
                    .map(StoreStep::Items)
                    .collect(),
                pending,
                stored: change,
                rejected: HashSet::new(),
            })
            .collect();
        loop {
            // the next step of all changes at once, as their mails are disjoint. Steps of a
            // change follow each other, as each one needs the modseqs of the one before.
            let steps: Vec<(usize, StoreStep)> = stores
                .iter_mut()
                .enumerate()
                .filter(|(_, store)| !store.pending.is_empty())
                .filter_map(|(index, store)| store.steps.pop_front().map(|step| (index, step)))
                .collect();
            if steps.is_empty() {
                break;
            }
            let commands: Vec<StoreCommand> = steps
                .iter()
                .enumerate()
                .flat_map(|(step, (store, items))| {
                    let (operation, items) = items.command();
                    grouped(&stores[*store].pending).into_iter().map(
                        move |(unchanged_since, sequence_set)| StoreCommand {
                            step,
                            unchanged_since,
                            sequence_set,
                            operation,
                            items: items.clone(),
                        },
                    )
                })
                .collect();
            let mut refused: Vec<Option<Option<String>>> = vec![None; steps.len()];
            for commands in commands.chunks(MAX_PIPELINED_COMMANDS) {
                let (results, modseqs) = self.store_flags(commands).await?;
                // also of commands pipelined with failed ones, which got stored nevertheless
                for (uid, modseq) in modseqs {
                    for store in &mut stores {
                        if let Some(pending_modseq) = store.pending.get_mut(&uid) {
                            *pending_modseq = modseq;
                            break;
                        }
                    }
                }
                let mut failed = None;
                for (command, result) in commands.iter().zip(results) {
                    let (store, step) = &steps[command.step];
                    let store = &mut stores[*store];
                    match result {
                        Ok(modified) => {
                            for uid in &modified {
                                store.pending.remove(uid);
                            }
                            store.rejected.extend(modified);
                        }
                        Err(Error::Transport(transport::Error::No(reason)))
                            if step.is_refusable() && !is_transient_no(reason.as_ref()) =>
                        {
                            refused[command.step] = Some(reason);
                        }
                        Err(e) => {
                            failed.get_or_insert(e);
                        }
                    }
                }
                if let Some(e) = failed {
                    return Err(e);
                }
            }
            for ((store, step), reason) in steps.into_iter().zip(refused) {
                if let Some(reason) = reason {
                    stores[store].refuse(step, reason.as_deref().unwrap_or("NO"));
                }
            }
        }

        Ok(stores
            .into_iter()
            .map(|store| (store.rejected, store.stored))
            .collect())
    }

    /// Stores flags of `pending` mails grouped by their UNCHANGEDSINCE modseq, pipelining the
    /// commands of all groups. Updates the modseqs of stored mails and removes rejected ones,
    /// returning them.
    async fn store_flags_grouped(
        &mut self,
        pending: &mut HashMap<Uid, ModSeq>,
        operation: &'static str,
        items: &str,
    ) -> Result<HashSet<Uid>, Error> {
        let commands: Vec<StoreCommand> = grouped(pending)
            .into_iter()
            .map(|(unchanged_since, sequence_set)| StoreCommand {
                step: 0,
                unchanged_since,
                sequence_set,
                operation,
                items: items.to_string(),
            })
            .collect();
        let mut rejected = HashSet::new();
        for commands in commands.chunks(MAX_PIPELINED_COMMANDS) {
            let (results, modseqs) = self.store_flags(commands).await?;
            for (uid, modseq) in modseqs {
                if let Some(pending_modseq) = pending.get_mut(&uid) {
                    *pending_modseq = modseq;
                }
            }
            for modified in results {
                for uid in modified? {
                    pending.remove(&uid);
                    rejected.insert(uid);
                }
            }
        }

        Ok(rejected)
    }

    /// Pipelines `commands`, which must store disjoint mails. Returns the UIDs each one
    /// rejected due to remote changes or its error, and the new modseqs of stored mails.
    async fn store_flags(
        &mut self,
        commands: &[StoreCommand],
    ) -> Result<(Vec<Result<HashSet<Uid>, Error>>, HashMap<Uid, ModSeq>), Error> {
        let raw = commands
            .iter()
            .map(|command| {
                let StoreCommand {
                    unchanged_since,
                    sequence_set,
                    operation,
                    items,
                    ..
                } = command;
                let command = format!(
                    "UID STORE {sequence_set} (UNCHANGEDSINCE {unchanged_since}) {operation}.SILENT ({items})"
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
            .send_pipelined_each(raw, &mut handler)
            .await?;
        let StoreHandler { modseqs, sequences } = handler.0;
        self.own_modseqs.record(&modseqs);
        // learning UIDs of e.g. own uploads lets a following UID EXPUNGE resolve without rescan
        if let Some(sequences) = sequences {
            self.mailbox_handler.apply_fetched_uids(sequences);
        }
        let results = responses
            .into_iter()
            .zip(commands)
            .map(|(response, command)| {
                let modified = modified_uids(&response?, &command.sequence_set)?;
                if !modified.is_empty() {
                    log::warn!("mails {modified:?} changed on server. Not updating their flags");
                }
                Ok(modified)
            })
            .collect();

        Ok((results, modseqs))
    }

    pub async fn delete(
        &mut self,
        highest_modseq: ModSeq,
        sequence_set: &SequenceSet,
    ) -> Result<HashSet<Uid>, Error> {
        let pending = self
            .own_modseqs
            .unchanged_since(highest_modseq, sequence_set);
        self.delete_since(pending).await
    }

    /// Deletes mails unchanged since their modseq in `pending` and returns the UIDs rejected due
    /// to remote changes
    pub async fn delete_since(
        &mut self,
        mut pending: HashMap<Uid, ModSeq>,
    ) -> Result<HashSet<Uid>, Error> {
        let modified = self
            .store_flags_grouped(&mut pending, "+FLAGS", &Flag::Deleted.to_string())
            .await?;
        if !modified.is_empty() {
            log::warn!("mails {modified:?} changed on server. Not deleting them");
        }
        let mut accepted: Vec<Uid> = pending.into_keys().collect();
        accepted.sort_unstable();
        let Ok(accepted) = SequenceSet::from_sorted(accepted) else {
            return Ok(modified);
        };
        for accepted in accepted.chunks(MAX_SEQUENCE_SET_LEN) {
            let command = format!("UID EXPUNGE {accepted}");
            log::debug!("{command}");
            self.connection
                .send(command.into_bytes(), &mut self.mailbox_handler)
                .await?;
        }

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
        let started = loop {
            let response = connection.next_response().await?;
            match response.parsed() {
                imap_proto::Response::Continue(_) => {
                    log::trace!("idling for up to {} seconds", timeout.as_secs());
                    break Instant::now();
                }
                imap_proto::Response::Done { .. } => return Err(Error::IdleRejected),
                _ => handler.handle_or_log(response).await?,
            }
        };

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
                    let response = response?;
                    if let imap_proto::Response::Data {
                        status: imap_proto::Status::Bye,
                        outcome,
                        ..
                    } = response.parsed()
                    {
                        log::debug!("BYE during IDLE: {outcome:?}");
                        return Err(Error::IdleTerminated(started.elapsed()));
                    }
                    handler.handle_or_log(response).await?;
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
    keyword_source: KeywordSource,
    metadata: Vec<RemoteMailMetadata>,
    highest_sequence: u32,
    /// Only collected while tracking the sequence to UID map
    sequences: Option<Vec<(u32, Uid)>>,
    /// Collects all mails if unset
    requested: Option<HashSet<Uid>>,
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
            && let Some(metadata) = parsed.metadata(self.keyword_source)?
            && self
                .requested
                .as_ref()
                .is_none_or(|requested| requested.contains(&metadata.uid()))
        {
            if metadata.keywords().is_none() {
                // e.g. labels sent as literals, which imap-proto cannot parse
                log::warn!(
                    "server reported mail {} without readable labels, which may stay unsynced",
                    metadata.uid()
                );
            }
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

/// `UID STORE` of a step storing a change
struct StoreCommand {
    /// Index of the step among the ones stored at once
    step: usize,
    unchanged_since: ModSeq,
    sequence_set: SequenceSet,
    operation: &'static str,
    items: String,
}

/// UIDs of `pending` grouped by their modseq, in sequence sets not exceeding
/// [`MAX_SEQUENCE_SET_LEN`]
fn grouped(pending: &HashMap<Uid, ModSeq>) -> Vec<(ModSeq, SequenceSet)> {
    let mut groups: HashMap<ModSeq, Vec<Uid>> = HashMap::new();
    for (uid, modseq) in pending {
        groups.entry(*modseq).or_default().push(*uid);
    }
    groups
        .into_iter()
        .flat_map(|(unchanged_since, uids)| {
            let set = SequenceSet::try_from(&uids).expect("grouped uids should not be empty");
            set.chunks(MAX_SEQUENCE_SET_LEN)
                .into_iter()
                .map(move |set| (unchanged_since, set))
        })
        .collect()
}

/// Storing a change on its mails step by step
struct ChangeStore {
    /// Mails not rejected yet with the modseq they must be unchanged since
    pending: HashMap<Uid, ModSeq>,
    /// The change without keywords the server refused
    stored: FlagChange,
    rejected: HashSet<Uid>,
    steps: VecDeque<StoreStep>,
}

impl ChangeStore {
    /// Continues after the server refused `step` for a not transient `reason`
    fn refuse(&mut self, step: StoreStep, reason: &str) {
        match step {
            StoreStep::Items(items) => {
                log::debug!(
                    "server refused {} ({reason}), storing keywords one by one",
                    items.operation
                );
                let operation = items.operation;
                let keywords =
                    items
                        .keywords
                        .into_iter()
                        .zip(items.originals)
                        .map(|(keyword, original)| StoreStep::Keyword {
                            operation,
                            keyword,
                            original,
                        });
                let flags = (!items.flags.is_empty()).then(|| StoreStep::Flags {
                    operation,
                    flags: items.flags.join(" "),
                });
                for step in flags.into_iter().chain(keywords).rev() {
                    self.steps.push_front(step);
                }
            }
            StoreStep::Keyword {
                operation,
                original,
                ..
            } => {
                log::warn!("server refused {operation} {original} ({reason}), not syncing it");
                let refused = if operation.starts_with('+') {
                    &mut self.stored.keywords.added
                } else {
                    &mut self.stored.keywords.removed
                };
                refused.retain(|kept| *kept != original);
            }
            StoreStep::Flags { .. } => unreachable!("flags are not refusable"),
        }
    }
}

/// Operation of storing a change
enum StoreStep {
    /// All items of an operation. If the server refuses them, its flags and each of its
    /// keywords get stored separately.
    Items(StoreItems),
    Flags {
        operation: &'static str,
        flags: String,
    },
    /// A single keyword, which the change loses if the server refuses it
    Keyword {
        operation: &'static str,
        /// Formatted keyword
        keyword: String,
        /// Keyword as in the change
        original: String,
    },
}

impl StoreStep {
    fn command(&self) -> (&'static str, String) {
        match self {
            Self::Items(items) => (items.operation, items.joined()),
            Self::Flags { operation, flags } => (operation, flags.clone()),
            Self::Keyword {
                operation, keyword, ..
            } => (operation, keyword.clone()),
        }
    }

    /// Whether the server may refuse it, e.g. as keywords are too long
    fn is_refusable(&self) -> bool {
        match self {
            Self::Items(items) => !items.keywords.is_empty(),
            Self::Flags { .. } => false,
            Self::Keyword { .. } => true,
        }
    }
}

/// Items of a `STORE` operation, e.g. `+FLAGS`
#[derive(Debug, PartialEq, Eq)]
struct StoreItems {
    operation: &'static str,
    flags: Vec<String>,
    /// Formatted keywords
    keywords: Vec<String>,
    /// Keywords as in the change
    originals: Vec<String>,
}

impl StoreItems {
    fn joined(&self) -> String {
        let mut items = self.flags.clone();
        items.extend(self.keywords.iter().cloned());
        items.join(" ")
    }
}

/// The `+`/`-` operations and their `FLAGS` or `X-GM-LABELS` items storing `change`.
/// Keywords the server cannot store get dropped.
fn store_items(
    change: &FlagChange,
    source: KeywordSource,
    permanent_keywords: &PermanentKeywords,
) -> Vec<StoreItems> {
    let keywords = |keywords: &Keywords, added: bool| -> (Vec<String>, Vec<String>) {
        keywords
            .iter()
            .filter(|keyword| permanent_keywords.can_store(keyword, source, added))
            .map(|keyword| {
                let formatted = match source {
                    KeywordSource::GmailLabels if !keyword.starts_with('\\') => {
                        encoding::quote(keyword).expect("storable labels should be quotable")
                    }
                    _ => keyword.to_string(),
                };
                (formatted, keyword.to_string())
            })
            .unzip()
    };
    let (added, added_originals) = keywords(&change.keywords.added, true);
    let (removed, removed_originals) = keywords(&change.keywords.removed, false);
    let flags = |flags: BitFlags<Flag>| flags.iter().map(|flag| flag.to_string()).collect();
    let item = |operation, flags, keywords, originals| StoreItems {
        operation,
        flags,
        keywords,
        originals,
    };
    let items = match source {
        KeywordSource::Flags => vec![
            item("+FLAGS", flags(change.added), added, added_originals),
            item("-FLAGS", flags(change.removed), removed, removed_originals),
        ],
        KeywordSource::GmailLabels => vec![
            item("+FLAGS", flags(change.added), Vec::new(), Vec::new()),
            item("-FLAGS", flags(change.removed), Vec::new(), Vec::new()),
            item("+X-GM-LABELS", Vec::new(), added, added_originals),
            item("-X-GM-LABELS", Vec::new(), removed, removed_originals),
        ],
    };

    items
        .into_iter()
        .filter(|items| !items.flags.is_empty() || !items.keywords.is_empty())
        .collect()
}

/// Whether a NO response with `reason` may succeed later
fn is_transient_no(reason: Option<&String>) -> bool {
    reason.is_some_and(|reason| transport::is_transient_no(reason))
}

fn plausible_uids(uids: Vec<Uid>, exists: u32) -> Result<Vec<Uid>, Error> {
    if uids.is_empty() && exists > 0 {
        Err(Error::ImplausibleUidList(exists))
    } else {
        Ok(uids)
    }
}

/// Collects sequence numbers and UIDs of `FETCH (UID)` responses
#[derive(Default)]
struct UidCollector {
    fetched: Vec<(u32, Uid)>,
}

impl ResponseHandler for UidCollector {
    type Error = Error;

    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>> {
        std::future::ready(self.collect(response))
    }
}

impl UidCollector {
    /// Passes on responses carrying more than the UID, e.g. concurrent flag changes, after
    /// recording their UID
    fn collect(&mut self, response: ResponseData) -> Result<Option<ResponseData>, Error> {
        let imap_proto::Response::Fetch(sequence, attributes) = response.parsed() else {
            return Ok(Some(response));
        };
        let attributes = FetchAttributes::parse(attributes);
        let Some(uid) = attributes.uid else {
            return Ok(Some(response));
        };
        self.fetched.push((*sequence, uid.try_into()?));

        if attributes.flags.is_some() || attributes.body.is_some() {
            Ok(Some(response))
        } else {
            Ok(None)
        }
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
                labels: None,
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

/// UIDs of `stored` reported as modified by the server
fn modified_uids(response: &ResponseData, stored: &SequenceSet) -> Result<HashSet<Uid>, Error> {
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
        let ranges = parse_uid_set(uids)?;
        // only checking stored UIDs bounds the work, as servers may send huge ranges
        Ok(stored
            .iter()
            .filter(|uid| ranges.iter().any(|range| range.contains(&u32::from(uid))))
            .collect())
    } else {
        Ok(HashSet::new())
    }
}

fn parse_uid_set(uids: &str) -> Result<Vec<RangeInclusive<u32>>, Error> {
    let parse = |uid: &str| -> Result<u32, Error> {
        uid.parse()
            .map_err(|_| Error::InvalidUidSet(uids.to_string()))
    };
    let mut parsed = Vec::new();
    for range in uids.split(',') {
        let (start, end) = range.split_once(':').unwrap_or((range, range));
        let (start, end) = (parse(start)?, parse(end)?);
        parsed.push(start.min(end)..=start.max(end));
    }

    Ok(parsed)
}

#[derive(Debug)]
struct MailboxHandler {
    task_tx: MAsyncTx<mpsc::Array<Task>>,
    /// Bounds the bytes of downloaded mails queued for storing
    budget: ByteBudget,
    message_count: MessageCount,
    /// Only tracked without QRESYNC, as VANISHED and FETCH responses carry UIDs otherwise
    tracking: Option<SequenceTracking>,
    /// Updated by unsolicited CAPABILITY responses
    capabilities: Capabilities,
    /// Flags of mails changed without reporting their keywords, e.g. Gmail labels
    unknown_keywords: bool,
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
                let source = self.capabilities.keyword_source();
                let metadata = if let Some(tracking) = &mut self.tracking {
                    let uid = attributes.uid.map(Uid::try_from).transpose()?;
                    let resolved = tracking.uid_map.on_fetch(*sequence, uid);
                    let metadata = attributes.metadata_with_uid(resolved.or(uid), source)?;
                    if metadata.is_none() && attributes.uid.is_none() && attributes.flags.is_some()
                    {
                        log::debug!("cannot resolve uid of mail {sequence} with changed flags");
                        tracking.unsolicited_change = true;
                        return Ok(None);
                    }
                    metadata
                } else {
                    attributes.metadata(source)?
                };
                if let Some(metadata) = &metadata
                    && metadata.keywords().is_none()
                {
                    log::debug!("keywords of mail {} changed unreported", metadata.uid());
                    self.unknown_keywords = true;
                }
                let task = match (metadata, attributes.body) {
                    (Some(metadata), Some(Some(content))) => {
                        self.budget
                            .acquire(content.len())
                            .await
                            .map_err(|_| Error::TaskProcessingStopped)?;
                        let content = RemoteContent::new(response.bytes_of(content));
                        Task::NewMail(RemoteMail::new(metadata, content))
                    }
                    (Some(metadata), Some(None)) => {
                        // servers may send NIL for mails expunged by other sessions (RFC 2180 4.1.3)
                        log::warn!("skipping mail {} without content", metadata.uid());
                        return Ok(None);
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
                    // ranges may be reversed (RFC 3501 9)
                    let count = uids
                        .iter()
                        .map(|range| range.end().abs_diff(*range.start()).saturating_add(1))
                        .fold(0u32, u32::saturating_add);
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
            imap_proto::Response::Capabilities(caps) => {
                self.capabilities.replace(caps);
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

    /// Replaces the sequence to UID map with the sequence numbers and UIDs of a `FETCH 1:* (UID)`.
    /// Skipped if mails got expunged meanwhile, which servers must not do during FETCH, as the
    /// sequence numbers may be stale then. The current map stays consistent then, as it processed
    /// the expunges itself.
    fn seed_uids(&mut self, fetched: &[(u32, Uid)], exists: u32) {
        let Some(SequenceTracking { uid_map, .. }) = &mut self.tracking else {
            return;
        };
        if uid_map.expunged_in_command() > 0 {
            log::debug!("not seeding sequence to uid map due to concurrent expunge");
            return;
        }
        uid_map.seed(fetched, exists);
    }

    /// Whether UIDs of fetched mails should be collected to resolve later responses
    fn needs_uids(&self) -> bool {
        self.tracking
            .as_ref()
            .is_some_and(|tracking| tracking.uid_map.needs_uids())
    }

    fn needs_remote_sync(&self) -> bool {
        self.message_count.has_unknown()
            || self.unknown_keywords
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rstest::*;

    use super::*;
    use crate::imap::client::encoding::LiteralMode;

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
            budget: ByteBudget::new(usize::MAX),
            message_count: MessageCount::new(exists),
            tracking: Some(SequenceTracking::new(exists)),
            capabilities: Capabilities::default(),
            unknown_keywords: false,
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
    fn test_uid_collector_collects_sequences_and_uids() {
        let mut collector = UidCollector::default();

        assertables::assert_none!(assertables::assert_ok!(
            collector.collect(ResponseData::from_raw(b"* 1 FETCH (UID 4)\r\n"))
        ));
        assertables::assert_none!(assertables::assert_ok!(
            collector.collect(ResponseData::from_raw(b"* 2 FETCH (UID 7 MODSEQ (3))\r\n"))
        ));

        assert_eq!(collector.fetched, vec![(1, uid(4)), (2, uid(7))]);
    }

    #[test]
    fn test_uid_collector_passes_on_flag_changes() {
        let mut collector = UidCollector::default();

        assertables::assert_some!(assertables::assert_ok!(collector.collect(
            ResponseData::from_raw(b"* 1 FETCH (UID 4 FLAGS (\\Seen) MODSEQ (3))\r\n")
        )));
        assertables::assert_some!(assertables::assert_ok!(collector.collect(
            ResponseData::from_raw(b"* 2 FETCH (FLAGS (\\Seen) MODSEQ (3))\r\n")
        )));
        assertables::assert_some!(assertables::assert_ok!(
            collector.collect(ResponseData::from_raw(b"* 3 EXISTS\r\n"))
        ));

        assert_eq!(collector.fetched, vec![(1, uid(4))]);
    }

    #[test]
    fn test_metadata_collector_passes_on_changes_of_unrequested_mails() {
        let mut collector = MetadataCollector {
            requested: Some(HashSet::from([uid(4)])),
            ..MetadataCollector::default()
        };

        assertables::assert_none!(assertables::assert_ok!(collector.collect(
            ResponseData::from_raw(b"* 1 FETCH (UID 4 FLAGS (\\Seen) MODSEQ (3))\r\n")
        )));
        assertables::assert_some!(assertables::assert_ok!(collector.collect(
            ResponseData::from_raw(b"* 2 FETCH (UID 7 FLAGS (\\Seen) MODSEQ (5))\r\n")
        )));

        assert_eq!(
            collector
                .metadata
                .iter()
                .map(RemoteMailMetadata::uid)
                .collect::<Vec<_>>(),
            vec![uid(4)]
        );
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
    fn test_plausible_uids_rejects_empty_result_for_nonempty_mailbox() {
        assert!(matches!(
            plausible_uids(Vec::new(), 3),
            Err(Error::ImplausibleUidList(3))
        ));
        assert_eq!(
            assertables::assert_ok!(plausible_uids(Vec::new(), 0)),
            Vec::new()
        );
        assert_eq!(
            assertables::assert_ok!(plausible_uids(vec![uid(2)], 3)),
            vec![uid(2)]
        );
    }

    #[compio::test]
    async fn test_mailbox_handler_replaces_capabilities() {
        let (mut handler, _task_rx) = mailbox_handler(0);

        handle(
            &mut handler,
            b"* CAPABILITY IMAP4rev1 LITERAL+ APPENDLIMIT=100\r\n",
        )
        .await;
        handle(
            &mut handler,
            b"* CAPABILITY IMAP4rev1 LITERAL- MULTIAPPEND\r\n",
        )
        .await;

        assert_eq!(handler.capabilities.literal_mode(), LiteralMode::Minus);
        assert_eq!(handler.capabilities.append_limit(), None);
        assert!(handler.capabilities.contains(Capability::Multiappend));
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
        let fetched = [(1, uid(1)), (2, uid(2)), (3, uid(3))];
        handler.seed_uids(&fetched, 3);
        assertables::assert_some!(handler.tracking.as_mut())
            .uid_map
            .begin_command();

        // FETCH returned UIDs 1 to 3, then the server violated RFC 3501 by expunging UID 1
        // during it, and UID 4 arrived
        handle(&mut handler, b"* 1 EXPUNGE\r\n").await;
        handle(&mut handler, b"* 3 EXISTS\r\n").await;
        handler.seed_uids(&fetched, 3);

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

    #[rstest]
    #[case(b"* 1 FETCH (UID 3 MODSEQ (5) FLAGS (\\Seen))\r\n", None, true)]
    #[case(
        b"* 1 FETCH (UID 3 MODSEQ (5) FLAGS (\\Seen) X-GM-LABELS (\\Inbox))\r\n",
        Some(&["\\Inbox"][..]),
        false
    )]
    #[compio::test]
    async fn test_mailbox_handler_takes_gmail_labels_or_requests_sync(
        #[case] response: &'static [u8],
        #[case] labels: Option<&[&str]>,
        #[case] needs_sync: bool,
    ) {
        let (mut handler, task_rx) = mailbox_handler(1);
        handler.capabilities.use_gmail_labels(true);
        handle(&mut handler, b"* CAPABILITY IMAP4rev1 X-GM-EXT-1\r\n").await;

        handle(&mut handler, response).await;

        let Ok(Task::UpdateFlags(metadata)) = task_rx.try_recv() else {
            panic!("flag change should be applied");
        };
        assert_eq!(metadata.keywords().cloned(), labels.map(Keywords::of));
        assert_eq!(handler.needs_remote_sync(), needs_sync);
    }

    fn joined(items: Vec<StoreItems>) -> Vec<(&'static str, String)> {
        items
            .into_iter()
            .map(|items| (items.operation, items.joined()))
            .collect()
    }

    fn keyword_change() -> FlagChange {
        FlagChange {
            added: Flag::Seen.into(),
            removed: Flag::Flagged.into(),
            keywords: crate::repository::KeywordChange {
                added: Keywords::of(&["$Junk", "my label", "\\Inbox"]),
                removed: Keywords::of(&["work"]),
            },
        }
    }

    #[test]
    fn test_store_items_combine_flags_and_keywords() {
        let items = joined(store_items(
            &keyword_change(),
            KeywordSource::Flags,
            &PermanentKeywords::default(),
        ));

        assert_eq!(
            items,
            vec![
                ("+FLAGS", "\\Seen $Junk".to_string()),
                ("-FLAGS", "\\Flagged work".to_string())
            ]
        );
    }

    #[test]
    fn test_store_items_skip_keywords_server_cannot_create() {
        let permanent = PermanentKeywords {
            any: false,
            existing: Keywords::of(&["work"]),
        };
        let mut change = keyword_change();
        change.keywords.added = Keywords::of(&["$Junk", "work"]);

        let items = joined(store_items(&change, KeywordSource::Flags, &permanent));

        assert_eq!(
            items,
            vec![
                ("+FLAGS", "\\Seen work".to_string()),
                ("-FLAGS", "\\Flagged work".to_string())
            ]
        );
    }

    #[test]
    fn test_store_items_store_gmail_labels_separately() {
        let items = joined(store_items(
            &keyword_change(),
            KeywordSource::GmailLabels,
            &PermanentKeywords::default(),
        ));

        assert_eq!(
            items,
            vec![
                ("+FLAGS", "\\Seen".to_string()),
                ("-FLAGS", "\\Flagged".to_string()),
                ("+X-GM-LABELS", "\"$Junk\" \\Inbox \"my label\"".to_string()),
                ("-X-GM-LABELS", "\"work\"".to_string()),
            ]
        );
    }

    #[test]
    fn test_parse_uid_set_parses_ranges() {
        assert_eq!(
            assertables::assert_ok!(parse_uid_set("1,5:3,9")),
            vec![1..=1, 3..=5, 9..=9]
        );
    }

    #[test]
    fn test_modified_uids_only_contains_stored_uids() {
        let response =
            ResponseData::from_raw(b"0001 OK [MODIFIED 1:4294967295] Conditional STORE failed\r\n");
        let stored = assertables::assert_ok!(SequenceSet::try_from(&vec![uid(3), uid(7)]));

        assert_eq!(
            assertables::assert_ok!(modified_uids(&response, &stored)),
            HashSet::from([uid(3), uid(7)])
        );
    }
}
