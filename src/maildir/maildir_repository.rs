use std::{collections::HashMap, path::Path};
use thiserror::Error;

use log::{info, trace};
use tokio::sync::mpsc;

use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    maildir::{
        LocalChanges, LocalFlagChangesBuilder, LocalMailMetadata, maildir::MaildirError,
        state::State,
    },
    repository::{MailboxMetadata, ModSeq, Uid, UidValidity},
};

use super::Maildir;

#[derive(Error, Debug)]
#[error("uid {uid} does not exist in state")]
pub struct NoExistsError {
    uid: Uid,
}

#[derive(Clone)]
pub struct MaildirRepository {
    maildir: Maildir,
    state: State,
}

impl MaildirRepository {
    fn new(maildir: Maildir, state: State) -> Self {
        Self { maildir, state }
    }

    pub fn init(mailbox_metadata: &MailboxMetadata, mail_dir: &Path, state_dir: &Path) -> Self {
        let mail = Maildir::try_new(mail_dir).expect("creating maildir should succeed");
        let state =
            State::init(state_dir, mailbox_metadata).expect("initializing state should work");

        Self::new(mail, state)
    }

    pub fn load(mail_dir: &Path, state_dir: &Path) -> Result<Self, ()> {
        match (State::load(state_dir), Maildir::load(mail_dir)) {
            (Ok(state), Ok(mail)) => {
                let repo = Self::new(mail, state);

                Ok(repo)
            }
            (Ok(_), Err(_)) => todo!("missing maildir for existing state"),
            (Err(_), Ok(_)) => todo!("missing state for existing maildir"),
            (Err(_), Err(_)) => Err(()),
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

    pub async fn add_synced(&self, mail_metadata: &mut LocalMailMetadata, new_uid: Uid) {
        info!("adding {new_uid} to newly synced mail");
        self.maildir
            .update_uid(mail_metadata, new_uid)
            .expect("updating maildir with newly synced mail should succeed");
        self.state
            .store(mail_metadata)
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
        let mut news = Vec::new();
        let maildir_metadata = self
            .maildir
            .list_cur()
            .expect("cur directory should be readable");

        let mut maildir_mails = HashMap::new();

        for metadata in maildir_metadata {
            let metadata = metadata.expect("file in cur should be readable");
            if let Some(uid) = metadata.uid() {
                maildir_mails.insert(uid, metadata);
            } else {
                news.push(
                    self.maildir
                        .read(metadata)
                        .expect("mail contents should be readable"),
                );
            }
        }

        let (all_entries_tx, mut all_entries_rx) = mpsc::channel::<LocalMailMetadata>(32);
        let build_updates_handle = tokio::spawn(async move {
            let mut updates = LocalFlagChangesBuilder::default();
            let mut deletions = Vec::new();
            while let Some(entry) = all_entries_rx.recv().await {
                let uid = entry.uid().expect("all mails in state should have a uid");
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
                    deletions.push(entry.uid().expect("uid should exist here"));
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
            news.push(
                self.maildir
                    .read(maildata)
                    .expect("mail contents should be readable"),
            );
        }

        let changes = LocalChanges::new(highest_modseq, deletions, news, updates);
        trace!("{changes:?}");
        changes
    }
}
