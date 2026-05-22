use enumflags2::BitFlags;
use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};
use thiserror::Error;

use log::{info, trace, warn};

use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    maildir::{
        LocalChanges, LocalFlagChangesBuilder, LocalMail, LocalMailMetadata, NewLocalMailMetadata,
        maildir::{self, MaildirError, MaildirFile},
        state::{self, State},
    },
    repository::{Flag, MailboxMetadata, ModSeq, Uid, UidValidity},
};

use super::Maildir;

#[derive(Error, Debug)]
#[error("uid {uid} does not exist in state")]
pub struct NoExistsError {
    uid: Uid,
}

// todo: remove Clone and rework untagged response handling
#[derive(Clone, Debug)]
pub struct MaildirRepository {
    maildir: Maildir,
    state: Arc<Mutex<State>>,
}

impl MaildirRepository {
    fn new(maildir: Maildir, state: State) -> Self {
        Self {
            maildir,
            state: Arc::new(Mutex::new(state)),
        }
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
                    warn!(
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

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(db) => db,
            Err(err) => {
                warn!("Encountered poisened mutex to db connection");

                err.into_inner()
            }
        }
    }

    pub fn uid_validity(&self) -> Result<UidValidity, state::Error> {
        let state = self.lock();
        state.uid_validity()
    }

    pub fn highest_modseq(&self) -> Result<ModSeq, state::Error> {
        let state = self.lock();
        state.highest_modseq()
    }

    pub fn set_highest_modseq(&self, value: ModSeq) -> Result<(), state::Error> {
        let state = self.lock();
        state.set_highest_modseq(value)
    }

    pub fn update_highest_modseq(&self, value: ModSeq) -> Result<(), state::Error> {
        let mut state = self.lock();
        state.update_highest_modseq(value)
    }

    pub fn store(&self, mail: &RemoteMail) {
        info!(
            "storing mail {} with flags {}",
            mail.metadata().uid(),
            mail.metadata().flags()
        );
        // todo: check if update is necessary
        if self.update_flags(mail.metadata()).is_err() {
            let metadata = self
                .maildir
                .store(mail)
                .expect("storing mail in maildir should succeed");
            let state = self.lock();
            state.store(&metadata).expect("storing data should succeed");
        }
    }

    pub fn update_flags(&self, mail_metadata: &RemoteMailMetadata) -> Result<(), NoExistsError> {
        let uid = mail_metadata.uid();
        let mut state = self.lock();

        if let Some(mut entry) = state
            .get_by_id(uid)
            .expect("getting state data by uid should succeed")
        {
            info!(
                "update flags of mail {uid}: {} -> {}",
                entry.flags(),
                mail_metadata.flags()
            );
            if entry.flags() != mail_metadata.flags() {
                let new_flags = mail_metadata.flags();
                self.handle_flags(&mut entry, new_flags)
                    .expect("updating flags should succeed");
                state
                    // todo: check highest modseq handling consistent with channel?
                    .update_highest_modseq(mail_metadata.modseq())
                    .expect("updating highest_modseq should succeed");
            }

            Ok(())
        } else {
            Err(NoExistsError { uid })
        }
    }

    pub fn add_flag(&self, uid: Uid, flag: Flag) -> Result<(), NoExistsError> {
        let state = self.lock();
        if let Some(mut entry) = state
            .get_by_id(uid)
            .expect("getting state data by uid should succeed")
        {
            if !entry.has_flag(flag) {
                info!("adding flag {flag} to mail {uid}");
                let mut new_flags = entry.flags();
                new_flags.insert(flag);
                self.handle_flags(&mut entry, new_flags)
                    .expect("updating flags should succeed");
            }

            Ok(())
        } else {
            Err(NoExistsError { uid })
        }
    }

    pub fn remove_flag(&self, uid: Uid, flag: Flag) -> Result<(), NoExistsError> {
        let state = self.lock();
        if let Some(mut entry) = state
            .get_by_id(uid)
            .expect("getting state data by uid should succeed")
        {
            if entry.has_flag(flag) {
                info!("removing flag {flag} of mail {uid}");
                let mut new_flags = entry.flags();
                new_flags.remove(flag);
                self.handle_flags(&mut entry, new_flags)
                    .expect("updating flags should succeed");
            }

            Ok(())
        } else {
            Err(NoExistsError { uid })
        }
    }

    fn handle_flags(
        &self,
        entry: &mut LocalMailMetadata,
        new_flags: BitFlags<Flag>,
    ) -> Result<(), NoExistsError> {
        match self.maildir.update_flags(entry, new_flags) {
            Ok(()) => {
                let state = self.lock();
                state
                    .update(entry)
                    .expect("updating stored data should succeed");

                Ok(())
            }
            Err(MaildirError::Missing(_)) => {
                let state = self.lock();
                state
                    .delete_by_id(entry.uid())
                    .expect("deleting by uid should succeed");

                Err(NoExistsError { uid: entry.uid() })
            }
            Err(e) => {
                todo!("handle error {e:?}")
            }
        }
    }

    pub fn add_synced(&self, mail_metadata: NewLocalMailMetadata, uid: Uid) {
        info!(
            "adding {uid} to newly synced mail {}",
            mail_metadata.filename()
        );
        let mail_metadata = self
            .maildir
            .update_uid(mail_metadata, uid)
            .expect("updating maildir with newly synced mail should succeed");
        let state = self.lock();
        state
            .store(&mail_metadata)
            .expect("storing data should succeed");
    }

    pub fn delete(&self, uid: Uid) {
        info!("deleting mail {uid}");
        let state = self.lock();
        if let Some(entry) = state
            .get_by_id(uid)
            .expect("getting state data by uid should succeed")
        {
            self.maildir
                .delete(&entry)
                .expect("deleting mail should succeed");
            state
                .delete_by_id(uid)
                .expect("deleting stored data by uid should succeed");
        } else {
            trace!("mail {uid:?} already gone");
        }
    }

    pub fn detect_changes(&self) -> LocalChanges {
        let mut news: Vec<LocalMail> = Vec::new();
        let maildir_metadata = self
            .maildir
            .list_cur()
            .expect("cur directory should be readable");

        // todo: use Set instead of Map
        let mut maildir_mails = HashMap::new();

        for metadata in maildir_metadata {
            let metadata = metadata.expect("file in cur should be readable");
            match metadata {
                maildir::MaildirEntry::New(new_local_mail_metadata) => {
                    let content = self
                        .maildir
                        .read_content(&new_local_mail_metadata)
                        .expect("reading mail content should succeed");
                    news.push(LocalMail::new(content, new_local_mail_metadata));
                }
                maildir::MaildirEntry::MaybeTracked(local_mail_metadata) => {
                    maildir_mails.insert(local_mail_metadata.uid(), local_mail_metadata);
                }
            }
        }
        let mut updates = LocalFlagChangesBuilder::default();
        let mut deletions = Vec::new();

        let mut state = self.lock();
        let highest_modseq = state
            .fore_each(|entry| {
                let uid = entry.uid();
                if let Some(data) = maildir_mails.remove(&uid) {
                    let mut additional_flags = data.flags();
                    additional_flags.remove(entry.flags());
                    for flag in additional_flags {
                        updates.insert_additional(flag, uid);
                    }
                    let mut removed_flags = entry.flags();
                    removed_flags.remove(data.flags());
                    for flag in removed_flags {
                        updates.insert_removed(flag, uid);
                    }
                } else {
                    deletions.push(entry.uid());
                }
            })
            .expect("getting all cached entries should succeed");
        for maildata in maildir_mails.into_values() {
            let maildata = self
                .maildir
                .remove_uid(maildata)
                .expect("removing uid of new mail should succeed");
            // todo: return Iterator and chain here
            let content = self
                .maildir
                .read_content(&maildata)
                .expect("mail contents should be readable");
            news.push(LocalMail::new(content, maildata));
        }

        let changes = LocalChanges::new(highest_modseq, deletions, news, updates);
        trace!("{changes:?}");
        changes
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

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use assertables::*;
    use rstest::*;
    use tempfile::TempDir;

    use crate::repository::MailboxMetadataBuilder;

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
    fn test_uid_validity_returns_uid_validity(
        repo: TestMaildirRepository,
        uid_validity: UidValidity,
    ) {
        let result = assert_ok!(repo.repo.uid_validity());

        assert_eq!(result, uid_validity);
    }

    #[rstest]
    fn test_highest_modseq_returns_highest_modseq(
        repo: TestMaildirRepository,
        highest_modseq: ModSeq,
    ) {
        let result = assert_ok!(repo.repo.highest_modseq());

        assert_eq!(result, highest_modseq);
    }

    #[rstest]
    fn test_set_highest_modseq_sets_highes_modseq(repo: TestMaildirRepository) {
        let repo = repo.repo;
        let modseq = assert_ok!(ModSeq::try_from(1));
        assert_lt!(modseq, assert_ok!(repo.highest_modseq()));

        assert_ok!(repo.set_highest_modseq(modseq));
        assert_eq!(modseq, assert_ok!(repo.highest_modseq()));
    }

    #[rstest]
    fn test_update_highest_modseq_updates_highest_modseq(repo: TestMaildirRepository) {
        let repo = repo.repo;
        let modseq = assert_ok!(ModSeq::try_from(999));
        assert_gt!(modseq, assert_ok!(repo.highest_modseq()));

        assert_ok!(repo.update_highest_modseq(modseq));
        assert_eq!(modseq, assert_ok!(repo.highest_modseq()));
    }
}
