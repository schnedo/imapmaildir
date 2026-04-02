use std::{collections::HashMap, io, path::Path};
use thiserror::Error;

use log::{info, trace, warn};
use tokio::sync::mpsc;

use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    maildir::{
        LocalChanges, LocalFlagChangesBuilder, LocalMail, LocalMailMetadata, NewLocalMailMetadata,
        maildir::{self, MaildirError, MaildirFile},
        state::{self, State},
    },
    repository::{MailboxMetadata, ModSeq, Uid, UidValidity},
};

use super::Maildir;

#[derive(Error, Debug)]
#[error("uid {uid} does not exist in state")]
pub struct NoExistsError {
    uid: Uid,
}

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

    pub async fn uid_validity(&self) -> UidValidity {
        self.state
            .uid_validity()
            .await
            .expect("getting uid_validity should succeed")
    }

    pub async fn highest_modseq(&self) -> ModSeq {
        self.state
            .highest_modseq()
            .await
            .expect("getting cached highest_modseq should succeed")
    }

    pub async fn set_highest_modseq(&self, value: ModSeq) {
        self.state
            .set_highest_modseq(value)
            .await
            .expect("setting highest_modseq should succeed");
    }

    pub async fn update_highest_modseq(&self, value: ModSeq) {
        self.state
            .update_highest_modseq(value)
            .await
            .expect("setting highest_modseq should succeed");
    }

    pub async fn store(&self, mail: &RemoteMail) {
        info!(
            "storing mail {} with flags {}",
            mail.metadata().uid(),
            mail.metadata().flags()
        );
        // todo: check if update is necessary
        if self.update_flags(mail.metadata()).await.is_err() {
            let metadata = self
                .maildir
                .store(mail)
                .expect("storing mail in maildir should succeed");
            self.state
                .store(&metadata)
                .await
                .expect("storing data should succeed");
        }
    }

    pub async fn update_flags(
        &self,
        mail_metadata: &RemoteMailMetadata,
    ) -> Result<(), NoExistsError> {
        let uid = mail_metadata.uid();

        if let Some(mut entry) = self
            .state
            .get_by_id(uid)
            .await
            .expect("getting state data by uid should succeed")
        {
            info!(
                "update flags of mail {uid}: {} -> {}",
                entry.flags(),
                mail_metadata.flags()
            );
            if entry.flags() != mail_metadata.flags() {
                let new_flags = mail_metadata.flags();
                match self.maildir.update_flags(&mut entry, new_flags) {
                    Ok(()) => {
                        // todo: update modseq in same step?
                        self.state
                            .update(&entry)
                            .await
                            .expect("updating stored data should succeed");
                        self.state
                            // todo: check highest modseq handling consistent with channel?
                            .update_highest_modseq(mail_metadata.modseq())
                            .await
                            .expect("updating highest_modseq should succeed");
                    }
                    Err(MaildirError::Missing(_)) => {
                        self.state
                            .delete_by_id(uid)
                            .await
                            .expect("deleting by uid should succeed");
                        return Err(NoExistsError { uid });
                    }
                    Err(e) => {
                        todo!("handle error {e:?}")
                    }
                }
            }

            Ok(())
        } else {
            Err(NoExistsError { uid })
        }
    }

    pub async fn add_synced(&self, mail_metadata: NewLocalMailMetadata, uid: Uid) {
        info!(
            "adding {} to newly synced mail {}",
            mail_metadata.uid(),
            mail_metadata.filename()
        );
        let mail_metadata = self
            .maildir
            .update_uid(mail_metadata, uid)
            .expect("updating maildir with newly synced mail should succeed");
        self.state
            .store(&mail_metadata)
            .await
            .expect("storing data should succeed");
    }

    pub async fn delete(&self, uid: Uid) {
        info!("deleting mail {uid}");
        if let Some(entry) = self
            .state
            .get_by_id(uid)
            .await
            .expect("getting state data by uid should succeed")
        {
            self.maildir
                .delete(&entry)
                .expect("deleting mail should succeed");
            self.state
                .delete_by_id(uid)
                .await
                .expect("deleting stored data by uid should succeed");
        } else {
            trace!("mail {uid:?} already gone");
        }
    }

    pub async fn detect_changes(&self) -> LocalChanges {
        let mut news: Vec<LocalMail> = Vec::new();
        let maildir_metadata = self
            .maildir
            .list_cur()
            .expect("cur directory should be readable");

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

        let (all_entries_tx, mut all_entries_rx) = mpsc::channel::<LocalMailMetadata>(32);
        let build_updates_handle = tokio::spawn(async move {
            let mut updates = LocalFlagChangesBuilder::default();
            let mut deletions = Vec::new();
            while let Some(entry) = all_entries_rx.recv().await {
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
            }

            (updates, deletions, maildir_mails)
        });
        let highest_modseq = self
            .state
            .get_all(all_entries_tx)
            .await
            .expect("getting all cached entries");
        let (updates, deletions, maildir_mails) = build_updates_handle
            .await
            .expect("building local updates should succeed");
        for maildata in maildir_mails.into_values() {
            // todo: return Iterator and chain here
            let content = self
                .maildir
                .read_content(&maildata)
                .expect("mail contents should be readable");
            news.push(LocalMail::new(content, maildata.into()));
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
        #[expect(unused)]
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
}
