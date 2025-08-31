use std::path::Path;

use enumflags2::BitFlags;
use futures::stream::iter;
use log::{debug, trace};

use crate::{
    imap::{Uid, UidValidity},
    maildir::state::StateEntry,
    sync::{Flag, Mail, MailMetadata, Repository},
};

use super::{Maildir, State};

pub struct MaildirRepository {
    maildir: Maildir,
    state: State,
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub struct LocalMailMetadata {
    uid: Option<Uid>,
    flags: BitFlags<Flag>,
}

impl LocalMailMetadata {
    pub fn new(uid: Option<Uid>, flags: BitFlags<Flag>) -> Self {
        Self { uid, flags }
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
    pub fn new(
        account: &str,
        mailbox: &str,
        mail_dir: &Path,
        state_dir: &Path,
        uid_validity: UidValidity,
    ) -> Self {
        match (
            Maildir::load(mail_dir, account, mailbox),
            State::load(state_dir, account, mailbox),
        ) {
            (Ok(mail), Ok(state)) => Self {
                maildir: mail,
                state,
            },
            (Ok(_), Err(_)) => todo!(
                "unmanaged maildir found: {}/{account}/{mailbox}",
                mail_dir.to_string_lossy()
            ),
            (Err(_), Ok(_)) => todo!(
                "existing state for new maildir found: {}/{account}",
                state_dir.to_string_lossy()
            ),
            (Err(_), Err(_)) => {
                let mail = Maildir::new(mail_dir, account, mailbox);
                let state = State::create_new(state_dir, account, mailbox, uid_validity);
                Self {
                    maildir: mail,
                    state,
                }
            }
        }
    }
}

impl Repository for MaildirRepository {
    fn validity(&self) -> UidValidity {
        self.state.uid_validity()
    }

    fn list_all(&self) -> impl futures::Stream<Item = impl MailMetadata> {
        iter(self.maildir.list_cur())
    }

    fn get_all(&self) -> impl futures::Stream<Item = impl Mail> {
        iter(self.maildir.get_cur())
    }

    fn store(&self, mail: &impl Mail) -> Option<Uid> {
        if let Some(uid) = mail.metadata().uid()
            && let Some(mut entry) = self.state.exists(mail.metadata().uid())
        {
            trace!("handling existing mail {mail:?}");
            if entry.flags() != mail.metadata().flags() {
                trace!("updating mail {mail:?}");
                let new_flags = mail.metadata().flags();
                self.maildir.update(&entry, new_flags);
                entry.set_flags(new_flags);
                self.state.update(&entry);
            }
            None
        } else {
            trace!("storing mail {mail:?}");
            let filename = self.maildir.store(mail);
            self.state.store(&StateEntry::new(
                mail.metadata().uid(),
                mail.metadata().flags(),
                filename,
            ))
        }
    }
}
