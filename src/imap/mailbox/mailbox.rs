use std::sync::Mutex;

use derive_getters::Getters;

use crate::imap::mailbox::{ModSeq, uid_validity::UidValidity};

#[derive(Debug, Getters)]
pub struct Mailbox {
    #[getter(skip)]
    uid_validity: UidValidity,
    #[getter(skip)]
    highest_modseq: Mutex<ModSeq>,
}

impl Mailbox {
    pub fn uid_validity(&self) -> UidValidity {
        self.uid_validity
    }

    pub fn set_highest_modseq(&self, modseq: ModSeq) {
        let mut lock = self
            .highest_modseq
            .lock()
            .expect("highest_modseq should be unlockable");
        *lock = modseq;
    }
}

#[derive(Default, Debug)]
pub struct MailboxBuilder {
    uid_validity: Option<UidValidity>,
    highest_modseq: Option<ModSeq>,
}

impl MailboxBuilder {
    pub fn build(self) -> Result<Mailbox, &'static str> {
        match (self.uid_validity, self.highest_modseq) {
            (Some(uid_validity), Some(highest_modseq)) => Ok(Mailbox {
                uid_validity,
                highest_modseq: Mutex::new(highest_modseq),
            }),
            _ => Err("not all required fields present"),
        }
    }
    pub fn uid_validity(&mut self, uid_validity: UidValidity) {
        self.uid_validity = Some(uid_validity);
    }

    pub fn highest_modseq(&mut self, highest_modseq: ModSeq) {
        self.highest_modseq = Some(highest_modseq);
    }
}
