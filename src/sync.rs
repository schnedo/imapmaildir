use std::{io, path::Path};

use log::debug;
use tokio::task::JoinHandle;

use crate::{
    imap::{SendCommand, SequenceSet, Session},
    maildir::{Maildir, State},
};

pub struct Syncer<T: SendCommand> {
    session: Session<T>,
    maildir: Maildir,
}

impl<T: SendCommand> Syncer<T> {
    pub async fn connect(
        mut session: Session<T>,
        maildir: &Path,
        state_dir: &Path,
        mailbox: &str,
    ) -> Self {
        let maildir = maildir.join(mailbox);
        let uid_validity = session
            .select(mailbox)
            .await
            .expect("select should not fail");
        if let Ok(state) = State::load(state_dir, mailbox) {
            debug!("existing state file for {mailbox} found");
            if *state.uid_validity() != uid_validity {
                todo!("handle uid_validity change");
            }
        } else {
            debug!("creating new state file for {mailbox}");
            State::create_new(state_dir, mailbox, uid_validity);
        };
        let maildir = Maildir::new(maildir);

        Self { session, maildir }
    }

    pub async fn fetch_6106(
        &mut self,
    ) -> impl Iterator<Item = JoinHandle<Result<(), io::Error>>> + use<'_, T> {
        let mails = self.session.fetch(&SequenceSet::single(6106)).await;
        mails.into_iter().map(|mail| self.maildir.store_new(mail))
    }
}
