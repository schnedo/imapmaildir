use crate::imap::mailbox::{ModSeq, uid_validity::UidValidity};

#[derive(Debug)]
pub struct Mailbox {
    uid_validity: UidValidity,
    highest_modseq: ModSeq,
}

impl Mailbox {
    pub fn uid_validity(&self) -> UidValidity {
        self.uid_validity
    }

    pub fn highest_modseq(&self) -> ModSeq {
        self.highest_modseq
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
                highest_modseq,
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
