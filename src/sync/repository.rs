use std::{borrow::Cow, fmt::Debug, hash::Hash};

use crate::imap::Uid;
use enumflags2::{BitFlags, bitflags};

#[bitflags]
#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum Flag {
    Seen,
    Answered,
    Flagged,
    Deleted,
    Draft,
    Recent,
}

impl Flag {
    pub fn into_bitflags(flags: &Vec<Cow<str>>) -> BitFlags<Flag, u8> {
        flags
            .iter()
            .filter_map(|flag| <&str as TryInto<Flag>>::try_into(flag.as_ref()).ok())
            .collect()
    }
}

pub trait Mail: Send + Debug {
    type Metadata: MailMetadata;

    fn metadata(&self) -> &Self::Metadata;
    fn content(&self) -> &[u8];
}

pub trait MailMetadata: Clone + Eq + Hash {
    fn uid(&self) -> Option<Uid>;
    fn flags(&self) -> BitFlags<Flag>;
    fn set_flags(&mut self, flags: BitFlags<Flag>);
}

pub enum Change<T: Mail> {
    New(T),
    Deleted(),
    Updated(T::Metadata),
}
