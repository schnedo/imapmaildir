#![expect(clippy::ref_option)]

use std::fmt::Display;

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
    uid_validity: UidValidity,
    uid_next: Uid,
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

impl ToString for UidValidity {
    fn to_string(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct Uid(u32);

impl From<&u32> for Uid {
    fn from(value: &u32) -> Self {
        Self(*value)
    }
}

impl Display for Uid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

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

impl From<&Uid> for u32 {
    fn from(value: &Uid) -> Self {
        value.0
    }
}
