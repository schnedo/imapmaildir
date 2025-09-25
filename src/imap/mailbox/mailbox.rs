use std::{
    num::NonZeroU64,
    ops::{Deref, DerefMut},
    sync::Mutex,
};

use derive_getters::Getters;

use crate::imap::mailbox::{ModSeq, uid::Uid, uid_validity::UidValidity};

#[derive(Debug, Getters)]
pub struct Mailbox {
    name: String,
    readonly: bool,
    flags: Vec<String>,
    exists: u32,
    recent: u32,
    #[getter(skip)]
    unseen: Option<u32>,
    permanent_flags: Vec<String>,
    #[getter(skip)]
    uid_validity: UidValidity,
    #[getter(skip)]
    uid_next: Uid,
    #[getter(skip)]
    highest_modseq: Mutex<ModSeq>,
}

impl Mailbox {
    pub fn uid_validity(&self) -> UidValidity {
        self.uid_validity
    }
    pub fn uid_next(&self) -> Uid {
        self.uid_next
    }

    pub fn highest_modseq(&self) -> ModSeq {
        let modseq = self
            .highest_modseq
            .lock()
            .expect("highest_modseq lock should be acquirable");
        *modseq
    }

    pub fn set_highest_modseq(&self, modseq: ModSeq) {
        let mut lock = self
            .highest_modseq
            .lock()
            .expect("highest_modseq should be unlockable");
        *lock = modseq;
    }

    pub fn unseen(&self) -> Option<u32> {
        self.unseen
    }
}

#[derive(Default, Debug)]
pub struct MailboxBuilder {
    name: Option<String>,
    // #[builder(default)]
    readonly: Option<bool>,
    flags: Option<Vec<String>>,
    exists: Option<u32>,
    recent: Option<u32>,
    // #[builder(setter(strip_option), default)]
    unseen: Option<u32>,
    // #[builder(default)]
    permanent_flags: Vec<String>,
    uid_validity: Option<UidValidity>,
    uid_next: Option<Uid>,
    highest_modseq: Option<ModSeq>,
}

impl MailboxBuilder {
    pub fn build(self) -> Result<Mailbox, &'static str> {
        match (
            self.name,
            self.readonly,
            self.flags,
            self.exists,
            self.recent,
            self.unseen,
            self.permanent_flags,
            self.uid_validity,
            self.uid_next,
            self.highest_modseq,
        ) {
            (
                Some(name),
                readonly,
                Some(flags),
                Some(exists),
                Some(recent),
                unseen,
                permanent_flags,
                Some(uid_validity),
                Some(uid_next),
                Some(highest_modseq),
            ) => Ok(Mailbox {
                name,
                readonly: readonly.unwrap_or(false),
                flags,
                exists,
                recent,
                unseen,
                permanent_flags,
                uid_validity,
                uid_next,
                highest_modseq: Mutex::new(highest_modseq),
            }),
            _ => Err("not all required fields present"),
        }
    }
    pub fn name(&mut self, name: String) {
        self.name = Some(name);
    }

    pub fn readonly(&mut self, readonly: bool) {
        self.readonly = Some(readonly);
    }

    pub fn flags(&mut self, flags: Vec<String>) {
        self.flags = Some(flags);
    }

    pub fn exists(&mut self, exists: u32) {
        self.exists = Some(exists);
    }

    pub fn recent(&mut self, recent: u32) {
        self.recent = Some(recent);
    }

    pub fn unseen(&mut self, unseen: u32) {
        self.unseen = Some(unseen);
    }

    pub fn permanent_flags(&mut self, permanent_flags: Vec<String>) {
        self.permanent_flags = permanent_flags;
    }

    pub fn uid_validity(&mut self, uid_validity: UidValidity) {
        self.uid_validity = Some(uid_validity);
    }

    pub fn uid_next(&mut self, uid_next: Uid) {
        self.uid_next = Some(uid_next);
    }

    pub fn highest_modseq(&mut self, highest_modseq: ModSeq) {
        self.highest_modseq = Some(highest_modseq);
    }

    pub fn get_highest_modseq(&self) -> Option<ModSeq> {
        self.highest_modseq
    }
}
