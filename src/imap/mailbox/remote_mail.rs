use enumflags2::BitFlags;
use std::fmt::{Debug, Formatter, Result};

use crate::{
    imap::{Uid, codec::ResponseData},
    sync::{Flag, Mail, MailMetadata},
};

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub struct RemoteMailMetadata {
    uid: Option<Uid>,
    flags: BitFlags<Flag>,
}

impl MailMetadata for RemoteMailMetadata {
    fn uid(&self) -> Option<Uid> {
        self.uid
    }

    fn flags(&self) -> BitFlags<Flag> {
        self.flags
    }

    fn set_flags(&mut self, flags: BitFlags<Flag>) {
        panic!("setting flags on RemoteMailMetadata should not be necessary")
    }

    fn filename(&self) -> String {
        panic!("filename should never be accessed for RemoteMailMetadata")
    }
}
pub struct RemoteMail {
    response: ResponseData,
    metadata: RemoteMailMetadata,
    content: &'static [u8],
}

impl Debug for RemoteMail {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("RemoteMail")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl Mail for RemoteMail {
    type Metadata = RemoteMailMetadata;

    fn metadata(&self) -> &Self::Metadata {
        &self.metadata
    }

    fn content(&self) -> &[u8] {
        self.content
    }
}
