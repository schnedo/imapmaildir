use std::{collections::HashMap, fmt::Display, fs, path::Path, str::FromStr};
use thiserror::Error;

use enumflags2::BitFlags;
use log::trace;

use crate::{
    imap::Uid,
    maildir::maildir::LocalMail,
    state::State,
    sync::{Change, Flag, Mail, MailMetadata},
};

use super::Maildir;

pub struct MaildirRepository {
    maildir: Maildir,
    state: State,
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

impl MaildirRepository {
    pub fn init(account: &str, mailbox: &str, mail_dir: &Path, state: State) -> Self {
        if let Ok(_mail) = Maildir::load(mail_dir, account, mailbox) {
            todo!(
                "unmanaged maildir found: {}/{account}/{mailbox}",
                mail_dir.to_string_lossy()
            )
        } else {
            let mail = Maildir::new(mail_dir, account, mailbox);
            Self {
                maildir: mail,
                state,
            }
        }
    }

    pub fn load(account: &str, mailbox: &str, mail_dir: &Path, state: State) -> Self {
        if let Ok(mail) = Maildir::load(mail_dir, account, mailbox) {
            Self {
                maildir: mail,
                state,
            }
        } else {
            todo!("missing maildir for existing state")
        }
    }

    pub async fn store(&self, mail: &impl Mail) -> Option<Uid> {
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

    pub async fn update(&self, mail_metadata: &impl MailMetadata) -> Result<(), NoExistsError> {
        let uid = mail_metadata.uid().expect("mail uid should exist here");
        if let Some(uid) = mail_metadata.uid()
            && let Some(mut entry) = self.state.get_by_id(uid).await
        {
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
        }
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
