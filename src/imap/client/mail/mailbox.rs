#![expect(clippy::ref_option)]

use std::{
    fmt::Display,
    num::{NonZeroU32, NonZeroU64},
};

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

#[derive(Clone, Debug, PartialEq, Copy)]
pub struct UidValidity(u32);

impl UidValidity {
    pub fn new(validity: u32) -> Self {
        Self(validity)
    }
}

impl Display for UidValidity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
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

#[derive(Debug, PartialEq, Clone, Copy, Eq, Hash)]
#[repr(transparent)]
pub struct Uid(NonZeroU32);

impl TryFrom<&u32> for Uid {
    type Error = <Self as TryFrom<u32>>::Error;

    fn try_from(value: &u32) -> Result<Self, Self::Error> {
        Self::try_from(*value)
    }
}

impl Display for Uid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<u32> for Uid {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Ok(Self(
            NonZeroU32::new(value).ok_or("Cannot convert u32 to nonzero")?,
        ))
    }
}

impl TryFrom<i64> for Uid {
    type Error = &'static str;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if let Ok(num) = value.try_into() {
            NonZeroU32::new(num)
                .ok_or("Cannot convert u32 to nonzero")
                .map(|nz| Self(nz))
        } else {
            Err("i64 too large")
        }
    }
}

impl From<Uid> for u32 {
    fn from(value: Uid) -> Self {
        value.0.into()
    }
}

impl From<&Uid> for u32 {
    fn from(value: &Uid) -> Self {
        value.0.into()
    }
}
