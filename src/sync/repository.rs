use futures::Stream;

use crate::imap::{Uid, UidValidity};

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
    flags: Vec<Flag>,
}

pub trait Mail: Send {
    fn metadata(&self) -> &MailMetadata;
    fn content(&self) -> &[u8];
}

impl MailMetadata {
    pub fn new(uid: Uid, flags: Vec<Flag>) -> Self {
        Self { uid, flags }
    }

    pub fn uid(&self) -> &Uid {
        &self.uid
    }

    pub fn flags(&self) -> &[Flag] {
        &self.flags
    }
}

pub trait Repository {
    fn validity(&self) -> &UidValidity;
    fn list_all(&mut self) -> impl Stream<Item = MailMetadata>;
    fn get_all(&mut self) -> impl Stream<Item = impl Mail>;
}
