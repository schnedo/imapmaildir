#![expect(clippy::ref_option)]

use derive_builder::Builder;
use derive_getters::Getters;

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
}

#[derive(Clone, Debug, PartialEq, Copy)]
pub struct UidValidity(u32);

impl UidValidity {
    pub fn new(validity: u32) -> Self {
        Self(validity)
    }
}

impl From<u32> for UidValidity {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<UidValidity> for u32 {
    fn from(value: UidValidity) -> Self {
        value.0
    }
}

#[derive(Builder, Debug, Clone, Getters)]
//TODO: refactor this
pub struct UidStruct {
    validity: UidValidity,
    next: u32,
}

#[derive(Debug, PartialEq)]
pub struct Uid(u32);

impl From<u32> for Uid {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<Uid> for u32 {
    fn from(value: Uid) -> Self {
        value.0
    }
}
