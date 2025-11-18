use derive_builder::Builder;
use enumflags2::BitFlags;
use std::fmt::{Debug, Formatter, Result};

use crate::{
    imap::{ModSeq, Uid, codec::ResponseData},
    sync::{Flag, Mail, MailMetadata},
};

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq, Builder)]
pub struct RemoteMailMetadata {
    // todo: is this really optional?
    #[builder(setter(strip_option))]
    uid: Option<Uid>,
    flags: BitFlags<Flag>,
    // todo: is this really optional?
    #[builder(setter(strip_option))]
    modseq: Option<ModSeq>,
}

impl RemoteMailMetadata {
    pub fn new(uid: Option<Uid>, flags: BitFlags<Flag>, modseq: Option<ModSeq>) -> Self {
        Self { uid, flags, modseq }
    }
}

impl MailMetadata for RemoteMailMetadata {
    fn uid(&self) -> Option<Uid> {
        self.uid
    }

    fn flags(&self) -> BitFlags<Flag> {
        self.flags
    }

    fn set_flags(&mut self, _flags: BitFlags<Flag>) {
        panic!("setting flags on RemoteMailMetadata should not be necessary")
    }
}
pub struct RemoteMail {
    #[expect(dead_code)] // Contains data that `response` borrows
    response: ResponseData,
    metadata: RemoteMailMetadata,
    content: &'static [u8],
}

impl RemoteMail {
    pub fn new(
        response: ResponseData,
        metadata: RemoteMailMetadata,
        content: &'static [u8],
    ) -> Self {
        Self {
            response,
            metadata,
            content,
        }
    }
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
