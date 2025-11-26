use rustix::system::uname;
use std::{
    collections::HashMap,
    fmt::Display,
    path::Path,
    process,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

use enumflags2::BitFlags;
use log::trace;
use tokio::sync::mpsc;

use crate::{
    imap::{ModSeq, RemoteMail, RemoteMailMetadata, Uid, UidValidity},
    maildir::maildir::LocalMail,
    state::State,
    sync::Flag,
};

use super::Maildir;

#[derive(Debug, Default)]
pub struct LocalChanges {
    pub updates: Vec<LocalMailMetadata>,
    // todo: use sequence set
    pub deletions: Vec<Uid>,
    pub news: Vec<LocalMail>,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LocalMailMetadata {
    uid: Option<Uid>,
    flags: BitFlags<Flag>,
    fileprefix: String,
}

#[derive(Error, Debug)]
#[error("uid {uid} does not exist in state")]
pub struct NoExistsError {
    uid: Uid,
}

impl LocalMailMetadata {
    pub fn new(uid: Option<Uid>, flags: BitFlags<Flag>, fileprefix: Option<String>) -> Self {
        let fileprefix = fileprefix.unwrap_or_else(Self::generate_file_prefix);

        Self {
            uid,
            flags,
            fileprefix,
        }
    }

    // todo: consider allowing custom prefix/name for user provided mails in maildir
    pub fn fileprefix(&self) -> &str {
        &self.fileprefix
    }

    pub fn filename(&self) -> String {
        self.to_string()
    }

    pub fn uid(&self) -> Option<Uid> {
        self.uid
    }

    pub fn set_uid(&mut self, uid: Uid) {
        self.uid = Some(uid);
    }

    pub fn flags(&self) -> BitFlags<Flag> {
        self.flags
    }

    pub fn set_flags(&mut self, flags: BitFlags<Flag>) {
        self.flags = flags;
    }

    fn generate_file_prefix() -> String {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("should be able to get unix time");
        let secs = time.as_secs();
        let nanos = time.subsec_nanos();
        let hostname = uname();
        let hostname = hostname.nodename().to_string_lossy();
        let pid = process::id();
        format!("{secs}.P{pid}N{nanos}.{hostname}")
    }
}

impl Display for LocalMailMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut string_flags = String::with_capacity(6);
        for flag in self.flags {
            if let Ok(char_flag) = flag.try_into() {
                string_flags.push(char_flag);
            }
        }
        if let Some(uid) = self.uid {
            write!(f, "{},U={uid}:2,{string_flags}", self.fileprefix)
        } else {
            write!(f, "{}:2,{string_flags}", self.fileprefix)
        }
    }
}

impl FromStr for LocalMailMetadata {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (head, flags) = s.rsplit_once(":2,").ok_or("filename should contain :2,")?;
        let flags = flags.chars().map(Flag::from).collect();
        if let Some((fileprefix, uid)) = head.rsplit_once(",U=") {
            let uid = uid
                .parse::<u32>()
                .map_err(|_| "uid field should be u32")?
                .try_into()
                .ok();
            Ok(Self {
                uid,
                flags,
                fileprefix: fileprefix.to_string(),
            })
        } else {
            Ok(Self {
                uid: None,
                flags,
                fileprefix: head.to_string(),
            })
        }
    }
}

pub struct MaildirRepository {
    maildir: Maildir,
    state: State,
}

impl MaildirRepository {
    pub fn new(maildir: Maildir, state: State) -> Self {
        Self { maildir, state }
    }

    pub async fn init(
        account: &str,
        mailbox: &str,
        uid_validity: UidValidity,
        mail_dir: &Path,
        state_dir: &Path,
    ) -> Self {
        let mail = Maildir::new(mail_dir, account, mailbox);
        let state = State::init(state_dir, account, mailbox, uid_validity)
            .await
            .expect("initializing state should work");

        Self::new(mail, state)
    }

    pub fn handle_highest_modseq(&self, highest_modseq_rx: mpsc::Receiver<ModSeq>) {
        self.state.handle_highest_modseq(highest_modseq_rx);
    }

    pub async fn load(
        account: &str,
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
    ) -> Option<Self> {
        match (
            State::load(state_dir, account, mailbox).await,
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

    pub async fn store(&self, mail: &RemoteMail) -> Option<Uid> {
        trace!("storing mail {mail:?}");
        if self.update_flags(mail.metadata()).await.is_ok() {
            None
        } else {
            let metadata = self.maildir.store(mail);
            self.state.store(metadata).await
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
                self.state.update(entry).await;
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

    pub async fn add_synced(&self, mut mail_metadata: LocalMailMetadata, new_uid: Uid) {
        self.maildir.update_uid(&mut mail_metadata, new_uid);
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
        let mut changes = LocalChanges::default();
        let maildir_metadata = self.maildir.list_cur();

        let mut maildir_mails = HashMap::new();

        for metadata in maildir_metadata {
            if let Some(uid) = metadata.uid() {
                maildir_mails.insert(uid, metadata);
            } else {
                changes.news.push(self.maildir.read(metadata));
            }
        }

        self.state
            .for_each(|entry| {
                if let Some(data) = maildir_mails
                    .remove(&entry.uid().expect("all mails in state should have a uid"))
                {
                    if data.flags() != entry.flags() {
                        changes.updates.push(data);
                    }
                } else {
                    changes
                        .deletions
                        .push(entry.uid().expect("uid should exist here"));
                }
            })
            .await;
        for maildata in maildir_mails.into_values() {
            changes.news.push(self.maildir.read(maildata));
        }

        changes
    }
}
