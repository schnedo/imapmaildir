use std::{collections::HashMap, fmt::Display, fs, path::Path, str::FromStr};
use thiserror::Error;

use enumflags2::BitFlags;
use log::trace;
use tokio::sync::mpsc;

use crate::{
    imap::{ModSeq, RemoteMail, RemoteMailMetadata, Uid, UidValidity},
    maildir::maildir::LocalMail,
    state::State,
    sync::{Change, Flag, Mail, MailMetadata},
};

use super::Maildir;

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
    pub fn new(uid: Option<Uid>, flags: BitFlags<Flag>, fileprefix: String) -> Self {
        Self {
            uid,
            flags,
            fileprefix,
        }
    }

    pub fn fileprefix(&self) -> &str {
        &self.fileprefix
    }

    fn filename(&self) -> String {
        self.to_string()
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

impl MailMetadata for LocalMailMetadata {
    fn uid(&self) -> Option<Uid> {
        self.uid
    }

    fn flags(&self) -> BitFlags<Flag> {
        self.flags
    }

    fn set_flags(&mut self, flags: BitFlags<Flag>) {
        self.flags = flags;
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
        if self.update(mail.metadata()).await.is_ok() {
            None
        } else {
            let filename = self.maildir.store(mail);

            self.state
                .store(LocalMailMetadata::new(
                    mail.metadata().uid(),
                    mail.metadata().flags(),
                    filename,
                ))
                .await
        }
    }

    pub async fn update(&self, mail_metadata: &RemoteMailMetadata) -> Result<(), NoExistsError> {
        let uid = mail_metadata.uid().expect("mail uid should exist here");
        let res = if let Some(mut entry) = self.state.get_by_id(uid).await {
            trace!("updating existing mail with uid {uid:?}");
            if entry.flags() != mail_metadata.flags() {
                let new_flags = mail_metadata.flags();
                self.maildir.update(&entry, new_flags);
                entry.set_flags(new_flags);
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

    pub async fn detect_changes(&self) -> Vec<Change<impl Mail>> {
        let mut changes = vec![];
        let maildir_metadata = self.maildir.list_cur();
        let mut maildir_mails = HashMap::with_capacity(maildir_metadata.size_hint().0);
        for mail_metadata in maildir_metadata {
            maildir_mails.insert(mail_metadata.uid(), mail_metadata);
        }
        self.state
            .for_each(|entry| {
                if let Some(data) = maildir_mails.remove(&entry.uid()) {
                    if data.flags() != entry.flags() {
                        changes.push(Change::Updated(data));
                    }
                } else {
                    changes.push(Change::Deleted());
                }
            })
            .await;
        for maildata in maildir_mails.into_values() {
            changes.push(Change::New(LocalMail::new(
                fs::read(self.maildir.resolve(&maildata.filename()))
                    .expect("mail contents should be readable"),
                maildata,
            )));
        }

        changes
    }
}
