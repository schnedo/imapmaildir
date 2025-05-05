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

impl MailMetadata {
    pub fn new(uid: Uid, flags: Vec<Flag>) -> Self {
        Self { uid, flags }
    }
}

pub trait Repository {
    fn validity(&self) -> &UidValidity;
    fn list_all(&self) -> impl Stream<Item = MailMetadata>;
}
