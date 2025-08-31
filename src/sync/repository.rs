use std::{fmt::Debug, hash::Hash};

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

pub trait Mail: Send + Debug {
    type Metadata: MailMetadata;

    fn metadata(&self) -> Self::Metadata;
    fn content(&self) -> &[u8];
}

pub trait MailMetadata: Clone + Eq + Hash {
    fn uid(&self) -> Option<Uid>;
    fn flags(&self) -> BitFlags<Flag>;
    fn set_flags(&mut self, flags: BitFlags<Flag>);
}

pub enum Change<T: MailMetadata, U: Mail<Metadata = T>> {
    New(U),
    Deleted(Uid),
    Updated(T),
}

pub trait Repository {
    fn validity(&self) -> UidValidity;
    fn list_all(&self) -> impl Stream<Item = impl MailMetadata>;
    fn get_all(&self) -> impl Stream<Item = impl Mail>;
    fn store(&self, mail: &impl Mail) -> Option<Uid>;
    fn detect_changes<T: MailMetadata, U: Mail<Metadata = T>>(&self) -> Vec<Change<T, U>>;
}
