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

impl From<&u32> for UidValidity {
    fn from(value: &u32) -> Self {
        Self(*value)
    }
}

#[derive(Builder, Debug, Clone, Getters)]
pub struct Uid {
    validity: UidValidity,
    next: u32,
}
