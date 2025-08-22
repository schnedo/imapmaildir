use std::path::Path;

use futures::stream::iter;
use log::{debug, trace};

use crate::{
    imap::UidValidity,
    sync::{Mail, MailMetadata, Repository},
};

use super::{Maildir, State};

pub struct MaildirRepository {
    maildir: Maildir,
    state: State,
}

impl MaildirRepository {
    pub fn new(
        account_dir: &Path,
        mailbox: &str,
        state_dir: &Path,
        uid_validity: UidValidity,
    ) -> Self {
        let maildir = account_dir.join(mailbox);
        match (
            Maildir::load(maildir.as_path()),
            State::load(state_dir, mailbox),
        ) {
            (Ok(maildir), Ok(state)) => Self { maildir, state },
            (Ok(_), Err(_)) => todo!("unmanaged maildir found"),
            (Err(_), Ok(_)) => todo!("existing state for new maildir found"),
            (Err(_), Err(_)) => {
                let maildir = Maildir::new(maildir.as_path());
                let state = State::create_new(state_dir, mailbox, uid_validity);
                Self { maildir, state }
            }
        }
    }
}

impl Repository for MaildirRepository {
    fn validity(&self) -> &UidValidity {
        self.state.uid_validity()
    }

    fn list_all(&mut self) -> impl futures::Stream<Item = MailMetadata> {
        iter(self.maildir.list_cur())
    }

    fn get_all(&mut self) -> impl futures::Stream<Item = impl Mail> {
        iter(self.maildir.get_cur())
    }

    fn store(&self, mail: &impl Mail) {
        trace!("storing mail {mail:?}");
        self.maildir.store(mail);
        self.state.store(mail.metadata());
    }
}
