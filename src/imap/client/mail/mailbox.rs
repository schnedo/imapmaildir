#![expect(clippy::ref_option)]

use std::{fmt::Display, num::NonZeroU32};

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
    #[getter(skip)]
    uid_validity: UidValidity,
    #[getter(skip)]
    uid_next: Uid,
}

impl Mailbox {
    pub fn uid_validity(&self) -> UidValidity {
        self.uid_validity
    }
    pub fn uid_next(&self) -> Uid {
        self.uid_next
    }
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

#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Uid(Option<NonZeroU32>);

impl From<&u32> for Uid {
    fn from(value: &u32) -> Self {
        Self(NonZeroU32::new(*value))
    }
}

impl Display for Uid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(nz) = self.0 {
            nz.fmt(f)
        } else {
            0.fmt(f)
        }
    }
}

impl From<u32> for Uid {
    fn from(value: u32) -> Self {
        Self(NonZeroU32::new(value))
    }
}

impl From<Uid> for u32 {
    fn from(value: Uid) -> Self {
        value.0.map_or(0, std::convert::Into::into)
    }
}

impl From<&Uid> for u32 {
    fn from(value: &Uid) -> Self {
        value.0.map_or(0, std::convert::Into::into)
    }
}
