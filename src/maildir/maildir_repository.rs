use enumflags2::BitFlags;
use std::{collections::HashMap, io, path::Path, time::Duration};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    maildir::{
        LocalChanges, LocalFlagChangesBuilder, LocalMail, LocalMailMetadata, NewLocalMailMetadata,
        maildir::{self, Change, MaildirFile},
        state::{self, State},
    },
    repository::{Flag, MailboxMetadata, ModSeq, Uid, UidValidity},
};

use super::Maildir;

// todo: remove Clone and rework untagged response handling
#[derive(Clone, Debug)]
pub struct MaildirRepository {
    maildir: Maildir,
    state: State,
}

impl MaildirRepository {
    fn new(maildir: Maildir, state: State) -> Self {
        Self { maildir, state }
    }

    pub fn try_init(
        mailbox_metadata: &MailboxMetadata,
        mail_dir: &Path,
        state_dir: &Path,
    ) -> Result<Self, InitError> {
        let mail = Maildir::try_init(mail_dir)?;
        let state = State::init(state_dir, mailbox_metadata)?;

        Ok(Self::new(mail, state))
    }

    pub fn load(mail_dir: &Path, state_dir: &Path) -> Result<Self, LoadError> {
        match (State::load(state_dir), Maildir::load(mail_dir)) {
            (Ok(state), Ok(mail)) => {
                let repo = Self::new(mail, state);

                Ok(repo)
            }
            (Err(state::InitError::Missing(_)), Err(maildir::LoadError::Missing(_))) => {
                Err(LoadError::Uninitialized)
            }
            (Ok(_), Err(maildir_error)) => match maildir_error {
                maildir::LoadError::Missing(_) => {
                    log::warn!(
                        "encountered missing maildir for existing state. Reinitilizing state and maildir..."
                    );
                    State::remove_from(state_dir)?;

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
        let additional_flags = new.additional_flags_compared_to(old);
        for flag in additional_flags {
            flag_changes_builder.insert_additional(flag, new.uid());
        }
        let removed_flags = new.removed_flags_compared_to(old);
        for flag in removed_flags {
            flag_changes_builder.insert_removed(flag, new.uid());
        }
    }

    pub async fn watch(self) -> mpsc::Receiver<LocalChanges> {
        log::debug!("listening for maildir mail changes");
        let (changes_tx, changes_rx) = mpsc::channel(1);
        let size = 32;
        let this = self.clone();
        let mut rx = self.maildir.watch(size).await;

        tokio::spawn(async move {
            loop {
                let mut deletions = Vec::new();
                let mut news = Vec::new();
                let mut updates = LocalFlagChangesBuilder::default();
                let mut handle_change = |change| match change {
                    Change::Deletion(entry) => {
                        deletions.push(entry.uid());
                    }
                    Change::New(mail) => {
                        news.push(mail);
                    }
                    Change::Rename { from, to } => {
                        Self::update_flag_changes(&mut updates, &from, &to);
                    }
                };
                let change = rx.recv().await.expect("changes_rx should still be open");
                let highest_modseq = this
                    .highest_modseq()
                    .expect("getting highest modseq should succeed");
                handle_change(change);

                loop {
                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_millis(100)) => {
                        break;
                    }
                        Some(change) = rx.recv() => {
                            handle_change(change);
                        }
                    }
                }
                changes_tx
                    .send(LocalChanges {
                        highest_modseq,
                        updates,
                        deletions,
                        news,
                    })
                    .await
                    .expect("changes_tx should still be open");
            }
        });

        changes_rx
    }

    pub fn uid_validity(&self) -> Result<UidValidity, state::Error> {
        self.state.uid_validity()
    }

    pub fn highest_modseq(&self) -> Result<ModSeq, state::Error> {
        self.state.highest_modseq()
    }

    pub fn set_highest_modseq(&self, value: ModSeq) -> Result<(), state::Error> {
        self.state.set_highest_modseq(value)
    }

    pub fn update_highest_modseq(&self, value: ModSeq) -> Result<(), state::Error> {
        self.state.update_highest_modseq(value)
    }

    pub async fn store(&self, mail: &RemoteMail) -> Result<(), StoreError> {
        let uid = mail.metadata().uid();
        log::info!("storing mail {uid} with flags {}", mail.metadata().flags());
        let metadata = self.maildir.store(mail).await?;
        if let Err(state::Error::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                ..
            },
            _,
        ))) = self.state.store(&metadata)
        {
            if let Some(existing_mail) = self.state.get_by_id(uid)? {
                self.maildir
                    .handle_uid_conflict(&existing_mail, &metadata)
                    .await?;

                Ok(())
            } else {
                unreachable!("stored mail should exist on store ConstraintViolation");
            }
        } else {
            self.state
                // todo: check highest modseq handling consistent with channel?
                .update_highest_modseq(mail.metadata().modseq())?;

            Ok(())
        }
    }

    pub async fn update_flags(&self, mail_metadata: &RemoteMailMetadata) -> Result<(), Error> {
        let uid = mail_metadata.uid();

        // todo: should this be transactional?
        if let Some(mut entry) = self.state.get_by_id(uid)? {
            log::info!(
                "update flags of mail {uid}: {} -> {}",
                entry.flags(),
                mail_metadata.flags()
            );
            if entry.flags() != mail_metadata.flags() {
                let new_flags = mail_metadata.flags();
                self.handle_flags(&mut entry, new_flags).await?;
                self.state
                    // todo: check highest modseq handling consistent with channel?
                    .update_highest_modseq(mail_metadata.modseq())?;
            }

            Ok(())
        } else {
            Err(Error::NoExists { uid })
        }
    }

    pub async fn add_flag(&self, uid: Uid, flag: Flag) -> Result<(), Error> {
        if let Some(mut entry) = self.state.get_by_id(uid)? {
            if !entry.has_flag(flag) {
                log::info!("adding flag {flag} to mail {uid}");
                let mut new_flags = entry.flags();
                new_flags.insert(flag);
                self.handle_flags(&mut entry, new_flags).await?;
            }

            Ok(())
        } else {
            Err(Error::NoExists { uid })
        }
    }

    pub async fn remove_flag(&self, uid: Uid, flag: Flag) -> Result<(), Error> {
        if let Some(mut entry) = self.state.get_by_id(uid)? {
            if entry.has_flag(flag) {
                log::info!("removing flag {flag} of mail {uid}");
                let mut new_flags = entry.flags();
                new_flags.remove(flag);
                self.handle_flags(&mut entry, new_flags).await?;
            }

            Ok(())
        } else {
            Err(Error::NoExists { uid })
        }
    }

    async fn handle_flags(
        &self,
        entry: &mut LocalMailMetadata,
        new_flags: BitFlags<Flag>,
    ) -> Result<(), Error> {
        match self.maildir.update_flags(entry, new_flags).await {
            Ok(()) => {
                self.state.update(entry)?;

                Ok(())
            }
            Err(maildir::Error::Missing(_)) => {
                self.state.delete_by_id(entry.uid())?;

                Err(Error::NoExists { uid: entry.uid() })
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn add_synced(
        &self,
        mail_metadata: NewLocalMailMetadata,
        uid: Uid,
    ) -> Result<(), Error> {
        log::info!(
            "adding {uid} to newly synced mail {}",
            mail_metadata.filename()
        );
        let mail_metadata = self.maildir.update_uid(mail_metadata, uid).await?;
        self.state.store(&mail_metadata)?;

        Ok(())
    }

    pub async fn delete(&self, uid: Uid) -> Result<(), DeleteError> {
        log::info!("deleting mail {uid}");
        if let Some(entry) = self.state.get_by_id(uid)? {
            self.maildir.delete(&entry).await?;
            self.state.delete_by_id(uid)?;
        } else {
            log::trace!("mail {uid:?} already gone");
        }

        Ok(())
    }

    pub async fn detect_changes(&self) -> Result<LocalChanges, DetectChangesError> {
        let mut news: Vec<LocalMail> = Vec::new();
        let mut maildir_metadata = self.maildir.list_cur()?;

        // todo: use Set instead of Map
        let mut maildir_mails = HashMap::new();

        while let Some(metadata) = maildir_metadata.recv().await {
            let metadata = metadata?;
            match metadata {
                maildir::MaildirEntry::New(new_local_mail_metadata) => {
                    let content = self.maildir.read_content(&new_local_mail_metadata)?;
                    news.push(LocalMail::new(content, new_local_mail_metadata));
                }
                maildir::MaildirEntry::MaybeTracked(local_mail_metadata) => {
                    maildir_mails.insert(local_mail_metadata.uid(), local_mail_metadata);
                }
            }
        }
        let mut updates = LocalFlagChangesBuilder::default();
        let mut deletions = Vec::new();

        let highest_modseq = self.state.fore_each(|entry| {
            let uid = entry.uid();
            if let Some(data) = maildir_mails.remove(&uid) {
                Self::update_flag_changes(&mut updates, &entry, &data);
            } else {
                deletions.push(entry.uid());
            }
        })?;
        for maildata in maildir_mails.into_values() {
            let maildata = self.maildir.remove_uid(maildata).await?;
            // todo: return Iterator and chain here
            let content = self.maildir.read_content(&maildata)?;
            news.push(LocalMail::new(content, maildata));
        }

        let changes = LocalChanges::new(highest_modseq, deletions, news, updates);
        log::trace!("{changes:?}");

        Ok(changes)
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

    use crate::{imap::RemoteContent, repository::MailboxMetadataBuilder};

    use super::*;

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

    #[fixture]
    fn mailbox_metadata(uid_validity: UidValidity, highest_modseq: ModSeq) -> MailboxMetadata {
        let mut builder = MailboxMetadataBuilder::default();
        builder.uid_validity(uid_validity);
        builder.highest_modseq(highest_modseq);

        assert_ok!(builder.build())
    }

    struct TestMaildirRepository {
        repo: MaildirRepository,
        mail_dir: TempDir,
        state_dir: TempDir,
    }

    #[fixture]
    fn repo(
        mailbox_metadata: MailboxMetadata,
        mail_dir: TempDir,
        state_dir: TempDir,
    ) -> TestMaildirRepository {
        TestMaildirRepository {
            repo: assert_ok!(MaildirRepository::try_init(
                &mailbox_metadata,
                mail_dir.path(),
                state_dir.path()
            )),
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
    async fn repo_with_mail(repo: TestMaildirRepository, mail: RemoteMail) -> RepoWithMail {
        assert_ok!(repo.repo.store(&mail).await);

        RepoWithMail { repo, mail }
    }

    #[rstest]
    fn test_init_works_in_empty_dirs(
        mailbox_metadata: MailboxMetadata,
        mail_dir: TempDir,
        state_dir: TempDir,
    ) {
        assert_ok!(MaildirRepository::try_init(
            &mailbox_metadata,
            mail_dir.path(),
            state_dir.path(),
        ));
        let mut readdir = assert_ok!(mail_dir.path().read_dir());
        assert_ok!(assert_some!(readdir.next()));
        let mut readdir = assert_ok!(state_dir.path().read_dir());
        assert_ok!(assert_some!(readdir.next()));
    }

    #[rstest]
    fn test_init_propagates_maildir_error(
        mailbox_metadata: MailboxMetadata,
        mail_dir: TempDir,
        state_dir: TempDir,
    ) {
        let mut permissions = assert_ok!(mail_dir.path().metadata()).permissions();
        permissions.set_readonly(true);
        assert_ok!(fs::set_permissions(mail_dir.path(), permissions));

        let result =
            MaildirRepository::try_init(&mailbox_metadata, mail_dir.path(), state_dir.path());
        let result = assert_err!(result);
        assert_matches!(result, InitError::Maildir(_));
    }

    #[rstest]
    fn test_init_propagates_state_error(
        mailbox_metadata: MailboxMetadata,
        mail_dir: TempDir,
        state_dir: TempDir,
    ) {
        let mut permissions = assert_ok!(state_dir.path().metadata()).permissions();
        permissions.set_readonly(true);
        assert_ok!(fs::set_permissions(state_dir.path(), permissions));

        let result =
            MaildirRepository::try_init(&mailbox_metadata, mail_dir.path(), state_dir.path());
        let result = assert_err!(result);
        assert_matches!(result, InitError::State(_));
    }

    #[rstest]
    fn test_load_loads_repo_on_existing_repository(repo: TestMaildirRepository) {
        assert_ok!(MaildirRepository::load(
            repo.mail_dir.path(),
            repo.state_dir.path()
        ));
    }

    #[rstest]
    fn test_load_errors_with_uninitialized_on_empty_dirs(mail_dir: TempDir, state_dir: TempDir) {
        let result = assert_err!(MaildirRepository::load(mail_dir.path(), state_dir.path()));

        assert_matches!(result, LoadError::Uninitialized);
    }

    #[rstest]
    fn test_load_removes_existing_state_on_uninitialized_maildir(repo: TestMaildirRepository) {
        assert_ok!(fs::remove_dir_all(repo.mail_dir.path()));
        assert!(assert_ok!(repo.state_dir.path().try_exists()));

        let result = assert_err!(MaildirRepository::load(
            repo.mail_dir.path(),
            repo.state_dir.path()
        ));

        assert_matches!(result, LoadError::Uninitialized);
    }

    #[rstest]
    fn test_load_propagates_maildir_error(repo: TestMaildirRepository) {
        let mut permissions = assert_ok!(repo.mail_dir.path().metadata()).permissions();
        permissions.set_mode(0o200);
        assert_ok!(fs::set_permissions(repo.mail_dir.path(), permissions));

        let result = assert_err!(MaildirRepository::load(
            repo.mail_dir.path(),
            repo.state_dir.path()
        ));

        assert_matches!(result, LoadError::Maildir(_));
    }

    #[rstest]
    fn test_load_propagates_state_error(repo: TestMaildirRepository) {
        let mut permissions = assert_ok!(repo.state_dir.path().metadata()).permissions();
        permissions.set_mode(0o200);
        assert_ok!(fs::set_permissions(repo.state_dir.path(), permissions));

        let result = assert_err!(MaildirRepository::load(
            repo.mail_dir.path(),
            repo.state_dir.path()
        ));

        assert_matches!(result, LoadError::State(_));
    }

    #[rstest]
    #[tokio::test]
    async fn test_uid_validity_returns_uid_validity(
        repo: TestMaildirRepository,
        uid_validity: UidValidity,
    ) {
        let result = assert_ok!(repo.repo.uid_validity());

        assert_eq!(result, uid_validity);
    }

    #[rstest]
    #[tokio::test]
    async fn test_highest_modseq_returns_highest_modseq(
        repo: TestMaildirRepository,
        highest_modseq: ModSeq,
    ) {
        let result = assert_ok!(repo.repo.highest_modseq());

        assert_eq!(result, highest_modseq);
    }

    #[rstest]
    #[tokio::test]
    async fn test_set_highest_modseq_sets_highes_modseq(repo: TestMaildirRepository) {
        let repo = repo.repo;
        let modseq = assert_ok!(ModSeq::try_from(1));
        assert_lt!(modseq, assert_ok!(repo.highest_modseq()));

        assert_ok!(repo.set_highest_modseq(modseq));
        assert_eq!(modseq, assert_ok!(repo.highest_modseq()));
    }

    #[rstest]
    #[tokio::test]
    async fn test_update_highest_modseq_updates_highest_modseq(repo: TestMaildirRepository) {
        let repo = repo.repo;
        let modseq = assert_ok!(ModSeq::try_from(999));
        assert_gt!(modseq, assert_ok!(repo.highest_modseq()));

        assert_ok!(repo.update_highest_modseq(modseq));
        assert_eq!(modseq, assert_ok!(repo.highest_modseq()));
    }

    #[rstest]
    #[tokio::test]
    async fn test_store_stores_new_mail(repo: TestMaildirRepository, mail: RemoteMail) {
        let uid = mail.metadata().uid();
        assert_none!(assert_ok!(repo.repo.maildir.list_cur()).recv().await);

        assert_ok!(repo.repo.store(&mail).await);

        let mut list_rx = assert_ok!(repo.repo.maildir.list_cur());
        assert_ok!(assert_some!(list_rx.recv().await));
        assert_none!(list_rx.recv().await);

        assert_some!(assert_ok!(repo.repo.state.get_by_id(uid)));
    }

    #[rstest]
    #[tokio::test]
    #[awt]
    async fn test_store_does_not_store_new_mail_if_mail_with_same_uid_already_present(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let RepoWithMail { repo, mail, .. } = repo_with_mail;
        let uid = mail.metadata().uid();
        let mut list_rx = assert_ok!(repo.repo.maildir.list_cur());
        assert_ok!(assert_some!(list_rx.recv().await));
        assert_none!(list_rx.recv().await);
        assert_some!(assert_ok!(repo.repo.state.get_by_id(uid)));

        assert_ok!(repo.repo.store(&mail).await);

        let mut list_rx = assert_ok!(repo.repo.maildir.list_cur());
        assert_ok!(assert_some!(list_rx.recv().await));
        assert_none!(list_rx.recv().await);

        assert_some!(assert_ok!(repo.repo.state.get_by_id(uid)));
    }

    #[rstest]
    #[tokio::test]
    async fn test_store_propagaters_maildir_error(repo: TestMaildirRepository, mail: RemoteMail) {
        assert_ok!(fs::remove_dir_all(repo.mail_dir.path()));

        let result = assert_err!(repo.repo.store(&mail).await);

        assert_matches!(result, StoreError::Maildir(_));
    }

    #[rstest]
    #[tokio::test]
    #[awt]
    async fn test_update_flags_updates_flags(#[future] repo_with_mail: RepoWithMail) {
        let metadata = repo_with_mail.mail.metadata();
        let flags = Flag::Deleted.into();
        assert_ne!(metadata.flags(), flags);
        let metadata = RemoteMailMetadata::new(metadata.uid(), flags, metadata.modseq());

        assert_ok!(repo_with_mail.repo.repo.update_flags(&metadata).await);
    }

    #[rstest]
    #[tokio::test]
    async fn test_update_flags_errors_on_non_existent_mail(
        repo: TestMaildirRepository,
        mail: RemoteMail,
    ) {
        let metadata = mail.metadata();
        let metadata = RemoteMailMetadata::new(metadata.uid(), metadata.flags(), metadata.modseq());

        let result = assert_err!(repo.repo.update_flags(&metadata).await);
        assert_matches!(result, Error::NoExists { .. });
    }

    #[rstest]
    #[tokio::test]
    #[awt]
    async fn test_add_flag_adds_flag(#[future] repo_with_mail: RepoWithMail) {
        let metadata = repo_with_mail.mail.metadata();
        let flag = Flag::Deleted;
        assert_not_contains!(metadata.flags(), flag);

        assert_ok!(
            repo_with_mail
                .repo
                .repo
                .add_flag(metadata.uid(), flag)
                .await
        );
    }

    #[rstest]
    #[tokio::test]
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
                .add_flag(metadata.uid(), flag)
                .await
        );
    }

    #[rstest]
    #[tokio::test]
    async fn test_add_flag_errors_on_non_existent_mail(
        repo: TestMaildirRepository,
        mail: RemoteMail,
    ) {
        let metadata = mail.metadata();

        let result = assert_err!(repo.repo.add_flag(metadata.uid(), Flag::Deleted).await);
        assert_matches!(result, Error::NoExists { .. });
    }

    #[rstest]
    #[tokio::test]
    #[awt]
    async fn test_add_flag_removes_mail_state_if_mail_is_not_in_maildir(
        #[future] repo_with_mail: RepoWithMail,
    ) {
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        assert_ok!(fs::remove_dir_all(&cur));
        assert_ok!(fs::create_dir(&cur));
        let metadata = repo_with_mail.mail.metadata();

        let result = assert_err!(
            repo_with_mail
                .repo
                .repo
                .add_flag(metadata.uid(), Flag::Deleted)
                .await
        );
        assert_matches!(result, Error::NoExists { .. });
        assert_none!(assert_ok!(
            repo_with_mail.repo.repo.state.get_by_id(metadata.uid())
        ));
    }

    #[rstest]
    #[tokio::test]
    #[awt]
    async fn test_remove_flag_removes_flag(#[future] repo_with_mail: RepoWithMail) {
        let metadata = repo_with_mail.mail.metadata();
        let flag = Flag::Seen;
        assert_contains!(metadata.flags(), flag);

        assert_ok!(
            repo_with_mail
                .repo
                .repo
                .remove_flag(metadata.uid(), flag)
                .await
        );
    }

    #[rstest]
    #[tokio::test]
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
                .remove_flag(metadata.uid(), flag)
                .await
        );
    }

    #[rstest]
    #[tokio::test]
    async fn test_remove_flag_errors_on_non_existent_mail(
        repo: TestMaildirRepository,
        mail: RemoteMail,
    ) {
        let metadata = mail.metadata();

        let result = assert_err!(repo.repo.remove_flag(metadata.uid(), Flag::Seen).await);
        assert_matches!(result, Error::NoExists { .. });
    }

    #[rstest]
    #[tokio::test]
    #[awt]
    async fn test_delete_deletes_mail(#[future] repo_with_mail: RepoWithMail) {
        let uid = repo_with_mail.mail.metadata().uid();
        assert_ok!(repo_with_mail.repo.repo.delete(uid).await);
        assert_none!(
            assert_ok!(repo_with_mail.repo.repo.maildir.list_cur())
                .recv()
                .await
        );
        assert_none!(assert_ok!(repo_with_mail.repo.repo.state.get_by_id(uid)));
    }

    #[rstest]
    #[tokio::test]
    async fn test_delete_does_nothing_on_mail_missing_in_state(repo: TestMaildirRepository) {
        assert_ok!(repo.repo.delete(Uid::MAX).await);
    }

    #[rstest]
    #[tokio::test]
    async fn test_add_synced_adds_synced_mail(repo: TestMaildirRepository) {
        let filename = "foo:2,S";
        let metadata = assert_ok!(NewLocalMailMetadata::from_str(filename));
        assert_ok!(fs::write(
            repo.mail_dir.path().join("cur").join(filename),
            "1"
        ));
        let uid = Uid::MAX;

        assert_ok!(repo.repo.add_synced(metadata, uid).await);

        let mail = assert_some!(assert_ok!(repo.repo.state.get_by_id(uid)));
        assert!(
            repo.mail_dir
                .path()
                .join("cur")
                .join(mail.filename())
                .exists()
        );
    }

    #[rstest]
    #[tokio::test]
    #[awt]
    async fn test_detect_changes_has_nothing_on_no_changes(#[future] repo_with_mail: RepoWithMail) {
        let repo = repo_with_mail.repo.repo;

        let result = assert_ok!(repo.detect_changes().await);

        assert_eq!(result.highest_modseq, assert_ok!(repo.highest_modseq()));
        assert_len_eq_x!(result.deletions, 0);
        assert_len_eq_x!(result.news, 0);
        let changes = result.updates.build();
        assert_len_eq_x!(changes.additional_flags().collect::<Vec<_>>(), 0);
        assert_len_eq_x!(changes.removed_flags().collect::<Vec<_>>(), 0);
    }

    #[rstest]
    #[tokio::test]
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
        let mail = assert_some!(result.news.pop());
        let (metadata, content) = mail.unpack();
        assert_eq!(content, expected_content.as_bytes());
        assert_eq!(metadata.filename(), expected_filename);
        assert_eq!(metadata.flags(), BitFlags::from_flag(Flag::Seen));
        assert!(cur.join(expected_filename).exists());
    }

    #[rstest]
    #[tokio::test]
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
        let mail = assert_some!(result.news.pop());
        let (metadata, content) = mail.unpack();
        assert_eq!(content, expected_content.as_bytes());
        assert_eq!(metadata.filename(), expected_filename);
        assert_eq!(metadata.flags(), BitFlags::from_flag(Flag::Seen));
        assert!(cur.join(expected_filename).exists());
    }

    #[rstest]
    #[tokio::test]
    #[awt]
    async fn test_detect_changes_detects_flag_changes(#[future] repo_with_mail: RepoWithMail) {
        let repo = repo_with_mail.repo.repo;
        let uid = repo_with_mail.mail.metadata().uid();
        let metadata = assert_some!(assert_ok!(repo.state.get_by_id(uid)));
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
        let mut removed: Vec<_> = changes.removed_flags().collect();
        assert_len_eq_x!(&removed, 1);
        let (removed_flag, removed_uids) = assert_some!(removed.pop());
        assert_eq!(removed_flag, Flag::Seen);
        let mut removed_uids: Vec<_> = removed_uids.iter().collect();
        assert_len_eq_x!(&removed_uids, 1);
        let removed_uid = assert_some!(removed_uids.pop());
        assert_eq!(removed_uid, uid);
        let mut additional: Vec<_> = changes.additional_flags().collect();
        assert_len_eq_x!(&additional, 1);
        let (additional_flag, additional_uids) = assert_some!(additional.pop());
        assert_eq!(additional_flag, Flag::Deleted);
        let mut additional_uids: Vec<_> = additional_uids.iter().collect();
        assert_len_eq_x!(&additional_uids, 1);
        let additional_uid = assert_some!(additional_uids.pop());
        assert_eq!(additional_uid, uid);
    }

    #[rstest]
    #[tokio::test]
    #[awt]
    async fn test_detect_changes_detects_deleted_mail(#[future] repo_with_mail: RepoWithMail) {
        let repo = repo_with_mail.repo.repo;
        let uid = repo_with_mail.mail.metadata().uid();
        let metadata = assert_some!(assert_ok!(repo.state.get_by_id(uid)));
        let cur = repo_with_mail.repo.mail_dir.path().join("cur");
        assert_ok!(fs::remove_file(cur.join(metadata.filename())));

        let mut result = assert_ok!(repo.detect_changes().await);

        assert_len_eq_x!(&result.deletions, 1);
        let deleted_mail_uid = assert_some!(result.deletions.pop());
        assert_eq!(deleted_mail_uid, uid);
    }
}
