use crossfire::{
    AsyncRx,
    spsc::{self, Array},
};
use enumflags2::BitFlags;
use futures::{StreamExt as _, TryStreamExt as _, stream};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    time::Duration,
};
use thiserror::Error;

use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    maildir::{
        FlagChange, LocalChanges, LocalFlagChangesBuilder, LocalMail, LocalMailMetadata,
        NewLocalMailMetadata, OtherFlags,
        maildir::{self, Change, MaildirFile},
        state::{self, State},
    },
    repository::{Flag, ModSeq, Uid, UidValidity},
};

use super::Maildir;

const MAX_CONCURRENT_FILE_OPS: usize = 64;

#[derive(Debug)]
pub struct MaildirRepository {
    maildir: Maildir,
    state: State,
}

impl MaildirRepository {
    fn new(maildir: Maildir, state: State) -> Self {
        Self { maildir, state }
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
            FlagChange {
                added: new.additional_flags_compared_to(old),
                removed: new.removed_flags_compared_to(old),
            },
        );
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

    pub async fn complete_initial_sync(&self, highest_modseq: ModSeq) -> Result<(), state::Error> {
        self.state.complete_initial_sync(highest_modseq).await
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

    pub async fn flags(
        &self,
        uids: Vec<Uid>,
    ) -> Result<HashMap<Uid, BitFlags<Flag>>, state::Error> {
        Ok(self
            .state
            .get_all_by_id(uids)
            .await?
            .into_iter()
            .map(|entry| (entry.uid(), entry.flags()))
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

        let (stored, conflicts) = self.state.store_all(stored).await?;
        stream::iter(&stored)
            .map(|mail| self.maildir.move_from_tmp(mail))
            .buffered(MAX_CONCURRENT_FILE_OPS)
            .try_collect::<Vec<()>>()
            .await?;
        self.maildir.sync_cur().await?;

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

    pub async fn update_flags(&self, mail_metadata: &RemoteMailMetadata) -> Result<(), Error> {
        let uid = mail_metadata.uid();

        if let Some(mut entry) = self.state.get_by_id(uid).await? {
            log::info!(
                "update flags of mail {uid}: {} -> {}",
                entry.flags(),
                mail_metadata.flags()
            );
            if entry.flags() != mail_metadata.flags() {
                let new_flags = mail_metadata.flags();
                self.handle_flags(&mut entry, new_flags).await?;
            }

            Ok(())
        } else {
            Err(Error::NoExists { uid })
        }
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

    /// Applies a flag change, that got accepted by the server, to the local mails. Mails, that
    /// vanished locally meanwhile, e.g. due to a concurrent local deletion, are skipped.
    pub async fn apply_flag_changes(&self, uids: &[Uid], change: FlagChange) -> Result<(), Error> {
        let entries = self.state.get_all_by_id(uids.to_vec()).await?;
        if entries.len() != uids.len() {
            let existing: HashSet<Uid> = entries.iter().map(LocalMailMetadata::uid).collect();
            for uid in uids.iter().filter(|uid| !existing.contains(uid)) {
                log::warn!("ignoring flag change of locally missing mail {uid}");
            }
        }
        let changes = entries
            .into_iter()
            .filter_map(|entry| {
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
        for uid in self.set_flags_all(changes).await? {
            log::warn!("ignoring flag change of locally missing mail {uid}");
        }

        Ok(())
    }

    async fn set_flags_all(
        &self,
        changes: Vec<(LocalMailMetadata, BitFlags<Flag>)>,
    ) -> Result<Vec<Uid>, Error> {
        let results: Vec<Result<LocalMailMetadata, Uid>> = stream::iter(changes)
            .map(|(mut entry, new_flags)| async move {
                match self.maildir.update_flags(&mut entry, new_flags).await {
                    Ok(()) => Ok(Ok(entry)),
                    Err(maildir::Error::Missing(_)) => Ok(Err(entry.uid())),
                    Err(e) => Err(e),
                }
            })
            .buffered(MAX_CONCURRENT_FILE_OPS)
            .try_collect()
            .await?;
        let mut updated = Vec::with_capacity(results.len());
        let mut missing = Vec::new();
        for result in results {
            match result {
                Ok(entry) => updated.push(entry),
                Err(uid) => missing.push(uid),
            }
        }
        if !updated.is_empty() {
            self.state.update_all(updated).await?;
        }
        if !missing.is_empty() {
            self.state.delete_all_by_id(missing.clone()).await?;
        }

        Ok(missing)
    }

    async fn handle_flags(
        &self,
        entry: &mut LocalMailMetadata,
        new_flags: BitFlags<Flag>,
    ) -> Result<(), Error> {
        match self.maildir.update_flags(entry, new_flags).await {
            Ok(()) => {
                self.state.update(entry).await?;

                Ok(())
            }
            Err(maildir::Error::Missing(_)) => {
                self.state.delete_by_id(entry.uid()).await?;

                Err(Error::NoExists { uid: entry.uid() })
            }
            Err(e) => Err(e.into()),
        }
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
                if let Ok(renamed) = &result {
                    synced.set_other_flags(renamed.other_flags().clone());
                }

                (synced, result)
            })
            .buffered(MAX_CONCURRENT_FILE_OPS)
            .collect()
            .await;
        let mut error = None;
        let mut stored = Vec::with_capacity(results.len());
        for (synced, result) in results {
            match result {
                Ok(_) => {}
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
        self.state.insert_all(stored).await?;

        error.map_or(Ok(()), |e| Err(e.into()))
    }

    pub async fn delete_all(&self, uids: impl Iterator<Item = Uid>) -> Result<(), DeleteError> {
        let entries = self.state.get_all_by_id(uids.collect()).await?;
        let deleted = stream::iter(entries)
            .map(|entry| async move {
                log::info!("deleting mail {}", entry.uid());
                self.maildir.delete(&entry).await.map(|()| entry.uid())
            })
            .buffered(MAX_CONCURRENT_FILE_OPS)
            .try_collect::<Vec<_>>()
            .await?;
        if !deleted.is_empty() {
            self.state.delete_all_by_id(deleted).await?;
        }

        Ok(())
    }

    pub async fn apply_remote_updates(
        &self,
        updates: &[RemoteMailMetadata],
    ) -> Result<(Vec<Uid>, bool), Error> {
        let mut remote_flags: HashMap<Uid, BitFlags<Flag>> = updates
            .iter()
            .map(|update| (update.uid(), update.flags()))
            .collect();
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
        let change_count = changes.len();
        let mut missing: Vec<Uid> = remote_flags.into_keys().collect();
        let gone = self.set_flags_all(changes).await?;
        let has_changes = change_count > gone.len();
        missing.extend(gone);

        Ok((missing, has_changes))
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
    use std::{fs, os::unix::fs::PermissionsExt, str::FromStr};

    use assertables::*;
    use rstest::*;
    use tempfile::TempDir;

    use compio::{BufResult, io::AsyncReadAtExt as _};

    use crate::imap::RemoteContent;

    use super::*;

    fn added(flag: Flag) -> FlagChange {
        FlagChange {
            added: flag.into(),
            removed: BitFlags::empty(),
        }
    }

    fn removed(flag: Flag) -> FlagChange {
        FlagChange {
            added: BitFlags::empty(),
            removed: flag.into(),
        }
    }

    async fn read_content(repo: &MaildirRepository, metadata: NewLocalMailMetadata) -> Vec<u8> {
        let opened = assert_ok!(repo.open_mail(metadata).await);
        let mail = assert_some!(opened);
        let BufResult(read, content) = mail.file().read_to_end_at(Vec::new(), 0).await;
        assert_ok!(read);
        assert_eq!(content.len(), mail.size());

        content
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
        assert_ok!(repo.complete_initial_sync(highest_modseq).await);
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

        assert_ok!(repo_with_mail.repo.repo.update_flags(&metadata).await);
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

        let result = assert_err!(repo.repo.update_flags(&metadata).await);
        assert_matches!(result, Error::NoExists { .. });
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
                .apply_flag_changes(&[metadata.uid()], added(flag))
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
                .apply_flag_changes(&[metadata.uid()], added(flag))
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
                .apply_flag_changes(&[metadata.uid()], added(Flag::Deleted))
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
                .apply_flag_changes(&[metadata.uid()], added(Flag::Deleted))
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
                .apply_flag_changes(&[metadata.uid()], removed(flag))
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
                .apply_flag_changes(&[metadata.uid()], removed(flag))
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
                .apply_flag_changes(&[metadata.uid()], removed(Flag::Seen))
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

        assert_ok!(repo.apply_flag_changes(&[uid], added(Flag::Flagged)).await);

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
        assert_is_empty!(assert_ok!(repo.repo.state.store_all(vec![metadata.clone()]).await).1);

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
    async fn test_apply_remote_updates_reports_changes(#[future] repo_with_mail: RepoWithMail) {
        let RepoWithMail { repo, mail } = repo_with_mail;
        let unchanged = mail.metadata().clone();
        let changed =
            RemoteMailMetadata::new(unchanged.uid(), Flag::Flagged.into(), unchanged.modseq());

        let (missing, has_changes) = assert_ok!(repo.repo.apply_remote_updates(&[unchanged]).await);
        assert_is_empty!(missing);
        assert!(!has_changes);

        let (missing, has_changes) = assert_ok!(repo.repo.apply_remote_updates(&[changed]).await);
        assert_is_empty!(missing);
        assert!(has_changes);
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
}
