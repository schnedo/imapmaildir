use std::{borrow::Cow, fmt::Debug, hash::Hash};

use futures::Stream;

use crate::imap::{Uid, UidValidity};
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
    fn filename(&self) -> String;
}

pub enum Change<T: Mail> {
    New(T),
    Deleted(Uid),
    Updated(T::Metadata),
}

pub trait Repository {
    fn validity(&self) -> UidValidity;
    fn list_all(&self) -> impl Stream<Item = impl MailMetadata>;
    fn get_all(&self) -> impl Stream<Item = impl Mail>;
    fn store(&self, mail: &impl Mail) -> Option<Uid>;
    fn detect_changes(&self) -> Vec<Change<impl Mail>>;
}
