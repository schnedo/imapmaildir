use std::num::NonZeroU64;

use derive_builder::Builder;
use derive_getters::Getters;

use crate::imap::mailbox::{uid::Uid, uid_validity::UidValidity};

#[derive(Builder, Debug, Getters)]
pub struct Mailbox {
    name: String,
    #[builder(default)]
    readonly: bool,
    flags: Vec<String>,
    exists: u32,
    recent: u32,
    #[builder(setter(strip_option), default)]
    unseen: Option<u32>,
    #[builder(default)]
    permanent_flags: Vec<String>,
    #[getter(skip)]
    uid_validity: UidValidity,
    #[getter(skip)]
    uid_next: Uid,
    #[getter(skip)]
    highest_modseq: NonZeroU64,
}

impl Mailbox {
    pub fn uid_validity(&self) -> UidValidity {
        self.uid_validity
    }
    pub fn uid_next(&self) -> Uid {
        self.uid_next
    }

    pub fn highest_modseq(&self) -> NonZeroU64 {
        self.highest_modseq
    }
}
