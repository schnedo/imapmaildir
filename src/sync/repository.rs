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

pub trait Mail<'a> {
    fn metadata(&'a self) -> &'a MailMetadata;
    fn content(&'a self) -> &'a [u8];
}

impl MailMetadata {
    pub fn new(uid: Uid, flags: Vec<Flag>) -> Self {
        Self { uid, flags }
    }
}

pub trait Repository {
    fn validity(&self) -> &UidValidity;
    fn list_all(&mut self) -> impl Stream<Item = MailMetadata>;
    fn get_all(&mut self) -> impl Stream<Item = impl Mail>;
}
