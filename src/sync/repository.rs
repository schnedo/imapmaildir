use futures::Stream;

use crate::imap::{Uid, UidValidity};
use enumflags2::{bitflags, BitFlags};

#[bitflags]
#[repr(u8)]
#[derive(Copy, Clone)]
pub enum Flag {
    Seen,
    Answered,
    Flagged,
    Deleted,
    Draft,
    Recent,
}

pub struct MailMetadata {
    uid: Uid,
    flags: BitFlags<Flag>,
}

pub trait Mail: Send {
    fn metadata(&self) -> &MailMetadata;
    fn content(&self) -> &[u8];
}

impl MailMetadata {
    pub fn new(uid: Uid, flags: BitFlags<Flag>) -> Self {
        Self { uid, flags }
    }

    pub fn uid(&self) -> &Uid {
        &self.uid
    }

    pub fn flags(&self) -> &BitFlags<Flag> {
        &self.flags
    }
}

pub trait Repository {
    fn validity(&self) -> &UidValidity;
    fn list_all(&mut self) -> impl Stream<Item = MailMetadata>;
    fn get_all(&mut self) -> impl Stream<Item = impl Mail>;
}
