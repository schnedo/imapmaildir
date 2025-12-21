use std::{collections::HashMap, path::Path};
use thiserror::Error;

use log::trace;
use tokio::sync::mpsc;

use crate::{
    maildir::{
        local_mail::{LocalMail, LocalMailMetadata},
        state::State,
    },
    repository::{
        Flag, ModSeq, RemoteMail, RemoteMailMetadata, SequenceSet, SequenceSetBuilder, Uid,
        UidValidity,
    },
};

use super::Maildir;

#[derive(Debug)]
pub struct LocalFlagChanges {
    additional_flags: HashMap<Flag, SequenceSet>,
    removed_flags: HashMap<Flag, SequenceSet>,
}

impl LocalFlagChanges {
    pub fn additional_flags(&self) -> impl Iterator<Item = (Flag, &SequenceSet)> {
        self.additional_flags.iter().map(|(flag, set)| (*flag, set))
    }

    pub fn removed_flags(&self) -> impl Iterator<Item = (Flag, &SequenceSet)> {
        self.removed_flags.iter().map(|(flag, set)| (*flag, set))
    }
}

#[derive(Debug, Default)]
pub struct LocalFlagChangesBuilder {
    additional_flags: HashMap<Flag, SequenceSetBuilder>,
    removed_flags: HashMap<Flag, SequenceSetBuilder>,
}

impl LocalFlagChangesBuilder {
    pub fn build(mut self) -> LocalFlagChanges {
        LocalFlagChanges {
            additional_flags: self
                .additional_flags
                .drain()
                .map(|(flag, builder)| {
                    (
                        flag,
                        builder.build().expect("sequence set should be buildable"),
                    )
                })
                .collect(),
            removed_flags: self
                .removed_flags
                .drain()
                .map(|(flag, builder)| {
                    (
                        flag,
                        builder.build().expect("sequence set should be buildable"),
                    )
                })
                .collect(),
        }
    }

    fn insert_into(map: &mut HashMap<Flag, SequenceSetBuilder>, flag: Flag, uid: Uid) {
        if let Some(set) = map.get_mut(&flag) {
            set.add(uid);
        } else {
            let mut set = SequenceSetBuilder::default();
            set.add(uid);
            map.insert(flag, set);
        }
    }

    fn insert_additional(&mut self, flag: Flag, uid: Uid) {
        Self::insert_into(&mut self.additional_flags, flag, uid);
    }

    fn insert_removed(&mut self, flag: Flag, uid: Uid) {
        Self::insert_into(&mut self.removed_flags, flag, uid);
    }

    pub fn remove(&mut self, uid: Uid) {
        Self::remove_from(&mut self.additional_flags, uid);
        Self::remove_from(&mut self.removed_flags, uid);
    }

    fn remove_from(map: &mut HashMap<Flag, SequenceSetBuilder>, uid: Uid) {
        for set in map.values_mut() {
            set.remove(uid);
            todo!("more removal")
        }
    }
}

#[derive(Debug)]
pub struct LocalChanges {
    pub highest_modseq: ModSeq,
    pub updates: LocalFlagChangesBuilder,
    pub deletions: Vec<Uid>,
    pub news: Vec<LocalMail>,
}

impl LocalChanges {
    fn new(
        highest_modseq: ModSeq,
        deletions: Vec<Uid>,
        news: Vec<LocalMail>,
        updates: LocalFlagChangesBuilder,
    ) -> Self {
        Self {
            highest_modseq,
            updates,
            deletions,
            news,
        }
    }
}

#[derive(Error, Debug)]
#[error("uid {uid} does not exist in state")]
pub struct NoExistsError {
    uid: Uid,
}

pub struct MaildirRepository {
    maildir: Maildir,
    state: State,
}

impl MaildirRepository {
    pub fn new(maildir: Maildir, state: State) -> Self {
        Self { maildir, state }
    }

    pub fn init(
        account: &str,
        mailbox: &str,
        uid_validity: UidValidity,
        mail_dir: &Path,
        state_dir: &Path,
    ) -> Self {
        let mail = Maildir::new(mail_dir, account, mailbox);
        let state = State::init(state_dir, account, mailbox, uid_validity)
            .expect("initializing state should work");

        Self::new(mail, state)
    }

    pub fn handle_highest_modseq(&self, highest_modseq_rx: mpsc::Receiver<ModSeq>) {
        self.state.handle_highest_modseq(highest_modseq_rx);
    }

    pub fn load(account: &str, mailbox: &str, mail_dir: &Path, state_dir: &Path) -> Option<Self> {
        match (
            State::load(state_dir, account, mailbox),
            Maildir::load(mail_dir, account, mailbox),
        ) {
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
        trace!("storing mail {mail:?}");
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
            trace!("updating existing mail with uid {uid:?}");
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
        self.maildir.update_uid(mail_metadata, new_uid);
        self.state.store(mail_metadata).await;
    }

    pub async fn delete(&self, uid: Uid) {
        if let Some(entry) = self.state.get_by_id(uid).await {
            self.maildir.delete(&entry);
            self.state.delete_by_id(uid).await;
        } else {
            trace!("mail {uid:?} already gone");
        }
    }

    pub async fn detect_changes(&self) -> LocalChanges {
        let mut deletions = Vec::new();
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

        let mut updates = LocalFlagChangesBuilder::default();
        let (all_entries_tx, mut all_entries_rx) = mpsc::channel(32);
        let highest_modseq = self.state.get_all(all_entries_tx).await;
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
        for maildata in maildir_mails.into_values() {
            // todo: return Iterator and chain here
            news.push(self.maildir.read(maildata));
        }

        LocalChanges::new(highest_modseq, deletions, news, updates)
    }
}
