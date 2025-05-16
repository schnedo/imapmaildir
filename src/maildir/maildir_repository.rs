use std::path::Path;

use futures::stream::iter;
use log::debug;

use crate::{
    imap::UidValidity,
    sync::{MailMetadata, Repository},
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
        let maildir = Maildir::new(maildir.as_path());

        let state = if let Ok(state) = State::load(state_dir, mailbox) {
            debug!("existing state file for {mailbox} found");
            if *state.uid_validity() != uid_validity {
                todo!("handle uid_validity change");
            }
            state
        } else {
            assert!(
                maildir.is_empty(),
                "managing maildir with already existing mail is not supported"
            );
            debug!("creating new state file for {mailbox}");
            State::create_new(state_dir, mailbox, uid_validity)
        };

        Self { maildir, state }
    }
}

impl Repository for MaildirRepository {
    fn validity(&self) -> &UidValidity {
        self.state.uid_validity()
    }

    fn list_all(&mut self) -> impl futures::Stream<Item = MailMetadata> {
        iter(self.maildir.list_cur())
    }
}
