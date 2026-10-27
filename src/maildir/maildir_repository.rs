use crossfire::{
    AsyncRx,
    spsc::{self, Array},
};
use enumflags2::BitFlags;
use futures::{StreamExt as _, TryStreamExt as _, stream};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    str::FromStr as _,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;

use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    maildir::{
        ChangeRecorder, FlagChange, LocalChanges, LocalFlagChangesBuilder, LocalMail,
        LocalMailMetadata, MailChange, NewLocalMailMetadata, OtherFlags,
        maildir::{self, Change, MaildirFile},
        state::{self, State},
    },
    notmuch::{self, IndexOptions, Revision, SyncedTags, TagMapping, TagUpdate},
    repository::{Flag, KeywordChange, KeywordSource, Keywords, ModSeq, Uid, UidValidity},
};

use super::Maildir;

const MAX_CONCURRENT_FILE_OPS: usize = 64;
/// Mails synced to notmuch per change of its database, which blocks other writers meanwhile
const TAG_SYNC_BATCH_SIZE: usize = 512;
/// Time after which a change of notmuch ends, as it blocks other writers meanwhile, e.g. while
/// indexing each renamed or new file
#[cfg(not(test))]
const NOTMUCH_WRITE_DURATION: Duration = Duration::from_millis(500);
#[cfg(test)]
const NOTMUCH_WRITE_DURATION: Duration = Duration::ZERO;
/// Time the last file of a message stays in notmuch after getting deleted
#[cfg(not(test))]
const NOTMUCH_REMOVAL_DELAY: Duration = Duration::from_mins(5);
#[cfg(test)]
const NOTMUCH_REMOVAL_DELAY: Duration = Duration::ZERO;

/// Applies `f` to each of `items` in as many changes of notmuch as needed to end each of them
/// after [`NOTMUCH_WRITE_DURATION`]. Returns the results of the changes before the first failing
/// one, which got discarded, with its error.
async fn write_briefly<I, T>(
    tags: &SyncedTags,
    items: Vec<I>,
    own: bool,
    f: impl Fn(&notmuch::Database, &IndexOptions, &TagMapping, &I) -> Result<T, notmuch::DatabaseError>
    + Clone
    + Send
    + 'static,
) -> (Vec<T>, Option<notmuch::Error>)
where
    I: Send + Sync + 'static,
    T: Send + 'static,
{
    let items = Arc::new(items);
    let mut results = Vec::with_capacity(items.len());
    while results.len() < items.len() {
        let (items, f, mapping, start) = (
            items.clone(),
            f.clone(),
            tags.mapping.clone(),
            results.len(),
        );
        let written = tags
            .notmuch
            .write(
                move |database| {
                    let options = IndexOptions::of(database, &mapping)?;
                    let started = Instant::now();
                    let mut written = Vec::new();
                    for item in &items[start..] {
                        written.push(f(database, &options, &mapping, item)?);
                        if started.elapsed() >= NOTMUCH_WRITE_DURATION {
                            break;
                        }
                    }

                    Ok(written)
                },
                own,
            )
            .await;
        match written {
            Ok(written) => results.extend(written),
            Err(e) => return (results, Some(e)),
        }
    }

    (results, None)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed())
}

/// Mail with updated flags, the path it got renamed from and its former flags, if recorded
type FlagUpdate = (LocalMailMetadata, Option<PathBuf>, Option<String>);

/// Renamed mail files, that notmuch needs to know about, in the order of renaming them
#[derive(Debug, Default)]
struct PendingRenames {
    renames: Vec<(PathBuf, PathBuf)>,
    /// Indices of `renames` by their current names, as e.g. marking all mails as read renames
    /// each of them
    by_target: HashMap<PathBuf, usize>,
}

impl PendingRenames {
    fn len(&self) -> usize {
        self.by_target.len()
    }

    fn add(&mut self, from: PathBuf, to: PathBuf) {
        // a file renamed again keeps its first name, that notmuch knows
        if let Some(index) = self.by_target.remove(&from) {
            let rename = &mut self.renames[index];
            // renamed back, which notmuch must not see as removal of its only file, gets
            // dropped when taking them
            if rename.0 != to {
                self.by_target.insert(to.clone(), index);
            }
            rename.1 = to;
        } else if from != to {
            self.by_target.insert(to.clone(), self.renames.len());
            self.renames.push((from, to));
        }
    }

    fn take(&mut self) -> Vec<(PathBuf, PathBuf)> {
        self.by_target.clear();
        let mut renames = std::mem::take(&mut self.renames);
        renames.retain(|(from, to)| from != to);
        renames
    }
}

#[derive(Debug)]
pub struct MaildirRepository {
    maildir: Maildir,
    state: State,
    changes: RefCell<Option<ChangeRecorder>>,
    /// Lets writes to notmuch wait for ones in progress, e.g. before deleting renamed files
    flushing: futures::lock::Mutex<()>,
    tags: Option<SyncedTags>,
    /// Whether tags of downloaded mails get synced after completing the initial sync, which
    /// writes them in larger batches
    defer_tags: Cell<bool>,
    /// Mails, whose keywords need writing to notmuch, which happens in batches
    tags_pending: RefCell<HashSet<Uid>>,
    renames_pending: RefCell<PendingRenames>,
}

impl MaildirRepository {
    fn new(maildir: Maildir, state: State) -> Self {
        Self {
            maildir,
            state,
            changes: RefCell::new(None),
            flushing: futures::lock::Mutex::new(()),
            tags: None,
            defer_tags: Cell::new(false),
            tags_pending: RefCell::new(HashSet::new()),
            renames_pending: RefCell::new(PendingRenames::default()),
        }
    }

    /// Reports changes of the remote state, that got applied locally, from now on
    fn records_changes(&self) -> bool {
        self.changes.borrow().is_some()
    }

    pub fn record_changes(&self, changes: Option<ChangeRecorder>) {
        *self.changes.borrow_mut() = changes;
    }

    /// Syncs keywords with notmuch tags and keeps notmuch informed about mail files from now on
    pub fn sync_tags(&mut self, tags: Option<SyncedTags>) {
        self.tags = tags;
    }

    /// Remembers mails, whose keywords need writing to notmuch with [`Self::flush_tags`]
    fn mark_tags_pending(&self, uids: impl IntoIterator<Item = Uid>) {
        // the initial sync writes all of them at its end
        if self.tags.is_some() && !self.defer_tags.get() {
            self.tags_pending.borrow_mut().extend(uids);
        }
    }

    /// Number of mails, whose keywords or files need writing to notmuch
    pub fn pending_tags(&self) -> usize {
        self.tags_pending.borrow().len() + self.renames_pending.borrow().len()
    }

    /// Writes renamed files and keywords of mails changed since the last call to notmuch
    pub async fn flush_tags(&self) -> Result<(), TagsError> {
        let _flushing = self.flushing.lock().await;
        let renames = self.renames_pending.borrow_mut().take();
        if let Some(tags) = &self.tags
            && !renames.is_empty()
        {
            let (written, failed) = write_briefly(
                tags,
                renames.clone(),
                true,
                |database, options, _, (from, to)| notmuch::rename(database, options, from, to),
            )
            .await;
            if let Some(e) = failed {
                // the failed change got discarded, so notmuch still knows the former names
                let mut pending = self.renames_pending.borrow_mut();
                let later = pending.take();
                for (from, to) in renames.into_iter().skip(written.len()).chain(later) {
                    pending.add(from, to);
                }
                return Err(e.into());
            }
        }
        let pending: Vec<Uid> = self.tags_pending.borrow_mut().drain().collect();
        if let Some(tags) = &self.tags
            && !pending.is_empty()
        {
            let pending = self.state.tags_pending_of(pending).await?;
            for chunk in pending.chunks(TAG_SYNC_BATCH_SIZE) {
                self.sync_tags_of(tags, chunk).await?;
            }
        }

        Ok(())
    }

    /// Syncs tags of mails downloaded by the initial sync after completing it
    pub fn defer_tag_sync(&mut self) {
        self.defer_tags.set(true);
    }

    /// Writes keywords of all mails, that differ from the ones notmuch got last, to notmuch.
    /// Indexes mails unknown to notmuch.
    pub async fn sync_pending_tags(&self) -> Result<(), TagsError> {
        let Some(tags) = &self.tags else {
            return Ok(());
        };
        self.tags_pending.borrow_mut().clear();
        // in pages, as e.g. syncing a mailbox for the first time concerns all mails
        let mut after = 0;
        loop {
            let pending = self.state.tags_pending(after, TAG_SYNC_BATCH_SIZE).await?;
            let Some((last, ..)) = pending.last() else {
                break;
            };
            after = u32::from(last.uid());
            self.sync_tags_of(tags, &pending).await?;
        }

        Ok(())
    }

    /// Writes the keywords of `pending` mails to notmuch
    async fn sync_tags_of(
        &self,
        tags: &SyncedTags,
        pending: &[(LocalMailMetadata, Keywords, Option<Keywords>)],
    ) -> Result<(), TagsError> {
        // e.g. renamed or deleted by other programs, which gets synced before
        let existing: Vec<_> = stream::iter(pending)
            .map(|entry| async move {
                let path = self.maildir.get_path_of(&entry.0);
                let exists = compio::fs::metadata(&path).await.is_ok();
                if !exists {
                    log::debug!("cannot sync tags of missing mail {}", entry.0.uid());
                }
                exists.then_some((entry, path))
            })
            .buffered(MAX_CONCURRENT_FILE_OPS)
            .filter_map(std::future::ready)
            .collect()
            .await;
        if existing.is_empty() {
            return Ok(());
        }
        log::debug!("syncing tags of {} mails to notmuch", existing.len());
        let updates: Vec<TagUpdate> = existing
            .iter()
            .map(|((_, keywords, synced), path)| TagUpdate {
                path: path.clone(),
                synced: synced.as_ref().map(|synced| tags.mapping.tags(synced)),
                tags: tags.mapping.tags(keywords),
            })
            .collect();
        // tags only in notmuch of merged mails need syncing to the server
        let merging = existing.iter().any(|((_, _, known), _)| known.is_none());
        let (written, failed) = write_briefly(
            tags,
            updates,
            !merging,
            |database, options, mapping, update| {
                notmuch::update(database, mapping, options, update)
            },
        )
        .await;
        let mut synced = Vec::new();
        let mut local = Vec::new();
        for (((entry, keywords, known), path), written) in existing.into_iter().zip(written) {
            let Some(kept) = written else {
                if compio::fs::metadata(&path).await.is_ok() {
                    // e.g. no mail, which retrying cannot change
                    log::debug!(
                        "notmuch cannot index mail {}, not syncing its tags",
                        entry.uid()
                    );
                    synced.push((entry.uid(), keywords.clone()));
                } else {
                    log::debug!("cannot sync tags of missing mail {}", entry.uid());
                }
                continue;
            };
            synced.push((entry.uid(), keywords.clone()));
            if known.is_none() {
                local.push((
                    entry.uid(),
                    tags.mapping.keywords(kept.iter().map(String::as_str)),
                ));
            }
        }
        // also with a failed change, as notmuch got the ones before
        self.state.set_notmuch_keywords_all(synced).await?;
        if !local.is_empty() {
            self.state.set_notmuch_local_all(local).await?;
        }

        failed.map_or(Ok(()), |e| Err(e.into()))
    }

    /// Informs notmuch about mail files renamed `from` `to` with the next
    /// [`Self::flush_tags`]
    fn rename_in_notmuch(&self, renames: Vec<(PathBuf, PathBuf)>) {
        if self.tags.is_none() {
            return;
        }
        let mut pending = self.renames_pending.borrow_mut();
        for (from, to) in renames {
            pending.add(from, to);
        }
    }

    /// Removes files from notmuch, whose removal got delayed long enough
    pub async fn remove_delayed_from_notmuch(&self) -> Result<(), TagsError> {
        let Some(tags) = &self.tags else {
            return Ok(());
        };
        let due = self
            .state
            .notmuch_removals(unix_now() - NOTMUCH_REMOVAL_DELAY.as_secs().cast_signed())
            .await?;
        for chunk in due.chunks(TAG_SYNC_BATCH_SIZE) {
            tags.notmuch.remove(chunk.to_vec(), false).await?;
            self.state.delete_notmuch_removals(chunk.to_vec()).await?;
        }

        Ok(())
    }

    /// Changes of synced tags in notmuch since the last call, whose revision gets stored with
    /// [`Self::complete_tag_changes`] after syncing them. Syncs all tags from the server instead,
    /// if notmuch got synced for the first time or its database got recreated.
    pub async fn detect_tag_changes(&self) -> Result<LocalChanges, TagsError> {
        let mut changes = LocalChanges::default();
        let Some(tags) = &self.tags else {
            return Ok(changes);
        };
        self.flush_tags().await?;
        self.remove_delayed_from_notmuch().await?;
        // e.g. writes skipped, as the file got renamed meanwhile
        self.sync_pending_tags().await?;
        let current_revision = tags.notmuch.revision().await?;
        let fingerprint = tags.mapping.fingerprint();
        let checked = match self.state.tags_revision().await? {
            Some((checked, mapping))
                if mapping == fingerprint && checked.uuid == current_revision.uuid =>
            {
                checked
            }
            checked => {
                if checked.is_some() {
                    // e.g. by `notmuch compact` or recreating the database
                    log::warn!(
                        "notmuch database or synced tags changed. Merging tags of all mails with keywords of server"
                    );
                    self.state.forget_notmuch_keywords().await?;
                } else {
                    log::info!("syncing tags of all mails to notmuch");
                }
                self.sync_pending_tags().await?;
                // tags only in notmuch get synced to the server
                let checked = Revision {
                    uuid: current_revision.uuid,
                    number: 0,
                };
                self.state
                    .set_tags_revision(checked.clone(), fingerprint)
                    .await?;
                checked
            }
        };
        // after own changes
        let revision = tags.notmuch.revision().await?;
        if checked.number >= revision.number {
            return Ok(changes);
        }
        let (from, to) = (checked.number + 1, revision.number);
        let current = self.tagged_mails(tags, from, to).await?;
        let synced = self
            .state
            .notmuch_keywords(current.keys().copied().collect())
            .await?;
        let mut local_changes = Vec::new();
        for (uid, tags_of_mail) in current {
            // mails unknown to notmuch before get synced from the server
            let Some((Some(synced), local)) = synced.get(&uid) else {
                continue;
            };
            // removed tags of `new.tags`, that stayed local, get synced again once re-added
            let still_local: Keywords = local
                .iter()
                .filter(|keyword| {
                    tags.mapping
                        .tag(keyword)
                        .is_some_and(|tag| tags_of_mail.contains(&tag))
                })
                .map(str::to_string)
                .collect();
            if still_local != *local {
                local_changes.push((uid, still_local.clone()));
            }
            let change = Self::tag_change(&tags.mapping, synced, &still_local, &tags_of_mail);
            if !change.is_empty() {
                log::info!(
                    "tags of mail {uid} changed: added {}, removed {}",
                    change.added,
                    change.removed
                );
                changes.updates.insert(uid, FlagChange::of_keywords(change));
            }
        }
        if !local_changes.is_empty() {
            self.state.set_notmuch_local_all(local_changes).await?;
        }
        changes.tags_revision = Some(revision);

        Ok(changes)
    }

    /// Synced tags of mails with tags changed in the revisions `from` to `to` by UID
    async fn tagged_mails(
        &self,
        tags: &SyncedTags,
        from: u64,
        to: u64,
    ) -> Result<HashMap<Uid, BTreeSet<String>>, TagsError> {
        let mapping = tags.mapping.clone();
        let folder = self.maildir.cur().to_path_buf();
        let tagged = tags
            .notmuch
            .read(move |database| notmuch::changed(database, &mapping, &folder, from, to))
            .await?;
        let mut current: HashMap<Uid, BTreeSet<String>> = HashMap::new();
        for (path, tags) in tagged {
            if let Some(name) = path.file_name().and_then(|name| name.to_str())
                && let Ok(mail) = LocalMailMetadata::from_str(name)
            {
                current.entry(mail.uid()).or_default().extend(tags);
            }
        }

        Ok(current)
    }

    /// Change of the synced keywords of a mail with `tags` in notmuch, ignoring tags of `local`
    /// keywords
    fn tag_change(
        mapping: &TagMapping,
        synced: &Keywords,
        local: &Keywords,
        tags: &BTreeSet<String>,
    ) -> KeywordChange {
        let local: BTreeSet<String> = local
            .iter()
            .filter_map(|keyword| mapping.tag(keyword))
            .collect();
        // compared as tags, so that removed keywords keep the server's spelling, e.g. of
        // labels not encoded like notmuch tags would be
        let synced: HashMap<String, &str> = synced
            .iter()
            .filter_map(|keyword| Some((mapping.tag(keyword)?, keyword)))
            .collect();
        KeywordChange {
            added: mapping.keywords(
                tags.iter()
                    .map(String::as_str)
                    .filter(|tag| !synced.contains_key(*tag) && !local.contains(*tag)),
            ),
            removed: synced
                .iter()
                .filter(|(tag, _)| !tags.contains(*tag))
                .map(|(_, keyword)| (*keyword).to_string())
                .collect(),
        }
    }

    /// Stores, that tag changes up to `revision` got synced
    pub async fn complete_tag_changes(&self, revision: Revision) -> Result<(), state::Error> {
        let Some(tags) = &self.tags else {
            return Ok(());
        };
        self.state
            .set_tags_revision(revision, tags.mapping.fingerprint())
            .await
    }

    fn record(&self, change: impl FnOnce() -> MailChange) {
        if let Some(changes) = &*self.changes.borrow() {
            changes.record(change());
        }
    }

    pub async fn try_init(
        uid_validity: UidValidity,
        mail_dir: &Path,
        state_dir: &Path,
    ) -> Result<Self, InitError> {
        let (mail, state) = futures::join!(
            Maildir::try_init(mail_dir),
            State::init(state_dir, uid_validity)
        );
        let (mail, state) = (mail?, state?);

        Ok(Self::new(mail, state))
    }

    pub async fn load(mail_dir: &Path, state_dir: &Path) -> Result<Self, LoadError> {
        match futures::join!(State::load(state_dir), Maildir::load(mail_dir)) {
            (Ok(state), Ok(mail)) => {
                let repo = Self::new(mail, state);

                Ok(repo)
            }
            (Err(state::InitError::Missing(_)), Err(maildir::LoadError::Missing(_))) => {
                Err(LoadError::Uninitialized)
            }
            // e.g. left by a crash during initialization
            (Err(state::InitError::Missing(_)), Ok(mail)) if mail.is_empty()? => {
                Err(LoadError::Uninitialized)
            }
            (Ok(state), Err(maildir_error)) => match maildir_error {
                maildir::LoadError::Missing(_) => {
                    log::warn!(
                        "encountered missing maildir for existing state. Reinitilizing state and maildir..."
                    );
                    state
                        .close()
                        .await
                        .map_err(|e| LoadError::State(e.into()))?;
                    State::remove_from(state_dir).await?;

                    Err(LoadError::Uninitialized)
                }
                e => Err(LoadError::Maildir(e)),
            },
            (Err(e), _) => Err(LoadError::State(e)),
        }
    }

    fn update_flag_changes(
        flag_changes_builder: &mut LocalFlagChangesBuilder,
        old: &LocalMailMetadata,
        new: &LocalMailMetadata,
    ) {
        debug_assert_eq!(old.uid(), new.uid());
        flag_changes_builder.insert(
            new.uid(),
            FlagChange::new(
                new.additional_flags_compared_to(old),
                new.removed_flags_compared_to(old),
            ),
        );
    }

    /// Waits for the state to be written completely, so that it can be loaded again right away
    pub async fn close(self) -> Result<(), state::Error> {
        self.state.close().await
    }

    pub fn local_watcher(&self) -> LocalWatcher {
        LocalWatcher {
            maildir: self.maildir.clone(),
        }
    }

    pub async fn uid_validity(&self) -> Result<UidValidity, state::Error> {
        self.state.uid_validity().await
    }

    pub async fn highest_modseq(&self) -> Result<ModSeq, state::Error> {
        self.state.highest_modseq().await
    }

    pub async fn update_highest_modseq(&self, value: ModSeq) -> Result<(), state::Error> {
        self.state.update_highest_modseq(value).await
    }

    pub async fn complete_initial_sync(
        &self,
        highest_modseq: ModSeq,
        keyword_source: KeywordSource,
    ) -> Result<(), state::Error> {
        self.defer_tags.set(false);
        self.state
            .complete_initial_sync(highest_modseq, keyword_source)
            .await
    }

    pub async fn keyword_source(&self) -> Result<Option<KeywordSource>, state::Error> {
        self.state.keyword_source().await
    }

    pub async fn set_keyword_source(&self, source: KeywordSource) -> Result<(), state::Error> {
        self.state.set_keyword_source(source).await
    }

    pub async fn is_initial_sync_complete(&self) -> Result<bool, state::Error> {
        self.state.is_initial_sync_complete().await
    }

    pub async fn update_other_flags_all(
        &self,
        other_flags: Vec<(Uid, OtherFlags)>,
    ) -> Result<(), state::Error> {
        self.state.update_other_flags_all(other_flags).await
    }

    pub async fn uids(&self) -> Result<HashSet<Uid>, state::Error> {
        self.state.uids().await
    }

    pub async fn uids_not_in(&self, uids: Vec<Uid>) -> Result<Vec<Uid>, state::Error> {
        self.state.uids_not_in(uids).await
    }

    /// Stored flags and keywords of `uids`
    pub async fn flags(
        &self,
        uids: Vec<Uid>,
    ) -> Result<HashMap<Uid, (BitFlags<Flag>, Keywords)>, state::Error> {
        let mut keywords = self.state.keywords(uids.clone()).await?;
        Ok(self
            .state
            .get_all_by_id(uids)
            .await?
            .into_iter()
            .map(|entry| {
                let uid = entry.uid();
                (
                    uid,
                    (entry.flags(), keywords.remove(&uid).unwrap_or_default()),
                )
            })
            .collect())
    }

    pub async fn store_batch(&self, mails: &[RemoteMail]) -> Result<(), StoreError> {
        for mail in mails {
            log::info!(
                "storing mail {} with flags {}",
                mail.metadata().uid(),
                mail.metadata().flags()
            );
        }
        let stored: Vec<LocalMailMetadata> =
            futures::future::join_all(mails.iter().map(|mail| self.maildir.write_tmp(mail)))
                .await
                .into_iter()
                .collect::<Result<_, _>>()?;
        self.maildir.sync_tmp().await?;
        let keywords: HashMap<Uid, Keywords> = mails
            .iter()
            .filter_map(|mail| {
                let metadata = mail.metadata();
                metadata
                    .keywords()
                    .map(|keywords| (metadata.uid(), keywords.clone()))
            })
            .collect();
        let mut recorded = if self.records_changes() {
            keywords.clone()
        } else {
            HashMap::new()
        };

        let (stored, conflicts) = self.state.store_all(stored, keywords).await?;
        stream::iter(&stored)
            .map(|mail| self.maildir.move_from_tmp(mail))
            .buffered(MAX_CONCURRENT_FILE_OPS)
            .try_collect::<Vec<()>>()
            .await?;
        self.maildir.sync_cur().await?;
        self.mark_tags_pending(stored.iter().map(LocalMailMetadata::uid));
        for mail in &stored {
            self.record_downloaded(mail, recorded.remove(&mail.uid()).unwrap_or_default());
        }

        for conflict in conflicts {
            if let Some(existing_mail) = self.state.get_by_id(conflict.uid()).await? {
                self.maildir
                    .handle_uid_conflict(&existing_mail, &conflict)
                    .await?;
            } else {
                unreachable!("stored mail should exist on store ConstraintViolation");
            }
        }

        Ok(())
    }

    fn record_downloaded(&self, mail: &LocalMailMetadata, keywords: Keywords) {
        self.record(|| MailChange::Downloaded {
            uid: mail.uid(),
            path: self.maildir.get_path_of(mail),
            flags: mail.info(),
            keywords,
        });
    }

    /// Opens a new mail for uploading it. Returns `None` if it vanished meanwhile.
    pub async fn open_mail(
        &self,
        metadata: NewLocalMailMetadata,
    ) -> Result<Option<LocalMail>, maildir::Error> {
        match self.maildir.open(&metadata).await {
            Ok((file, size)) => Ok(Some(LocalMail::new(metadata, file, size))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                log::warn!("new mail {metadata} vanished before syncing it");
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Stores keywords of mails reported by the server and records their changes
    async fn apply_remote_keywords(&self, updates: Vec<(Uid, Keywords)>) -> Result<(), Error> {
        let mut stored = self
            .state
            .keywords(updates.iter().map(|(uid, _)| *uid).collect())
            .await?;
        let mut new_keywords = Vec::new();
        let mut changes = HashMap::new();
        for (uid, keywords) in updates {
            let Some(old) = stored.remove(&uid) else {
                continue;
            };
            if old != keywords {
                log::info!("update keywords of mail {uid}: {old} -> {keywords}");
                changes.insert(uid, KeywordChange::between(&old, &keywords));
                new_keywords.push((uid, keywords));
            }
        }
        if new_keywords.is_empty() {
            return Ok(());
        }
        let uids: Vec<Uid> = new_keywords.iter().map(|(uid, _)| *uid).collect();
        self.state.update_keywords_all(new_keywords).await?;
        self.mark_tags_pending(uids);
        if self.records_changes() {
            let entries = self
                .state
                .get_all_by_id(changes.keys().copied().collect())
                .await?;
            for entry in entries {
                if let Some(KeywordChange { added, removed }) = changes.remove(&entry.uid()) {
                    self.record(|| MailChange::KeywordsChanged {
                        uid: entry.uid(),
                        path: self.maildir.get_path_of(&entry),
                        added,
                        removed,
                    });
                }
            }
        }

        Ok(())
    }

    /// Applies a flag change, that got accepted by the server, to the local mails. Mails, that
    /// vanished locally meanwhile, e.g. due to a concurrent local deletion, are skipped.
    #[cfg(test)]
    pub async fn apply_flag_changes(&self, uids: &[Uid], change: &FlagChange) -> Result<(), Error> {
        self.apply_flag_changes_all(&[(uids.to_vec(), change.clone())])
            .await
    }

    /// Applies stored changes to each of their mails at once
    pub async fn apply_flag_changes_all(
        &self,
        changes: &[(Vec<Uid>, FlagChange)],
    ) -> Result<(), Error> {
        let mut by_uid: HashMap<Uid, FlagChange> = HashMap::new();
        for (uids, change) in changes {
            for uid in uids {
                by_uid
                    .entry(*uid)
                    .and_modify(|before| *before = before.then(change))
                    .or_insert_with(|| change.clone());
            }
        }
        let keyword_uids: Vec<Uid> = by_uid
            .iter()
            .filter(|(_, change)| !change.keywords.is_empty())
            .map(|(uid, _)| *uid)
            .collect();
        if !keyword_uids.is_empty() {
            let keywords = self
                .state
                .keywords(keyword_uids.clone())
                .await?
                .into_iter()
                .filter_map(|(uid, keywords)| {
                    let new = by_uid.get(&uid)?.keywords.apply(&keywords);
                    (new != keywords).then_some((uid, new))
                })
                .collect::<Vec<_>>();
            if !keywords.is_empty() {
                self.state.update_keywords_all(keywords).await?;
            }
            // keyword changes come from notmuch, which already has them. Writing them again
            // would revert later changes in notmuch.
            let synced: Vec<(Uid, Keywords)> = self
                .state
                .notmuch_keywords(keyword_uids)
                .await?
                .into_iter()
                .filter_map(|(uid, (synced, _))| {
                    let synced = synced?;
                    let new = by_uid.get(&uid)?.keywords.apply(&synced);
                    (new != synced).then_some((uid, new))
                })
                .collect();
            if !synced.is_empty() {
                self.state.set_notmuch_keywords_all(synced).await?;
            }
        }
        let entries = self
            .state
            .get_all_by_id(by_uid.keys().copied().collect())
            .await?;
        if entries.len() != by_uid.len() {
            let existing: HashSet<Uid> = entries.iter().map(LocalMailMetadata::uid).collect();
            for uid in by_uid.keys().filter(|uid| !existing.contains(uid)) {
                log::warn!("ignoring flag change of locally missing mail {uid}");
            }
        }
        let flag_changes = entries
            .into_iter()
            .filter_map(|entry| {
                let change = &by_uid[&entry.uid()];
                let new_flags = (entry.flags() | change.added) & !change.removed;
                (entry.flags() != new_flags).then(|| {
                    log::info!(
                        "updating flags of mail {}: {} -> {new_flags}",
                        entry.uid(),
                        entry.flags()
                    );
                    (entry, new_flags)
                })
            })
            .collect();
        // the local change is already part of the local files
        for uid in self.set_flags_all(flag_changes, false).await? {
            log::warn!("ignoring flag change of locally missing mail {uid}");
        }
        // tags may not have been written, while the stored file name was outdated
        self.mark_tags_pending(by_uid.into_keys());

        Ok(())
    }

    /// Records the flag changes as remote changes if `remote`
    async fn set_flags_all(
        &self,
        changes: Vec<(LocalMailMetadata, BitFlags<Flag>)>,
        remote: bool,
    ) -> Result<Vec<Uid>, Error> {
        let record = remote && self.records_changes();
        let track = record || self.tags.is_some();
        // all results, as files renamed before a failure must still get stored
        let results: Vec<Result<Result<FlagUpdate, Uid>, maildir::Error>> = stream::iter(changes)
            .map(|(mut entry, new_flags)| async move {
                let old_flags = track.then(|| entry.info());
                match self.maildir.update_flags(&mut entry, new_flags).await {
                    Ok(from) => Ok(Ok((entry, from, old_flags))),
                    Err(maildir::Error::Missing(_)) => Ok(Err(entry.uid())),
                    Err(e) => Err(e),
                }
            })
            .buffered(MAX_CONCURRENT_FILE_OPS)
            .collect()
            .await;
        let mut updated = Vec::with_capacity(results.len());
        let mut renames = Vec::new();
        let mut missing = Vec::new();
        let mut failed = None;
        for result in results {
            match result {
                Ok(Ok((entry, from, old_flags))) => {
                    if let Some(from) = from
                        && let Some(old_flags) = old_flags
                    {
                        renames.push((entry.clone(), from, old_flags));
                    }
                    updated.push(entry);
                }
                Ok(Err(uid)) => missing.push(uid),
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        if !updated.is_empty() {
            self.state.update_all(updated).await?;
        }
        self.rename_in_notmuch(
            renames
                .iter()
                .map(|(entry, from, _)| (from.clone(), self.maildir.get_path_of(entry)))
                .collect(),
        );
        if record {
            for (entry, from, old) in renames {
                self.record_flags_changed(&entry, from, old);
            }
        }
        if !missing.is_empty() {
            self.state.delete_all_by_id(missing.clone()).await?;
        }
        if let Some(e) = failed {
            return Err(e.into());
        }

        Ok(missing)
    }

    fn record_flags_changed(&self, entry: &LocalMailMetadata, from: PathBuf, old: String) {
        self.record(|| MailChange::FlagsChanged {
            uid: entry.uid(),
            from,
            to: self.maildir.get_path_of(entry),
            old,
            new: entry.info(),
        });
    }

    /// Tracks uploaded mails with their new UIDs. Tracks all of them even if renaming some fails,
    /// as untracked mails get uploaded again. The state keeps the uploaded flags, so that differing
    /// local files, e.g. renamed or deleted by other clients, are detected as local changes.
    pub async fn add_synced_all(
        &self,
        synced: Vec<(NewLocalMailMetadata, Uid)>,
    ) -> Result<(), Error> {
        let results: Vec<_> = stream::iter(synced)
            .map(|(mail_metadata, uid)| async move {
                log::info!(
                    "adding {uid} to newly synced mail {}",
                    mail_metadata.filename()
                );
                let mut synced = mail_metadata.clone().set_uid(uid);
                let result = self.maildir.update_uid(mail_metadata, uid).await;
                // keeps the uploaded flags, but other clients may have changed other flags
                if let Ok((renamed, _)) = &result {
                    synced.set_other_flags(renamed.other_flags().clone());
                }

                (synced, result)
            })
            .buffered(MAX_CONCURRENT_FILE_OPS)
            .collect()
            .await;
        let mut error = None;
        let mut stored = Vec::with_capacity(results.len());
        let mut uploaded = Vec::with_capacity(results.len());
        for (synced, result) in results {
            match result {
                Ok((renamed, from)) => uploaded.push((renamed, from)),
                Err(maildir::Error::Missing(path)) => {
                    log::warn!("newly synced mail {} vanished", path.display());
                }
                Err(e) => {
                    log::error!("cannot add uid {} to newly synced mail: {e}", synced.uid());
                    error.get_or_insert(e);
                }
            }
            stored.push(synced);
        }
        let uids: Vec<Uid> = stored.iter().map(LocalMailMetadata::uid).collect();
        self.state.insert_all(stored).await?;
        self.rename_in_notmuch(
            uploaded
                .iter()
                .map(|(renamed, from)| (from.clone(), self.maildir.get_path_of(renamed)))
                .collect(),
        );
        // merges tags of notmuch, which then get synced to the server, except ones of
        // `new.tags`
        self.mark_tags_pending(uids);
        for (renamed, from) in uploaded {
            self.record(|| MailChange::Uploaded {
                uid: renamed.uid(),
                from,
                to: self.maildir.get_path_of(&renamed),
                flags: renamed.info(),
            });
        }

        error.map_or(Ok(()), |e| Err(e.into()))
    }

    pub async fn delete_all(&self, uids: impl Iterator<Item = Uid>) -> Result<(), DeleteError> {
        // notmuch must know the current file names, which are about to vanish
        self.flush_tags().await?;
        let entries = self.state.get_all_by_id(uids.collect()).await?;
        let deleted = stream::iter(entries)
            .map(|entry| async move {
                log::info!("deleting mail {}", entry.uid());
                let expected = self.maildir.get_path_of(&entry);
                self.maildir
                    .delete(&entry)
                    .await
                    .map(|path| (entry.uid(), path, expected))
            })
            .buffered(MAX_CONCURRENT_FILE_OPS)
            .try_collect::<Vec<_>>()
            .await?;
        // mails deleted locally before vanished from their stored path
        let removed: Vec<PathBuf> = deleted
            .iter()
            .map(|(_, path, expected)| path.clone().unwrap_or_else(|| expected.clone()))
            .collect();
        if !deleted.is_empty() {
            // also once the state forgot the mails, failed removals get retried
            let removals = if self.tags.is_some() {
                removed.clone()
            } else {
                Vec::new()
            };
            self.state
                .delete_all_by_id_removing(
                    deleted.iter().map(|(uid, ..)| *uid).collect(),
                    removals,
                    unix_now(),
                )
                .await?;
        }
        for (uid, path, _) in deleted {
            if let Some(path) = path {
                self.record(|| MailChange::Deleted { uid, path });
            }
        }
        // last files of messages stay for [`NOTMUCH_REMOVAL_DELAY`], so that copies of the
        // message downloaded meanwhile, e.g. after moving it between mailboxes on the server,
        // keep its tags. Their delayed removal also retries failed ones, as the paths got
        // remembered when deleting the mails.
        if let Some(tags) = &self.tags
            && !removed.is_empty()
        {
            // removed files may have been renamed before
            self.flush_tags().await?;
            // in batches, as notmuch blocks other writers meanwhile
            for chunk in removed.chunks(TAG_SYNC_BATCH_SIZE) {
                tags.notmuch
                    .remove(chunk.to_vec(), true)
                    .await
                    .map_err(TagsError::from)?;
            }
        }

        Ok(())
    }

    pub async fn apply_remote_updates(
        &self,
        updates: &[RemoteMailMetadata],
    ) -> Result<Vec<Uid>, Error> {
        // e.g. unsolicited FETCH responses during fetching report mails again. Later ones are
        // newer, but may lack keywords, e.g. Gmail labels.
        let mut remote_flags: HashMap<Uid, BitFlags<Flag>> = HashMap::new();
        let mut remote_keywords: HashMap<Uid, &Keywords> = HashMap::new();
        for update in updates {
            remote_flags.insert(update.uid(), update.flags());
            if let Some(keywords) = update.keywords() {
                remote_keywords.insert(update.uid(), keywords);
            }
        }
        let entries = self
            .state
            .get_all_by_id(remote_flags.keys().copied().collect())
            .await?;
        let changes: Vec<_> = entries
            .into_iter()
            .filter_map(|entry| {
                let new_flags = remote_flags.remove(&entry.uid())?;
                (entry.flags() != new_flags).then(|| {
                    log::info!(
                        "update flags of mail {}: {} -> {new_flags}",
                        entry.uid(),
                        entry.flags()
                    );
                    (entry, new_flags)
                })
            })
            .collect();
        let mut missing: Vec<Uid> = remote_flags.into_keys().collect();
        let gone = self.set_flags_all(changes, true).await?;
        missing.extend(gone);
        let skipped: HashSet<Uid> = missing.iter().copied().collect();
        let keywords = remote_keywords
            .into_iter()
            .filter(|(uid, _)| !skipped.contains(uid))
            .map(|(uid, keywords)| (uid, keywords.clone()))
            .collect::<Vec<_>>();
        if !keywords.is_empty() {
            self.apply_remote_keywords(keywords).await?;
        }

        Ok(missing)
    }

    pub async fn detect_changes(&self) -> Result<LocalChanges, DetectChangesError> {
        self.maildir.move_new_to_cur().await?;
        let maildir_metadata = self.maildir.list_cur().await?;

        let mut maildir_mails: HashMap<Uid, Vec<LocalMailMetadata>> = HashMap::new();

        let mut new_metadata = Vec::new();
        while let Ok(metadata) = maildir_metadata.recv().await {
            let metadata = match metadata {
                Ok(metadata) => metadata,
                Err(maildir::MaildirListError::Missing(path)) => {
                    log::warn!("ignoring {}, which vanished during listing", path.display());
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            match metadata {
                maildir::MaildirEntry::New(new_local_mail_metadata) => {
                    new_metadata.push(new_local_mail_metadata);
                }
                maildir::MaildirEntry::MaybeTracked(local_mail_metadata) => {
                    maildir_mails
                        .entry(local_mail_metadata.uid())
                        .or_default()
                        .push(local_mail_metadata);
                }
            }
        }
        let mut updates = LocalFlagChangesBuilder::default();
        let mut missing = Vec::new();
        let mut duplicates = Vec::new();
        let mut other_flag_changes = Vec::new();

        self.state
            .fore_each(|entry| {
                let uid = entry.uid();
                if let Some(mut files) = maildir_mails.remove(&uid) {
                    // e.g. copies by other clients carry the same uid, but a different prefix
                    let tracked = files
                        .iter()
                        .position(|file| file.fileprefix() == entry.fileprefix())
                        .unwrap_or_default();
                    let data = files.swap_remove(tracked);
                    Self::update_flag_changes(&mut updates, &entry, &data);
                    // keeps the stored file name accurate, so renames need not search the file
                    if data.fileprefix() == entry.fileprefix()
                        && data.other_flags() != entry.other_flags()
                    {
                        other_flag_changes.push((uid, data.other_flags().clone()));
                    }
                    duplicates.extend(files);
                } else {
                    missing.push(entry);
                }
            })
            .await?;
        if !other_flag_changes.is_empty() {
            self.state
                .update_other_flags_all(other_flag_changes)
                .await?;
        }
        let mut deletions = Vec::new();
        for entry in missing {
            if self.maildir.recover_from_tmp(&entry).await? {
                log::warn!("recovered mail {} from tmp", entry.uid());
                let keywords = if self.records_changes() {
                    let uid = entry.uid();
                    self.state
                        .keywords(vec![uid])
                        .await?
                        .remove(&uid)
                        .unwrap_or_default()
                } else {
                    Keywords::default()
                };
                self.record_downloaded(&entry, keywords);
            } else {
                deletions.push(entry.uid());
            }
        }
        let untracked = stream::iter(maildir_mails.into_values().flatten().chain(duplicates))
            .map(|maildata| self.maildir.remove_uid(maildata))
            .buffered(MAX_CONCURRENT_FILE_OPS)
            .filter_map(|result| {
                std::future::ready(match result {
                    Err(maildir::Error::Missing(path)) => {
                        log::warn!("ignoring {}, which vanished during listing", path.display());
                        None
                    }
                    result => Some(result),
                })
            })
            .err_into::<DetectChangesError>();
        let news = stream::iter(new_metadata.into_iter().map(Ok))
            .chain(untracked)
            .try_collect()
            .await?;

        let changes = LocalChanges::new(deletions, news, updates);
        log::trace!("{changes:?}");

        Ok(changes)
    }
}

pub struct LocalWatcher {
    maildir: Maildir,
}

#[derive(Debug)]
pub enum LocalEvent {
    Changes(LocalChanges),
    Overflow,
    Error(maildir::Error),
}

impl LocalWatcher {
    /// Folder of the synced mails, e.g. for checking notmuch for their changes
    pub fn cur(&self) -> PathBuf {
        self.maildir.cur().to_path_buf()
    }

    pub async fn watch(self) -> Result<AsyncRx<Array<LocalEvent>>, maildir::Error> {
        log::debug!("listening for maildir mail changes");
        let (changes_tx, changes_rx) = spsc::bounded_async(1);
        let size = 32;
        let rx = self.maildir.watch(size).await?;

        compio::runtime::spawn(async move {
            while let Ok(change) = rx.recv().await {
                let mut deletions = Vec::new();
                let mut news = Vec::new();
                let mut updates = LocalFlagChangesBuilder::default();
                let mut other_flags = Vec::new();
                let mut overflowed = false;
                let mut error = None;
                let mut handle_change = |change| match change {
                    Change::Deletion(entry) => {
                        deletions.push(entry.uid());
                    }
                    Change::New(mail) => {
                        news.push(mail);
                    }
                    Change::Rename { from, to } => {
                        MaildirRepository::update_flag_changes(&mut updates, &from, &to);
                        if from.other_flags() != to.other_flags() {
                            other_flags.push((to.uid(), to.other_flags().clone()));
                        }
                    }
                    Change::Overflow => {
                        overflowed = true;
                    }
                    Change::Error(e) => {
                        error = Some(e);
                    }
                };
                handle_change(change);

                while let Ok(Ok(change)) =
                    compio::time::timeout(Duration::from_millis(100), rx.recv()).await
                {
                    handle_change(change);
                }
                let (event, stop) = if let Some(error) = error {
                    (LocalEvent::Error(error), true)
                } else if overflowed {
                    (LocalEvent::Overflow, false)
                } else {
                    (
                        LocalEvent::Changes(LocalChanges {
                            updates,
                            deletions,
                            news,
                            other_flags,
                            tags_revision: None,
                        }),
                        false,
                    )
                };
                if changes_tx.send(event).await.is_err() || stop {
                    break;
                }
            }
        })
        .detach();

        Ok(changes_rx)
    }
}

/// Syncing tags with notmuch failed
#[derive(Debug, Error)]
pub enum TagsError {
    #[error("{0}")]
    Notmuch(#[from] notmuch::Error),
    #[error("{0}")]
    State(#[from] state::Error),
}

impl TagsError {
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Notmuch(e) => e.is_transient(),
            Self::State(_) => false,
        }
    }
}

#[derive(Debug, Error)]
pub enum InitError {
    #[error("{0}")]
    Maildir(maildir::InitError),
    #[error("{0}")]
    State(state::InitError),
}

impl From<maildir::InitError> for InitError {
    fn from(value: maildir::InitError) -> Self {
        Self::Maildir(value)
    }
}

impl From<state::InitError> for InitError {
    fn from(value: state::InitError) -> Self {
        Self::State(value)
    }
}

#[derive(Debug, Error)]
pub enum DetectChangesError {
    #[error("{0}")]
    Io(io::Error),
    #[error("{0}")]
    Maildir(maildir::Error),
    #[error("{0}")]
    MaildirList(maildir::MaildirListError),
    #[error("{0}")]
    State(state::Error),
}

impl From<maildir::Error> for DetectChangesError {
    fn from(value: maildir::Error) -> Self {
        Self::Maildir(value)
    }
}

impl From<maildir::MaildirListError> for DetectChangesError {
    fn from(value: maildir::MaildirListError) -> Self {
        Self::MaildirList(value)
    }
}

impl From<state::Error> for DetectChangesError {
    fn from(value: state::Error) -> Self {
        Self::State(value)
    }
}

impl From<io::Error> for DetectChangesError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("No maildir repository present")]
    Uninitialized,
    #[error("IO error during loading of maildir repository")]
    Io(io::Error),
    #[error("{0}")]
    Maildir(maildir::LoadError),
    #[error("{0}")]
    State(state::InitError),
}

impl From<io::Error> for LoadError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{0}")]
    Maildir(maildir::Error),
    #[error("{0}")]
    State(state::Error),
}

impl From<maildir::Error> for StoreError {
    fn from(value: maildir::Error) -> Self {
        Self::Maildir(value)
    }
}

impl From<state::Error> for StoreError {
    fn from(value: state::Error) -> Self {
        Self::State(value)
    }
}

#[derive(Debug, Error)]
pub enum DeleteError {
    #[error("{0}")]
    Io(io::Error),
    #[error("{0}")]
    State(state::Error),
    #[error("{0}")]
    Tags(#[from] TagsError),
}

impl From<io::Error> for DeleteError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<state::Error> for DeleteError {
    fn from(value: state::Error) -> Self {
        Self::State(value)
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Maildir(maildir::Error),
    #[error("{0}")]
    State(state::Error),
    #[error("uid {uid} does not exist in state")]
    NoExists { uid: Uid },
}

impl From<maildir::Error> for Error {
    fn from(value: maildir::Error) -> Self {
        Self::Maildir(value)
    }
}

impl From<state::Error> for Error {
    fn from(value: state::Error) -> Self {
        Self::State(value)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, str::FromStr, sync::Arc};

    use assertables::*;
    use rstest::*;
    use tempfile::TempDir;

    use compio::{BufResult, io::AsyncReadAtExt as _};

    use crate::{imap::RemoteContent, maildir::ChangeSink};

    use super::*;

    fn added(flag: Flag) -> FlagChange {
        FlagChange::new(flag.into(), BitFlags::empty())
    }

    fn removed(flag: Flag) -> FlagChange {
        FlagChange::new(BitFlags::empty(), flag.into())
    }

    async fn read_content(repo: &MaildirRepository, metadata: NewLocalMailMetadata) -> Vec<u8> {
        let opened = assert_ok!(repo.open_mail(metadata).await);
        let mail = assert_some!(opened);
        let BufResult(read, content) = mail.file().read_to_end_at(Vec::new(), 0).await;
        assert_ok!(read);
        assert_eq!(content.len(), mail.size());

        content
    }

    #[derive(Default)]
    struct RecordingSink(std::sync::Mutex<Vec<MailChange>>);

    impl RecordingSink {
        fn take(&self) -> Vec<MailChange> {
            std::mem::take(&mut *assert_ok!(self.0.lock()))
        }
    }

    impl ChangeSink for RecordingSink {
        fn record(&self, change: MailChange) {
            assert_ok!(self.0.lock()).push(change);
        }
    }

    fn record_changes(repo: &mut MaildirRepository) -> Arc<RecordingSink> {
        let sink = Arc::new(RecordingSink::default());
        repo.record_changes(Some(ChangeRecorder::new(vec![sink.clone()])));

        sink
    }

    #[fixture]
    fn mail_dir() -> TempDir {
        assert_ok!(TempDir::new())
    }

    #[fixture]
    fn state_dir() -> TempDir {
        assert_ok!(TempDir::new())
    }

    #[fixture]
    fn uid_validity() -> UidValidity {
        assert_ok!(UidValidity::try_from(43))
    }

    #[fixture]
    fn highest_modseq() -> ModSeq {
        assert_ok!(ModSeq::try_from(900))
    }

    struct TestMaildirRepository {
        repo: MaildirRepository,
        mail_dir: TempDir,
        state_dir: TempDir,
    }

    #[fixture]
    async fn repo(
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
        mail_dir: TempDir,
        state_dir: TempDir,
    ) -> TestMaildirRepository {
        let repo = assert_ok!(
            MaildirRepository::try_init(uid_validity, mail_dir.path(), state_dir.path()).await
        );
        assert_ok!(
            repo.complete_initial_sync(highest_modseq, KeywordSource::Flags)
                .await
        );
        TestMaildirRepository {
            repo,
            mail_dir,
            state_dir,
        }
    }

    #[fixture]
    fn mail(highest_modseq: ModSeq) -> RemoteMail {
        RemoteMail::new(
            RemoteMailMetadata::new(Uid::MAX, Flag::Seen.into(), highest_modseq),
            RemoteContent::from_string(String::new()),
        )
    }

    struct RepoWithMail {
        repo: TestMaildirRepository,
        mail: RemoteMail,
    }
    #[fixture]
    async fn repo_with_mail(
        #[future] repo: TestMaildirRepository,
        mail: RemoteMail,
    ) -> RepoWithMail {
        let repo = repo.await;
        assert_ok!(repo.repo.store_batch(std::slice::from_ref(&mail)).await);

        RepoWithMail { repo, mail }
    }

    #[rstest]
    #[compio::test]
    async fn test_init_works_in_empty_dirs(
        uid_validity: UidValidity,
        mail_dir: TempDir,
        state_dir: TempDir,
    ) {
        assert_ok!(
            MaildirRepository::try_init(uid_validity, mail_dir.path(), state_dir.path(),).await
        );
        let mut readdir = assert_ok!(mail_dir.path().read_dir());
        assert_ok!(assert_some!(readdir.next()));
        let mut readdir = assert_ok!(state_dir.path().read_dir());
        assert_ok!(assert_some!(readdir.next()));
    }

    #[rstest]
    #[compio::test]
    async fn test_init_propagates_maildir_error(
        uid_validity: UidValidity,
        mail_dir: TempDir,
        state_dir: TempDir,
    ) {
        let mut permissions = assert_ok!(mail_dir.path().metadata()).permissions();
        permissions.set_readonly(true);
        assert_ok!(fs::set_permissions(mail_dir.path(), permissions));

        let result =
            MaildirRepository::try_init(uid_validity, mail_dir.path(), state_dir.path()).await;
        let result = assert_err!(result);
        assert_matches!(result, InitError::Maildir(_));
    }

    #[rstest]
    #[compio::test]
    async fn test_init_propagates_state_error(
        uid_validity: UidValidity,
        mail_dir: TempDir,
        state_dir: TempDir,
    ) {
        let mut permissions = assert_ok!(state_dir.path().metadata()).permissions();
        permissions.set_readonly(true);
        assert_ok!(fs::set_permissions(state_dir.path(), permissions));

        let result =
            MaildirRepository::try_init(uid_validity, mail_dir.path(), state_dir.path()).await;
        let result = assert_err!(result);
        assert_matches!(result, InitError::State(_));
    }

    #[rstest]
    #[compio::test]
    async fn test_load_treats_empty_maildir_without_state_as_uninitialized(
        uid_validity: UidValidity,
        mail_dir: TempDir,
        state_dir: TempDir,
    ) {
        assert_ok!(Maildir::try_init(mail_dir.path()).await);

        let result = assert_err!(MaildirRepository::load(mail_dir.path(), state_dir.path()).await);
        assert_matches!(result, LoadError::Uninitialized);
        assert_ok!(
            MaildirRepository::try_init(uid_validity, mail_dir.path(), state_dir.path()).await
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_load_loads_repo_on_existing_repository(#[future] repo: TestMaildirRepository) {
        assert_ok!(MaildirRepository::load(repo.mail_dir.path(), repo.state_dir.path()).await);
    }

    #[rstest]
    #[compio::test]
    async fn test_load_errors_with_uninitialized_on_empty_dirs(
        mail_dir: TempDir,
        state_dir: TempDir,
    ) {
        let result = assert_err!(MaildirRepository::load(mail_dir.path(), state_dir.path()).await);

        assert_matches!(result, LoadError::Uninitialized);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_load_removes_existing_state_on_uninitialized_maildir(
        #[future] repo: TestMaildirRepository,
    ) {
        assert_ok!(fs::remove_dir_all(repo.mail_dir.path()));
        assert!(assert_ok!(repo.state_dir.path().try_exists()));

        let result =
            assert_err!(MaildirRepository::load(repo.mail_dir.path(), repo.state_dir.path()).await);

        assert_matches!(result, LoadError::Uninitialized);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_load_propagates_maildir_error(#[future] repo: TestMaildirRepository) {
        let mut permissions = assert_ok!(repo.mail_dir.path().metadata()).permissions();
        permissions.set_mode(0o200);
        assert_ok!(fs::set_permissions(repo.mail_dir.path(), permissions));

        let result =
            assert_err!(MaildirRepository::load(repo.mail_dir.path(), repo.state_dir.path()).await);

        assert_matches!(result, LoadError::Maildir(_));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_load_propagates_state_error(#[future] repo: TestMaildirRepository) {
        let mut permissions = assert_ok!(repo.state_dir.path().metadata()).permissions();
        permissions.set_mode(0o200);
        assert_ok!(fs::set_permissions(repo.state_dir.path(), permissions));

        let result =
            assert_err!(MaildirRepository::load(repo.mail_dir.path(), repo.state_dir.path()).await);

        assert_matches!(result, LoadError::State(_));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_uid_validity_returns_uid_validity(
        #[future] repo: TestMaildirRepository,
        uid_validity: UidValidity,
    ) {
        let result = assert_ok!(repo.repo.uid_validity().await);

        assert_eq!(result, uid_validity);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_highest_modseq_returns_highest_modseq(
        #[future] repo: TestMaildirRepository,
        highest_modseq: ModSeq,
    ) {
        let result = assert_ok!(repo.repo.highest_modseq().await);

        assert_eq!(result, highest_modseq);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_update_highest_modseq_updates_highest_modseq(
        #[future] repo: TestMaildirRepository,
    ) {
        let repo = repo.repo;
        let modseq = assert_ok!(ModSeq::try_from(999));
        assert_gt!(modseq, assert_ok!(repo.highest_modseq().await));

        assert_ok!(repo.update_highest_modseq(modseq).await);
        assert_eq!(modseq, assert_ok!(repo.highest_modseq().await));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_store_stores_new_mail(#[future] repo: TestMaildirRepository, mail: RemoteMail) {
        let uid = mail.metadata().uid();
        assert_err!(assert_ok!(repo.repo.maildir.list_cur().await).recv().await);

        assert_ok!(repo.repo.store_batch(std::slice::from_ref(&mail)).await);

        let list_rx = assert_ok!(repo.repo.maildir.list_cur().await);
        assert_ok!(assert_ok!(list_rx.recv().await));
        assert_err!(list_rx.recv().await);

        assert_some!(assert_ok!(repo.repo.state.get_by_id(uid).await));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_store_does_not_store_new_mail_if_mail_with_same_uid_already_present(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let RepoWithMail { repo, mail, .. } = repo_with_mail;
        let uid = mail.metadata().uid();
        let list_rx = assert_ok!(repo.repo.maildir.list_cur().await);
        assert_ok!(assert_ok!(list_rx.recv().await));
        assert_err!(list_rx.recv().await);
        assert_some!(assert_ok!(repo.repo.state.get_by_id(uid).await));

        assert_ok!(repo.repo.store_batch(std::slice::from_ref(&mail)).await);

        let list_rx = assert_ok!(repo.repo.maildir.list_cur().await);
        assert_ok!(assert_ok!(list_rx.recv().await));
        assert_err!(list_rx.recv().await);

        assert_some!(assert_ok!(repo.repo.state.get_by_id(uid).await));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_store_propagaters_maildir_error(
        #[future] repo: TestMaildirRepository,
        mail: RemoteMail,
    ) {
        assert_ok!(fs::remove_dir_all(repo.mail_dir.path()));

        let result = assert_err!(repo.repo.store_batch(std::slice::from_ref(&mail)).await);

        assert_matches!(result, StoreError::Maildir(_));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_update_flags_updates_flags(#[future] repo_with_mail: RepoWithMail) {
        let metadata = repo_with_mail.mail.metadata();
        let flags = Flag::Deleted.into();
        assert_ne!(metadata.flags(), flags);
        let metadata = RemoteMailMetadata::new(metadata.uid(), flags, metadata.modseq());

        assert_is_empty!(assert_ok!(
            repo_with_mail
                .repo
                .repo
                .apply_remote_updates(&[metadata])
                .await
        ));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_update_flags_errors_on_non_existent_mail(
        #[future] repo: TestMaildirRepository,
        mail: RemoteMail,
    ) {
        let metadata = mail.metadata();
        let metadata = RemoteMailMetadata::new(metadata.uid(), metadata.flags(), metadata.modseq());

        let missing = assert_ok!(
            repo.repo
                .apply_remote_updates(std::slice::from_ref(&metadata))
                .await
        );
        assert_eq!(missing, vec![metadata.uid()]);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_add_flag_adds_flag(#[future] repo_with_mail: RepoWithMail) {
        let metadata = repo_with_mail.mail.metadata();
        let flag = Flag::Deleted;
        assert_not_contains!(metadata.flags(), flag);

        assert_ok!(
            repo_with_mail
                .repo
                .repo
                .apply_flag_changes(&[metadata.uid()], &added(flag))
                .await
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_add_flag_does_nothing_if_flag_is_already_present(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let metadata = repo_with_mail.mail.metadata();
        let flag = Flag::Seen;
        assert_contains!(metadata.flags(), flag);

        assert_ok!(
            repo_with_mail
                .repo
                .repo
                .apply_flag_changes(&[metadata.uid()], &added(flag))
                .await
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_add_flag_ignores_non_existent_mail(
        #[future] repo: TestMaildirRepository,
        mail: RemoteMail,
    ) {
        let metadata = mail.metadata();

        assert_ok!(
            repo.repo
                .apply_flag_changes(&[metadata.uid()], &added(Flag::Deleted))
                .await
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_add_flag_removes_mail_state_if_mail_is_not_in_maildir(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        assert_ok!(fs::remove_dir_all(&cur));
        assert_ok!(fs::create_dir(&cur));
        let metadata = repo_with_mail.mail.metadata();

        assert_ok!(
            repo_with_mail
                .repo
                .repo
                .apply_flag_changes(&[metadata.uid()], &added(Flag::Deleted))
                .await
        );
        assert_none!(assert_ok!(
            repo_with_mail
                .repo
                .repo
                .state
                .get_by_id(metadata.uid())
                .await
        ));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_remove_flag_removes_flag(#[future] repo_with_mail: RepoWithMail) {
        let metadata = repo_with_mail.mail.metadata();
        let flag = Flag::Seen;
        assert_contains!(metadata.flags(), flag);

        assert_ok!(
            repo_with_mail
                .repo
                .repo
                .apply_flag_changes(&[metadata.uid()], &removed(flag))
                .await
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_remove_flag_does_nothing_if_flag_is_not_present(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let metadata = repo_with_mail.mail.metadata();
        let flag = Flag::Deleted;
        assert_not_contains!(metadata.flags(), flag);

        assert_ok!(
            repo_with_mail
                .repo
                .repo
                .apply_flag_changes(&[metadata.uid()], &removed(flag))
                .await
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_remove_flag_ignores_non_existent_mail(
        #[future] repo: TestMaildirRepository,
        mail: RemoteMail,
    ) {
        let metadata = mail.metadata();

        assert_ok!(
            repo.repo
                .apply_flag_changes(&[metadata.uid()], &removed(Flag::Seen))
                .await
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_deletes_mail(#[future] repo_with_mail: RepoWithMail) {
        let uid = repo_with_mail.mail.metadata().uid();
        assert_ok!(
            repo_with_mail
                .repo
                .repo
                .delete_all(std::iter::once(uid))
                .await
        );
        assert_err!(
            assert_ok!(repo_with_mail.repo.repo.maildir.list_cur().await)
                .recv()
                .await
        );
        assert_none!(assert_ok!(
            repo_with_mail.repo.repo.state.get_by_id(uid).await
        ));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_does_nothing_on_mail_missing_in_state(
        #[future] repo: TestMaildirRepository,
    ) {
        assert_ok!(repo.repo.delete_all(std::iter::once(Uid::MAX)).await);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_add_synced_adds_synced_mail(#[future] repo: TestMaildirRepository) {
        let filename = "foo:2,S";
        let metadata = assert_ok!(NewLocalMailMetadata::from_str(filename));
        assert_ok!(fs::write(
            repo.mail_dir.path().join("cur").join(filename),
            "1"
        ));
        let uid = Uid::MAX;

        assert_ok!(repo.repo.add_synced_all(vec![(metadata, uid)]).await);

        let mail = assert_some!(assert_ok!(repo.repo.state.get_by_id(uid).await));
        assert!(
            repo.mail_dir
                .path()
                .join("cur")
                .join(mail.filename())
                .exists()
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_add_synced_tracks_all_mails_if_one_vanished(
        #[future] repo: TestMaildirRepository,
    ) {
        let cur = repo.mail_dir.path().join("cur");
        let present = assert_ok!(NewLocalMailMetadata::from_str("foo:2,S"));
        assert_ok!(fs::write(cur.join(present.filename()), "1"));
        let vanished = assert_ok!(NewLocalMailMetadata::from_str("bar:2,S"));
        let (present_uid, vanished_uid) = (assert_ok!(Uid::try_from(1)), Uid::MAX);

        assert_ok!(
            repo.repo
                .add_synced_all(vec![(vanished, vanished_uid), (present, present_uid)])
                .await
        );

        assert_some!(assert_ok!(repo.repo.state.get_by_id(present_uid).await));
        assert_some!(assert_ok!(repo.repo.state.get_by_id(vanished_uid).await));
        let changes = assert_ok!(repo.repo.detect_changes().await);
        assert_eq!(changes.deletions, vec![vanished_uid]);
        assert_len_eq_x!(changes.news, 0);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_detect_changes_treats_copy_with_same_uid_as_new(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let repo = repo_with_mail.repo.repo;
        let uid = repo_with_mail.mail.metadata().uid();
        let tracked = assert_some!(assert_ok!(repo.state.get_by_id(uid).await));
        let copy = LocalMailMetadata::new(uid, tracked.flags(), Some("copy".to_string()));
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        assert_ok!(fs::copy(
            cur.join(tracked.filename()),
            cur.join(copy.filename())
        ));

        let result = assert_ok!(repo.detect_changes().await);

        assert_len_eq_x!(result.deletions, 0);
        assert_eq!(result.news, vec![NewLocalMailMetadata::from(copy)]);
        assert!(cur.join(tracked.filename()).exists());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_detect_changes_has_nothing_on_no_changes(#[future] repo_with_mail: RepoWithMail) {
        let repo = repo_with_mail.repo.repo;

        let result = assert_ok!(repo.detect_changes().await);

        assert_len_eq_x!(result.deletions, 0);
        assert_len_eq_x!(result.news, 0);
        let changes = result.updates.build();
        assert_len_eq_x!(changes.iter().collect::<Vec<_>>(), 0);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_detect_changes_detects_new_mail(#[future] repo_with_mail: RepoWithMail) {
        let expected_content = "1";
        let uid = assert_ok!(Uid::try_from(3));
        let flag = Flag::Seen;
        let filename = "f";
        let new_mail_metadata = LocalMailMetadata::new(uid, flag.into(), Some(filename.into()));
        assert_none!(assert_ok!(
            repo_with_mail
                .repo
                .repo
                .state
                .get_by_id(new_mail_metadata.uid())
                .await
        ));
        let expected_filename = format!("{filename}:2,{}", char::from(flag));
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        assert_ok!(fs::write(
            cur.join(new_mail_metadata.filename()),
            expected_content
        ));
        let repo = repo_with_mail.repo.repo;

        let mut result = assert_ok!(repo.detect_changes().await);

        assert_len_eq_x!(&result.news, 1);
        let metadata = assert_some!(result.news.pop());
        let content = read_content(&repo, metadata.clone()).await;
        assert_eq!(content, expected_content.as_bytes());
        assert_eq!(metadata.filename(), expected_filename);
        assert_eq!(metadata.flags(), BitFlags::from_flag(Flag::Seen));
        assert!(cur.join(expected_filename).exists());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_detect_changes_detects_new_mail_without_uid(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let expected_content = "1";
        let expected_filename = "f";
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        assert_ok!(fs::write(cur.join(expected_filename), expected_content));
        let mut expected_filename = expected_filename.to_string();
        expected_filename.push_str(":2,S");
        let repo = repo_with_mail.repo.repo;

        let mut result = assert_ok!(repo.detect_changes().await);

        assert_len_eq_x!(&result.news, 1);
        let metadata = assert_some!(result.news.pop());
        let content = read_content(&repo, metadata.clone()).await;
        assert_eq!(content, expected_content.as_bytes());
        assert_eq!(metadata.filename(), expected_filename);
        assert_eq!(metadata.flags(), BitFlags::from_flag(Flag::Seen));
        assert!(cur.join(expected_filename).exists());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_detect_changes_detects_new_mail_in_new(#[future] repo: TestMaildirRepository) {
        let expected_content = "1";
        let new = repo.mail_dir.path().join("new");
        assert_ok!(fs::write(new.join("f"), expected_content));

        let mut result = assert_ok!(repo.repo.detect_changes().await);

        assert_len_eq_x!(&result.news, 1);
        let metadata = assert_some!(result.news.pop());
        let content = read_content(&repo.repo, metadata.clone()).await;
        assert_eq!(content, expected_content.as_bytes());
        assert_eq!(metadata.filename(), "f:2,");
        assert!(repo.mail_dir.path().join("cur/f:2,").exists());
        assert_eq!(assert_ok!(new.read_dir()).count(), 0);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_detect_changes_detects_flag_changes(#[future] repo_with_mail: RepoWithMail) {
        let repo = repo_with_mail.repo.repo;
        let uid = repo_with_mail.mail.metadata().uid();
        let metadata = assert_some!(assert_ok!(repo.state.get_by_id(uid).await));
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        let mut new_name = metadata.filename();
        new_name.pop();
        new_name.push('T');
        assert_ok!(fs::rename(
            cur.join(metadata.filename()),
            cur.join(new_name)
        ));

        let result = assert_ok!(repo.detect_changes().await);

        let changes = result.updates.build();
        let mut changes: Vec<_> = changes.iter().collect();
        assert_len_eq_x!(&changes, 1);
        let (change, uids) = assert_some!(changes.pop());
        assert_eq!(change.removed, BitFlags::from_flag(Flag::Seen));
        assert_eq!(change.added, BitFlags::from_flag(Flag::Deleted));
        assert_eq!(uids.iter().collect::<Vec<_>>(), vec![uid]);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_detect_changes_keeps_mail_with_other_flags_tracked(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let repo = repo_with_mail.repo.repo;
        let uid = repo_with_mail.mail.metadata().uid();
        let tracked = assert_some!(assert_ok!(repo.state.get_by_id(uid).await));
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        let passed = format!("{},U={uid}:2,PS", tracked.fileprefix());
        assert_ok!(fs::rename(cur.join(tracked.filename()), cur.join(&passed)));

        let result = assert_ok!(repo.detect_changes().await);

        assert_is_empty!(result.deletions);
        assert_is_empty!(result.news);
        assert_is_empty!(result.updates.build().iter().collect::<Vec<_>>());
        assert!(cur.join(passed).exists());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_watcher_reports_changed_other_flags(#[future] repo_with_mail: RepoWithMail) {
        let repo = repo_with_mail.repo.repo;
        let uid = repo_with_mail.mail.metadata().uid();
        let tracked = assert_some!(assert_ok!(repo.state.get_by_id(uid).await));
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        let events = assert_ok!(repo.local_watcher().watch().await);

        assert_ok!(fs::rename(
            cur.join(tracked.filename()),
            cur.join(format!("{},U={uid}:2,PS", tracked.fileprefix()))
        ));

        let event = assert_ok!(compio::time::timeout(Duration::from_secs(5), events.recv()).await);
        let LocalEvent::Changes(changes) = assert_ok!(event) else {
            panic!("rename should be reported as changes");
        };
        assert_eq!(
            changes.other_flags,
            vec![(uid, OtherFlags::from("P".to_string()))]
        );
        assert_is_empty!(changes.updates.build().iter().collect::<Vec<_>>());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_detect_changes_stores_other_flags(#[future] repo_with_mail: RepoWithMail) {
        let repo = repo_with_mail.repo.repo;
        let uid = repo_with_mail.mail.metadata().uid();
        let tracked = assert_some!(assert_ok!(repo.state.get_by_id(uid).await));
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        let passed = format!("{},U={uid}:2,PS", tracked.fileprefix());
        assert_ok!(fs::rename(cur.join(tracked.filename()), cur.join(&passed)));

        assert_ok!(repo.detect_changes().await);

        let stored = assert_some!(assert_ok!(repo.state.get_by_id(uid).await));
        assert_eq!(stored.filename(), passed);
        assert_eq!(stored.flags(), tracked.flags());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_apply_flag_changes_keeps_other_flags(#[future] repo_with_mail: RepoWithMail) {
        let repo = repo_with_mail.repo.repo;
        let uid = repo_with_mail.mail.metadata().uid();
        let tracked = assert_some!(assert_ok!(repo.state.get_by_id(uid).await));
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        assert_ok!(fs::rename(
            cur.join(tracked.filename()),
            cur.join(format!("{},U={uid}:2,PSa", tracked.fileprefix()))
        ));

        assert_ok!(repo.apply_flag_changes(&[uid], &added(Flag::Flagged)).await);

        assert!(
            cur.join(format!("{},U={uid}:2,FPSa", tracked.fileprefix()))
                .exists()
        );
        let stored = assert_some!(assert_ok!(repo.state.get_by_id(uid).await));
        assert_eq!(stored.flags(), Flag::Flagged | Flag::Seen);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_detect_changes_detects_deleted_mail(#[future] repo_with_mail: RepoWithMail) {
        let repo = repo_with_mail.repo.repo;
        let uid = repo_with_mail.mail.metadata().uid();
        let metadata = assert_some!(assert_ok!(repo.state.get_by_id(uid).await));
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        assert_ok!(fs::remove_file(cur.join(metadata.filename())));

        let mut result = assert_ok!(repo.detect_changes().await);

        assert_len_eq_x!(&result.deletions, 1);
        let deleted_mail_uid = assert_some!(result.deletions.pop());
        assert_eq!(deleted_mail_uid, uid);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_detect_changes_recovers_tracked_mail_from_tmp(
        #[future] repo: TestMaildirRepository,
        mail: RemoteMail,
    ) {
        let metadata = LocalMailMetadata::from(mail.metadata());
        assert_ok!(fs::write(
            repo.mail_dir.path().join("tmp").join(metadata.fileprefix()),
            "1"
        ));
        assert_is_empty!(
            assert_ok!(
                repo.repo
                    .state
                    .store_all(vec![metadata.clone()], HashMap::new())
                    .await
            )
            .1
        );

        let result = assert_ok!(repo.repo.detect_changes().await);

        assert_is_empty!(result.deletions);
        assert_is_empty!(result.news);
        assert!(
            repo.mail_dir
                .path()
                .join("cur")
                .join(metadata.filename())
                .exists()
        );
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_store_batch_keeps_conflicting_mail_out_of_cur(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let RepoWithMail { repo, mail } = repo_with_mail;
        let conflicting = RemoteMail::new(
            mail.metadata().clone(),
            RemoteContent::from_string("different".to_string()),
        );

        assert_ok!(
            repo.repo
                .store_batch(std::slice::from_ref(&conflicting))
                .await
        );

        let list_rx = assert_ok!(repo.repo.maildir.list_cur().await);
        assert_ok!(assert_ok!(list_rx.recv().await));
        assert_err!(list_rx.recv().await);
        let tmp_files = assert_ok!(repo.mail_dir.path().join("tmp").read_dir()).count();
        assert_eq!(tmp_files, 1);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_failed_flag_update_keeps_other_renamed_mails(
        #[future] repo: TestMaildirRepository,
        highest_modseq: ModSeq,
    ) {
        let remote = |uid: u32, flags: BitFlags<Flag>| {
            RemoteMailMetadata::new(assert_ok!(Uid::try_from(uid)), flags, highest_modseq)
        };
        let mails: Vec<RemoteMail> = (1..=2)
            .map(|uid| {
                RemoteMail::new(
                    remote(uid, Flag::Seen.into()),
                    RemoteContent::from_string(format!("mail {uid}")),
                )
            })
            .collect();
        assert_ok!(repo.repo.store_batch(&mails).await);
        let mut blocked = assert_some!(assert_ok!(
            repo.repo
                .state
                .get_by_id(assert_ok!(Uid::try_from(2)))
                .await
        ));
        blocked.set_flags(Flag::Flagged.into());
        assert_ok!(fs::write(repo.repo.maildir.get_path_of(&blocked), "other"));

        assert_err!(
            repo.repo
                .apply_remote_updates(&[
                    remote(1, Flag::Flagged.into()),
                    remote(2, Flag::Flagged.into())
                ])
                .await
        );

        let renamed = assert_some!(assert_ok!(
            repo.repo
                .state
                .get_by_id(assert_ok!(Uid::try_from(1)))
                .await
        ));
        assert_eq!(renamed.flags(), Flag::Flagged);
        assert!(repo.repo.maildir.get_path_of(&renamed).exists());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_apply_remote_updates_records_changes(#[future] repo_with_mail: RepoWithMail) {
        let RepoWithMail { mut repo, mail } = repo_with_mail;
        let changes = record_changes(&mut repo.repo);
        let unchanged = mail.metadata().clone();
        let changed =
            RemoteMailMetadata::new(unchanged.uid(), Flag::Flagged.into(), unchanged.modseq());

        let missing = assert_ok!(
            repo.repo
                .apply_remote_updates(std::slice::from_ref(&unchanged))
                .await
        );
        assert_is_empty!(missing);
        assert_is_empty!(changes.take());

        let missing = assert_ok!(repo.repo.apply_remote_updates(&[changed]).await);
        assert_is_empty!(missing);
        let recorded = changes.take();
        let [
            MailChange::FlagsChanged {
                uid,
                from,
                to,
                old,
                new,
            },
        ] = recorded.as_slice()
        else {
            panic!("expected single flag change, got {recorded:?}");
        };
        assert_eq!(*uid, unchanged.uid());
        assert_eq!(old, "S");
        assert_eq!(new, "F");
        assert!(!from.exists());
        assert!(to.exists());
        assert!(to.is_absolute());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_store_batch_stores_all_mails(#[future] repo: TestMaildirRepository) {
        let mails: Vec<_> = (1..=3u32)
            .map(|uid| {
                RemoteMail::new(
                    RemoteMailMetadata::new(
                        assert_ok!(Uid::try_from(uid)),
                        Flag::Seen.into(),
                        assert_ok!(ModSeq::try_from(i64::from(uid))),
                    ),
                    RemoteContent::from_string(uid.to_string()),
                )
            })
            .collect();

        assert_ok!(repo.repo.store_batch(&mails).await);

        let expected: HashSet<_> = mails.iter().map(|mail| mail.metadata().uid()).collect();
        assert_eq!(assert_ok!(repo.repo.uids().await), expected);
        let list_rx = assert_ok!(repo.repo.maildir.list_cur().await);
        let mut count = 0;
        while let Ok(entry) = list_rx.recv().await {
            assert_ok!(entry);
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_store_batch_records_downloads(
        #[future] mut repo: TestMaildirRepository,
        mail: RemoteMail,
    ) {
        let changes = record_changes(&mut repo.repo);

        assert_ok!(repo.repo.store_batch(std::slice::from_ref(&mail)).await);

        let recorded = changes.take();
        let [
            MailChange::Downloaded {
                uid, path, flags, ..
            },
        ] = recorded.as_slice()
        else {
            panic!("expected single download, got {recorded:?}");
        };
        assert_eq!(*uid, mail.metadata().uid());
        assert_eq!(flags, "S");
        assert!(path.exists());
        assert!(path.starts_with(repo.mail_dir.path().join("cur")));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_update_flags_records_flag_change(#[future] repo_with_mail: RepoWithMail) {
        let RepoWithMail { mut repo, mail } = repo_with_mail;
        let changes = record_changes(&mut repo.repo);
        let metadata = mail.metadata();
        let metadata = RemoteMailMetadata::new(
            metadata.uid(),
            Flag::Seen | Flag::Answered,
            metadata.modseq(),
        );

        assert_ok!(repo.repo.apply_remote_updates(&[metadata]).await);

        let recorded = changes.take();
        let [MailChange::FlagsChanged { old, new, to, .. }] = recorded.as_slice() else {
            panic!("expected single flag change, got {recorded:?}");
        };
        assert_eq!(old, "S");
        assert_eq!(new, "RS");
        assert!(to.exists());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_update_flags_onto_identically_renamed_mail_records_nothing(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let RepoWithMail { mut repo, mail } = repo_with_mail;
        let changes = record_changes(&mut repo.repo);
        let metadata = mail.metadata();
        let entry = assert_some!(assert_ok!(repo.repo.state.get_by_id(metadata.uid()).await));
        let mut renamed = entry.clone();
        renamed.set_flags(Flag::Seen | Flag::Flagged);
        // another client already applied the remote change
        assert_ok!(fs::rename(
            repo.repo.maildir.get_path_of(&entry),
            repo.repo.maildir.get_path_of(&renamed)
        ));
        let metadata = RemoteMailMetadata::new(metadata.uid(), renamed.flags(), metadata.modseq());

        assert_ok!(repo.repo.apply_remote_updates(&[metadata]).await);

        assert_is_empty!(changes.take());
        assert!(repo.repo.maildir.get_path_of(&renamed).exists());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_apply_flag_changes_records_nothing(#[future] repo_with_mail: RepoWithMail) {
        let RepoWithMail { mut repo, mail } = repo_with_mail;
        let changes = record_changes(&mut repo.repo);

        assert_ok!(
            repo.repo
                .apply_flag_changes(&[mail.metadata().uid()], &added(Flag::Flagged))
                .await
        );

        assert_is_empty!(changes.take());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_store_batch_stores_and_records_keywords(
        #[future] mut repo: TestMaildirRepository,
        mail: RemoteMail,
    ) {
        let changes = record_changes(&mut repo.repo);
        let metadata = mail.metadata().clone();
        let mail = RemoteMail::new(
            metadata
                .clone()
                .with_keywords(Some(Keywords::of(&["$Junk", "work"]))),
            RemoteContent::from_string("content".to_string()),
        );

        assert_ok!(repo.repo.store_batch(std::slice::from_ref(&mail)).await);

        let stored = assert_ok!(repo.repo.flags(vec![metadata.uid()]).await);
        assert_eq!(stored[&metadata.uid()].1, Keywords::of(&["$Junk", "work"]));
        let recorded = changes.take();
        let [
            MailChange::Downloaded {
                keywords: recorded, ..
            },
        ] = recorded.as_slice()
        else {
            panic!("expected single download, got {recorded:?}");
        };
        assert_eq!(*recorded, Keywords::of(&["$Junk", "work"]));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_apply_remote_updates_records_keyword_changes(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let RepoWithMail { mut repo, mail } = repo_with_mail;
        let changes = record_changes(&mut repo.repo);
        let uid = mail.metadata().uid();
        let with_keywords = |values: &[&str]| {
            mail.metadata()
                .clone()
                .with_keywords(Some(Keywords::of(values)))
        };
        assert_ok!(
            repo.repo
                .apply_remote_updates(&[with_keywords(&["a", "b"])])
                .await
        );
        changes.take();

        let missing = assert_ok!(
            repo.repo
                .apply_remote_updates(&[with_keywords(&["b", "c"])])
                .await
        );

        assert_is_empty!(missing);
        let recorded = changes.take();
        let [
            MailChange::KeywordsChanged {
                uid: changed,
                path,
                added,
                removed,
            },
        ] = recorded.as_slice()
        else {
            panic!("expected single keyword change, got {recorded:?}");
        };
        assert_eq!(*changed, uid);
        assert!(path.exists());
        assert_eq!(*added, Keywords::of(&["c"]));
        assert_eq!(*removed, Keywords::of(&["a"]));
        let stored = assert_ok!(repo.repo.flags(vec![uid]).await);
        assert_eq!(stored[&uid].1, Keywords::of(&["b", "c"]));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_apply_remote_updates_keeps_unreported_keywords(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let RepoWithMail { repo, mail } = repo_with_mail;
        let uid = mail.metadata().uid();
        let reported = mail
            .metadata()
            .clone()
            .with_keywords(Some(Keywords::of(&["a"])));
        assert_ok!(repo.repo.apply_remote_updates(&[reported]).await);

        let unreported = mail.metadata().clone().with_keywords(None);
        assert_ok!(repo.repo.apply_remote_updates(&[unreported]).await);

        let stored = assert_ok!(repo.repo.flags(vec![uid]).await);
        assert_eq!(stored[&uid].1, Keywords::of(&["a"]));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_apply_flag_changes_applies_keyword_changes(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let RepoWithMail { mut repo, mail } = repo_with_mail;
        let uid = mail.metadata().uid();
        let reported = mail
            .metadata()
            .clone()
            .with_keywords(Some(Keywords::of(&["a", "b"])));
        assert_ok!(repo.repo.apply_remote_updates(&[reported]).await);
        let changes = record_changes(&mut repo.repo);

        assert_ok!(
            repo.repo
                .apply_flag_changes(
                    &[uid],
                    &FlagChange::of_keywords(KeywordChange {
                        added: Keywords::of(&["c"]),
                        removed: Keywords::of(&["a"]),
                    }),
                )
                .await
        );

        let stored = assert_ok!(repo.repo.flags(vec![uid]).await);
        assert_eq!(stored[&uid], (Flag::Seen.into(), Keywords::of(&["b", "c"])));
        assert_is_empty!(changes.take());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_records_deletion(#[future] repo_with_mail: RepoWithMail) {
        let RepoWithMail { mut repo, mail } = repo_with_mail;
        let changes = record_changes(&mut repo.repo);
        let uid = mail.metadata().uid();

        assert_ok!(repo.repo.delete_all(std::iter::once(uid)).await);

        let recorded = changes.take();
        let [MailChange::Deleted { uid: deleted, path }] = recorded.as_slice() else {
            panic!("expected single deletion, got {recorded:?}");
        };
        assert_eq!(*deleted, uid);
        assert!(!path.exists());
        assert!(path.starts_with(repo.mail_dir.path().join("cur")));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_of_vanished_mail_records_nothing(#[future] repo_with_mail: RepoWithMail) {
        let RepoWithMail { mut repo, mail } = repo_with_mail;
        let changes = record_changes(&mut repo.repo);
        let cur = repo.mail_dir.path().join("cur");
        for entry in assert_ok!(fs::read_dir(&cur)) {
            assert_ok!(fs::remove_file(assert_ok!(entry).path()));
        }

        assert_ok!(
            repo.repo
                .delete_all(std::iter::once(mail.metadata().uid()))
                .await
        );

        assert_is_empty!(changes.take());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_add_synced_records_upload(#[future] mut repo: TestMaildirRepository) {
        let changes = record_changes(&mut repo.repo);
        let filename = "foo:2,FS";
        let metadata = assert_ok!(NewLocalMailMetadata::from_str(filename));
        let local = repo.mail_dir.path().join("cur").join(filename);
        assert_ok!(fs::write(&local, "1"));
        let uid = Uid::MAX;

        assert_ok!(repo.repo.add_synced_all(vec![(metadata, uid)]).await);

        let recorded = changes.take();
        let [
            MailChange::Uploaded {
                uid: uploaded,
                from,
                to,
                flags,
            },
        ] = recorded.as_slice()
        else {
            panic!("expected single upload, got {recorded:?}");
        };
        assert_eq!(*uploaded, uid);
        assert_eq!(from, &local);
        assert!(to.exists());
        assert_eq!(flags, "FS");
    }

    mod tags {
        use super::*;
        use crate::{
            notmuch::{
                Notmuch, TagConfig, TagMapping,
                test_support::{TestDatabase, set},
            },
            repository::SequenceSet,
        };

        struct TaggedRepo {
            repo: MaildirRepository,
            database: TestDatabase,
            _state_dir: TempDir,
        }

        async fn tagged_repo() -> Option<TaggedRepo> {
            tagged_repo_of(KeywordSource::Flags).await
        }

        async fn tagged_repo_of(source: KeywordSource) -> Option<TaggedRepo> {
            let database = TestDatabase::if_available()?;
            let state_dir = assert_ok!(TempDir::new());
            let mut repo = assert_ok!(
                MaildirRepository::try_init(
                    assert_ok!(UidValidity::try_from(43)),
                    &database.root.path().join("INBOX"),
                    state_dir.path(),
                )
                .await
            );
            assert_ok!(
                repo.complete_initial_sync(assert_ok!(ModSeq::try_from(1)), KeywordSource::Flags)
                    .await
            );
            repo.sync_tags(Some(SyncedTags {
                notmuch: assert_ok!(Notmuch::start(database.location())),
                mapping: Arc::new(TagMapping::new(&TagConfig::default(), source)),
            }));

            Some(TaggedRepo {
                repo,
                database,
                _state_dir: state_dir,
            })
        }

        fn metadata(uid: u32, flags: BitFlags<Flag>, values: &[&str]) -> RemoteMailMetadata {
            RemoteMailMetadata::new(
                assert_ok!(Uid::try_from(uid)),
                flags,
                assert_ok!(ModSeq::try_from(2)),
            )
            .with_keywords(Some(Keywords::of(values)))
        }

        fn mail(uid: u32, values: &[&str]) -> RemoteMail {
            RemoteMail::new(
                metadata(uid, Flag::Seen.into(), values),
                RemoteContent::from_string(format!(
                    "Message-ID: <{uid}@example.com>\nSubject: test\n\nbody\n"
                )),
            )
        }

        async fn path_of(repo: &MaildirRepository, uid: u32) -> PathBuf {
            let entry = assert_some!(assert_ok!(
                repo.state.get_by_id(assert_ok!(Uid::try_from(uid))).await
            ));
            repo.maildir.get_path_of(&entry)
        }

        /// Stores mail `uid` with keywords `values` and returns its path once notmuch has its tags
        async fn store(repo: &MaildirRepository, uid: u32, values: &[&str]) -> PathBuf {
            assert_ok!(repo.store_batch(&[mail(uid, values)]).await);
            assert_ok!(repo.flush_tags().await);
            path_of(repo, uid).await
        }

        /// Makes the stored revision one of another database
        async fn replace_database(repo: &MaildirRepository) {
            let fingerprint = assert_some!(repo.tags.as_ref()).mapping.fingerprint();
            assert_ok!(
                repo.state
                    .set_tags_revision(
                        Revision {
                            uuid: "replaced".to_string(),
                            number: 1,
                        },
                        fingerprint,
                    )
                    .await
            );
        }

        #[compio::test]
        async fn test_stored_mails_get_indexed_with_tags_of_keywords() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };

            let path = store(&repo, 1, &["work", "$Junk"]).await;

            assert_eq!(database.tags(&path), set(&["$Junk", "inbox", "work"]));
            assert_is_empty!(assert_ok!(repo.state.tags_pending(0, usize::MAX).await));
        }

        #[compio::test]
        async fn test_latest_of_repeated_remote_updates_wins() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let path = store(&repo, 1, &["a"]).await;
            let uid = assert_ok!(Uid::try_from(1));

            // e.g. an unsolicited FETCH response during fetching all mails
            assert_ok!(
                repo.apply_remote_updates(&[
                    metadata(1, Flag::Seen.into(), &["b"]),
                    metadata(1, Flag::Seen | Flag::Flagged, &["c"]),
                ])
                .await
            );
            assert_ok!(repo.flush_tags().await);

            let flags = assert_ok!(repo.flags(vec![uid]).await);
            assert_eq!(
                flags.get(&uid),
                Some(&(Flag::Seen | Flag::Flagged, Keywords::of(&["c"])))
            );
            assert_eq!(
                database.tags(&path_of(&repo, 1).await),
                set(&["c", "flagged", "inbox"])
            );
            assert!(!path.exists());
        }

        #[compio::test]
        async fn test_repeated_remote_update_without_keywords_keeps_earlier_ones() {
            let Some(TaggedRepo {
                repo,
                database: _database,
                ..
            }) = tagged_repo().await
            else {
                return;
            };
            store(&repo, 1, &["a"]).await;
            let uid = assert_ok!(Uid::try_from(1));
            let without_keywords = RemoteMailMetadata::new(
                uid,
                Flag::Seen | Flag::Flagged,
                assert_ok!(ModSeq::try_from(3)),
            );

            assert_ok!(
                repo.apply_remote_updates(&[
                    metadata(1, Flag::Seen.into(), &["b"]),
                    without_keywords
                ])
                .await
            );

            let flags = assert_ok!(repo.flags(vec![uid]).await);
            assert_eq!(
                flags.get(&uid),
                Some(&(Flag::Seen | Flag::Flagged, Keywords::of(&["b"])))
            );
        }

        #[compio::test]
        async fn test_remote_keyword_changes_keep_other_tags() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let path = store(&repo, 1, &["a"]).await;
            database.tag(&path, "local");

            assert_ok!(
                repo.apply_remote_updates(&[metadata(1, Flag::Seen.into(), &["b"])])
                    .await
            );
            assert_ok!(repo.flush_tags().await);

            assert_eq!(database.tags(&path), set(&["b", "inbox", "local"]));
        }

        #[compio::test]
        async fn test_tag_changes_get_detected_until_completed() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let path = store(&repo, 1, &["a"]).await;
            let first = assert_ok!(repo.detect_tag_changes().await);
            assert_is_empty!(first.updates.build().iter().collect::<Vec<_>>());
            database.tag(&path, "b");
            database.untag(&path, "a");
            database.tag(&path, "attachment");

            let changes = assert_ok!(repo.detect_tag_changes().await);
            let revision = assert_some!(changes.tags_revision.clone());
            let expected = FlagChange::of_keywords(KeywordChange {
                added: Keywords::of(&["b"]),
                removed: Keywords::of(&["a"]),
            });
            let uid = assert_ok!(Uid::try_from(1));
            assert_eq!(
                changes.updates.build().iter().collect::<Vec<_>>(),
                vec![(&expected, &SequenceSet::from(uid))]
            );
            let repeated = assert_ok!(repo.detect_tag_changes().await);
            assert_eq!(repeated.updates.build().iter().count(), 1);

            assert_ok!(repo.apply_flag_changes(&[uid], &expected).await);
            assert_ok!(repo.flush_tags().await);
            assert_ok!(repo.complete_tag_changes(revision).await);

            let completed = assert_ok!(repo.detect_tag_changes().await);
            assert_is_empty!(completed.updates.build().iter().collect::<Vec<_>>());
            let stored = assert_ok!(repo.flags(vec![uid]).await);
            assert_eq!(stored[&uid].1, Keywords::of(&["b"]));
            assert_eq!(database.tags(&path), set(&["attachment", "b", "inbox"]));
        }

        #[compio::test]
        async fn test_discarded_tag_change_gets_reverted_by_remote_keywords() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let path = store(&repo, 1, &["a"]).await;
            assert_ok!(repo.detect_tag_changes().await);
            database.tag(&path, "b");
            let changes = assert_ok!(repo.detect_tag_changes().await);
            let built = changes.updates.build();
            let (change, set_of_uids) = assert_some!(built.iter().next());
            let (change, uids): (FlagChange, Vec<Uid>) =
                (change.clone(), set_of_uids.iter().collect());

            // like retrying rejected changes, which lets the conflicting remote state win
            assert_ok!(repo.apply_flag_changes(&uids, &change).await);
            assert_ok!(repo.flush_tags().await);
            assert_ok!(
                repo.apply_remote_updates(&[metadata(1, Flag::Flagged.into(), &["a"])])
                    .await
            );
            assert_ok!(repo.flush_tags().await);

            let path = path_of(&repo, 1).await;
            // the remote flags lack \Seen
            assert_eq!(
                database.tags(&path),
                set(&["a", "flagged", "inbox", "unread"])
            );
        }

        #[compio::test]
        async fn test_changed_tag_mapping_replaces_synced_tags() {
            let Some(TaggedRepo {
                mut repo, database, ..
            }) = tagged_repo().await
            else {
                return;
            };
            let path = store(&repo, 1, &["$Junk"]).await;
            assert_ok!(repo.detect_tag_changes().await);
            assert_eq!(database.tags(&path), set(&["$Junk", "inbox"]));
            let notmuch = assert_some!(repo.tags.take()).notmuch;
            repo.sync_tags(Some(SyncedTags {
                notmuch,
                mapping: Arc::new(TagMapping::new(
                    &TagConfig {
                        tags: [("$Junk".to_string(), "spam".to_string())].into(),
                        ..TagConfig::default()
                    },
                    KeywordSource::Flags,
                )),
            }));

            let changes = assert_ok!(repo.detect_tag_changes().await);

            assert_is_empty!(changes.updates.build().iter().collect::<Vec<_>>());
            // the former tag is no synced one anymore, so it stays
            assert_eq!(database.tags(&path), set(&["$Junk", "inbox", "spam"]));
            let uid = assert_ok!(Uid::try_from(1));
            assert_eq!(
                assert_ok!(repo.flags(vec![uid]).await)[&uid].1,
                Keywords::of(&["$Junk"])
            );
        }

        #[compio::test]
        async fn test_replaced_database_keeps_added_tags_and_restores_removed_ones() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let path = store(&repo, 1, &["a"]).await;
            assert_ok!(repo.detect_tag_changes().await);
            database.tag(&path, "b");
            database.untag(&path, "a");
            // like `notmuch compact`, which changes the uuid
            replace_database(&repo).await;

            let changes = assert_ok!(repo.detect_tag_changes().await);

            let expected = FlagChange::of_keywords(KeywordChange {
                added: Keywords::of(&["b"]),
                removed: Keywords::default(),
            });
            let uid = assert_ok!(Uid::try_from(1));
            assert_eq!(
                changes.updates.build().iter().collect::<Vec<_>>(),
                vec![(&expected, &SequenceSet::from(uid))]
            );
            assert_eq!(database.tags(&path), set(&["a", "b", "inbox"]));
        }

        #[compio::test]
        async fn test_recreated_database_gets_server_keywords_without_removing_them() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let path = store(&repo, 1, &["a"]).await;
            assert_ok!(repo.detect_tag_changes().await);
            // like a database recreated, that did not index the mail yet
            let opened = database.open(true);
            assert_ok!(opened.remove_filename(&path));
            assert_ok!(opened.close());
            replace_database(&repo).await;

            let changes = assert_ok!(repo.detect_tag_changes().await);

            assert_is_empty!(changes.updates.build().iter().collect::<Vec<_>>());
            assert_eq!(database.tags(&path), set(&["a", "inbox"]));
        }

        #[compio::test]
        async fn test_rename_to_vanished_file_keeps_message() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let path = store(&repo, 1, &["a"]).await;

            repo.rename_in_notmuch(vec![(path.clone(), path.with_file_name("vanished"))]);
            assert_ok!(repo.flush_tags().await);

            assert_eq!(database.tags(&path), set(&["a", "inbox"]));
        }

        #[compio::test]
        async fn test_renames_of_failed_write_get_written_later() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let from = store(&repo, 1, &["a"]).await;
            let to = from.with_file_name(
                assert_some!(from.file_name())
                    .to_string_lossy()
                    .replace(":2,S", ":2,FS"),
            );
            assert_ok!(std::fs::rename(&from, &to));
            let invalid = PathBuf::from("in\0valid");
            repo.rename_in_notmuch(vec![
                (from.clone(), to.clone()),
                (invalid.clone(), invalid.with_file_name("other")),
            ]);

            assert_err!(repo.flush_tags().await);
            let pending = repo.renames_pending.borrow_mut().take();
            repo.rename_in_notmuch(
                pending
                    .into_iter()
                    .filter(|(from, _)| *from != invalid)
                    .collect(),
            );
            assert_ok!(repo.flush_tags().await);

            assert_eq!(database.filenames(&to), vec![to.clone()]);
        }

        #[compio::test]
        async fn test_renames_get_written_in_batches_until_one_fails() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let mut renamed = Vec::new();
            for uid in 1..=2 {
                let from = store(&repo, uid, &["a"]).await;
                let to = from.with_file_name(
                    assert_some!(from.file_name())
                        .to_string_lossy()
                        .replace(":2,S", ":2,FS"),
                );
                assert_ok!(std::fs::rename(&from, &to));
                renamed.push((from, to));
            }
            let invalid = (PathBuf::from("in\0valid"), PathBuf::from("other"));
            let mut renames = renamed.clone();
            renames.push(invalid.clone());
            repo.rename_in_notmuch(renames);

            assert_err!(repo.flush_tags().await);

            assert_eq!(repo.renames_pending.borrow_mut().take(), vec![invalid]);
            for (_, to) in renamed {
                assert_eq!(database.filenames(&to), vec![to.clone()]);
            }
        }

        #[compio::test]
        async fn test_renames_again_keep_the_name_notmuch_knows() {
            let Some(TaggedRepo { repo, .. }) = tagged_repo().await else {
                return;
            };
            let path = |name: &str| PathBuf::from(format!("/mail/cur/{name}"));
            repo.rename_in_notmuch(vec![(path("a"), path("b")), (path("c"), path("d"))]);

            repo.rename_in_notmuch(vec![(path("b"), path("e")), (path("d"), path("c"))]);

            assert_eq!(repo.pending_tags(), 1);
            assert_eq!(
                repo.renames_pending.borrow_mut().take(),
                vec![(path("a"), path("e"))]
            );
        }

        #[compio::test]
        async fn test_renames_back_and_again_keep_the_name_notmuch_knows() {
            let Some(TaggedRepo { repo, .. }) = tagged_repo().await else {
                return;
            };
            let path = |name: &str| PathBuf::from(format!("/mail/cur/{name}"));
            repo.rename_in_notmuch(vec![(path("a"), path("b"))]);
            repo.rename_in_notmuch(vec![(path("b"), path("a"))]);
            assert_eq!(repo.pending_tags(), 0);

            repo.rename_in_notmuch(vec![(path("a"), path("c"))]);

            assert_eq!(
                repo.renames_pending.borrow_mut().take(),
                vec![(path("a"), path("c"))]
            );
        }

        #[compio::test]
        async fn test_tag_removed_again_while_storing_stays_removed() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let path = store(&repo, 1, &["a"]).await;
            assert_ok!(repo.detect_tag_changes().await);
            database.tag(&path, "b");
            let changes = assert_ok!(repo.detect_tag_changes().await);
            let revision = assert_some!(changes.tags_revision.clone());
            let built = changes.updates.build();
            let (change, uids) = assert_some!(built.iter().next());
            let (change, uids): (FlagChange, Vec<Uid>) = (change.clone(), uids.iter().collect());
            // removed again, while storing the added tag on the server
            database.untag(&path, "b");

            assert_ok!(repo.apply_flag_changes(&uids, &change).await);
            assert_ok!(repo.flush_tags().await);
            assert_ok!(repo.complete_tag_changes(revision).await);
            let next = assert_ok!(repo.detect_tag_changes().await);

            assert_eq!(database.tags(&path), set(&["a", "inbox"]));
            let expected = FlagChange::of_keywords(KeywordChange {
                added: Keywords::default(),
                removed: Keywords::of(&["b"]),
            });
            assert_eq!(
                next.updates.build().iter().collect::<Vec<_>>(),
                vec![(&expected, &SequenceSet::from(uids[0]))]
            );
        }

        #[compio::test]
        async fn test_remote_removal_reaches_mail_renamed_without_notmuch() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let from = store(&repo, 1, &["a"]).await;
            assert_ok!(repo.detect_tag_changes().await);
            // flagged by another program, before notmuch indexed the new file name
            let to = from.with_file_name(
                assert_some!(from.file_name())
                    .to_string_lossy()
                    .replace(":2,S", ":2,FS"),
            );
            assert_ok!(std::fs::rename(&from, &to));
            let uid = assert_ok!(Uid::try_from(1));
            assert_ok!(
                repo.apply_flag_changes(
                    &[uid],
                    &FlagChange::new(Flag::Flagged.into(), BitFlags::empty())
                )
                .await
            );
            assert_ok!(repo.flush_tags().await);

            assert_ok!(
                repo.apply_remote_updates(&[metadata(1, Flag::Seen | Flag::Flagged, &[])])
                    .await
            );
            assert_ok!(repo.flush_tags().await);

            assert!(!database.tags(&to).contains("a"));
            let next = assert_ok!(repo.detect_tag_changes().await);
            assert_is_empty!(next.updates.build().iter().collect::<Vec<_>>());
        }

        async fn complete(repo: &MaildirRepository) -> LocalChanges {
            let changes = assert_ok!(repo.detect_tag_changes().await);
            if let Some(revision) = changes.tags_revision.clone() {
                assert_ok!(repo.complete_tag_changes(revision).await);
                assert_ok!(repo.flush_tags().await);
            }
            changes
        }

        #[compio::test]
        async fn test_new_tags_of_replaced_database_stay_local_until_tagged_again() {
            let Some(TaggedRepo { repo, database, .. }) =
                tagged_repo_of(KeywordSource::GmailLabels).await
            else {
                return;
            };
            let path = store(&repo, 1, &["work"]).await;
            complete(&repo).await;
            // like `notmuch new` of a recreated database, which tags every mail with `inbox`
            database.tag(&path, "inbox");
            replace_database(&repo).await;

            let replaced = complete(&repo).await;

            assert_is_empty!(replaced.updates.build().iter().collect::<Vec<_>>());
            assert_eq!(database.tags(&path), set(&["inbox", "work"]));

            database.untag(&path, "inbox");
            assert_is_empty!(
                complete(&repo)
                    .await
                    .updates
                    .build()
                    .iter()
                    .collect::<Vec<_>>()
            );
            database.tag(&path, "inbox");
            let tagged = complete(&repo).await;

            let expected = FlagChange::of_keywords(KeywordChange {
                added: Keywords::of(&["\\Inbox"]),
                removed: Keywords::default(),
            });
            assert_eq!(
                tagged.updates.build().iter().collect::<Vec<_>>(),
                vec![(&expected, &SequenceSet::from(assert_ok!(Uid::try_from(1))))]
            );
        }

        #[compio::test]
        async fn test_flags_changed_back_before_flushing_keep_message() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let path = store(&repo, 1, &["a"]).await;
            database.tag(&path, "local");

            assert_ok!(
                repo.apply_remote_updates(&[metadata(1, BitFlags::empty(), &["a"])])
                    .await
            );
            assert_ok!(
                repo.apply_remote_updates(&[metadata(1, Flag::Seen.into(), &["a"])])
                    .await
            );
            assert_ok!(repo.flush_tags().await);

            assert_eq!(database.tags(&path), set(&["a", "inbox", "local"]));
        }

        #[compio::test]
        async fn test_mail_deleted_before_flushing_its_rename_gets_removed() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let from = store(&repo, 1, &["a"]).await;
            assert_ok!(
                repo.apply_remote_updates(&[metadata(1, Flag::Seen | Flag::Flagged, &["a"])])
                    .await
            );
            let to = path_of(&repo, 1).await;

            assert_ok!(
                repo.delete_all([assert_ok!(Uid::try_from(1))].into_iter())
                    .await
            );
            assert_ok!(repo.detect_tag_changes().await);

            assert!(!database.is_indexed(&from));
            assert!(!database.is_indexed(&to));
        }

        #[compio::test]
        async fn test_removals_of_last_files_get_remembered_until_done() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let path = store(&repo, 1, &["a"]).await;

            assert_ok!(
                repo.delete_all([assert_ok!(Uid::try_from(1))].into_iter())
                    .await
            );

            assert!(database.is_indexed(&path));
            assert_eq!(
                assert_ok!(repo.state.notmuch_removals(i64::MAX).await),
                vec![path.clone()]
            );
            assert_ok!(repo.remove_delayed_from_notmuch().await);
            assert!(!database.is_indexed(&path));
            assert_is_empty!(assert_ok!(repo.state.notmuch_removals(i64::MAX).await));
        }

        #[compio::test]
        async fn test_unindexable_mail_stops_being_pending() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let unindexable = RemoteMail::new(
                metadata(1, Flag::Seen.into(), &["a"]),
                RemoteContent::from_string("\u{0}\u{1}\u{2}".to_string()),
            );

            assert_ok!(repo.store_batch(&[unindexable]).await);
            assert_ok!(repo.flush_tags().await);

            assert!(!database.is_indexed(&path_of(&repo, 1).await));
            assert_is_empty!(assert_ok!(repo.state.tags_pending(0, usize::MAX).await));
        }

        #[compio::test]
        async fn test_renamed_and_deleted_mails_update_notmuch() {
            let Some(TaggedRepo { repo, database, .. }) = tagged_repo().await else {
                return;
            };
            let from = store(&repo, 1, &["a"]).await;

            assert_ok!(
                repo.apply_remote_updates(&[metadata(1, Flag::Seen | Flag::Flagged, &["a"])])
                    .await
            );
            assert_ok!(repo.flush_tags().await);

            let to = path_of(&repo, 1).await;
            assert_ne!(from, to);
            assert!(!database.is_indexed(&from));
            assert_eq!(database.tags(&to), set(&["a", "flagged", "inbox"]));

            assert_ok!(
                repo.delete_all([assert_ok!(Uid::try_from(1))].into_iter())
                    .await
            );
            // the last file of a message gets removed later
            assert!(database.is_indexed(&to));
            assert_ok!(repo.detect_tag_changes().await);

            assert!(!database.is_indexed(&to));
        }
    }
}
