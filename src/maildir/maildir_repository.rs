use std::{collections::HashMap, path::Path};
use thiserror::Error;

use log::{info, trace};
use tokio::sync::mpsc;

use crate::{
    imap::{RemoteMail, RemoteMailMetadata},
    maildir::{LocalChanges, LocalFlagChangesBuilder, LocalMailMetadata, state::State},
    repository::{ModSeq, Uid, UidValidity},
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
    pub fn new(maildir: Maildir, state: State) -> Self {
        Self { maildir, state }
    }

    pub fn init(
        uid_validity: UidValidity,
        highest_modseq: ModSeq,
        mail_dir: &Path,
        state_dir: &Path,
    ) -> Self {
        let mail = Maildir::new(mail_dir);
        let state = State::init(state_dir, uid_validity, highest_modseq)
            .expect("initializing state should work");

        Self::new(mail, state)
    }

    pub fn load(mail_dir: &Path, state_dir: &Path) -> Option<Self> {
        match (State::load(state_dir), Maildir::load(mail_dir)) {
            (Ok(state), Ok(mail)) => Some(Self::new(mail, state)),
            (Ok(_), Err(_)) => todo!("missing maildir for existing state"),
            (Err(_), Ok(_)) => todo!("missing state for existing maildir"),
            (Err(_), Err(_)) => None,
        }
    }

    pub async fn uid_validity(&self) -> UidValidity {
        self.state.uid_validity().await
    }

    pub async fn highest_modseq(&self) -> ModSeq {
        self.state.highest_modseq().await
    }

    pub async fn set_highest_modseq(&self, value: ModSeq) {
        self.state.set_highest_modseq(value).await;
    }

    pub async fn store(&self, mail: &RemoteMail) {
        info!("storing mail {}", mail.metadata().uid());
        // todo: check if update is necessary
        if self.update_flags(mail.metadata()).await.is_err() {
            let metadata = self.maildir.store(mail);
            self.state.store(&metadata).await;
        }
    }

    pub async fn update_flags(
        &self,
        mail_metadata: &RemoteMailMetadata,
    ) -> Result<(), NoExistsError> {
        let uid = mail_metadata.uid();
        let res = if let Some(mut entry) = self.state.get_by_id(uid).await {
            info!("update flags of mail {uid}");
            if entry.flags() != mail_metadata.flags() {
                let new_flags = mail_metadata.flags();
                self.maildir.update_flags(&mut entry, new_flags);
                self.state.update(&entry).await;
            }

            Ok(())
        } else {
            Err(NoExistsError { uid })
        };
        self.state
            .update_highest_modseq(mail_metadata.modseq())
            .await;

        res
    }

    pub async fn add_synced(&self, mail_metadata: &mut LocalMailMetadata, new_uid: Uid) {
        info!("adding {new_uid} to newly synced mail");
        self.maildir.update_uid(mail_metadata, new_uid);
        self.state.store(mail_metadata).await;
    }

    pub async fn delete(&self, uid: Uid) {
        info!("deleting mail {uid}");
        if let Some(entry) = self.state.get_by_id(uid).await {
            self.maildir.delete(&entry);
            self.state.delete_by_id(uid).await;
        } else {
            trace!("mail {uid:?} already gone");
        }
    }

    pub async fn detect_changes(&self) -> LocalChanges {
        let mut news = Vec::new();
        let maildir_metadata = self.maildir.list_cur();

        let mut maildir_mails = HashMap::new();

        for metadata in maildir_metadata {
            if let Some(uid) = metadata.uid() {
                maildir_mails.insert(uid, metadata);
            } else {
                news.push(self.maildir.read(metadata));
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
        let highest_modseq = self.state.get_all(all_entries_tx).await;
        let (updates, deletions, maildir_mails) = build_updates_handle
            .await
            .expect("building local updates should succeed");
        for maildata in maildir_mails.into_values() {
            // todo: return Iterator and chain here
            news.push(self.maildir.read(maildata));
        }

        LocalChanges::new(highest_modseq, deletions, news, updates)
    }
}
