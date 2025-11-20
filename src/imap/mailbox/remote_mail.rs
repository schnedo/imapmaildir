use derive_builder::Builder;
use enumflags2::BitFlags;
use std::fmt::{Debug, Formatter, Result};

use crate::{
    imap::{ModSeq, Uid, codec::ResponseData},
    sync::Flag,
};

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq, Builder)]
pub struct RemoteMailMetadata {
    // todo: is this really optional?
    #[builder(setter(strip_option))]
    uid: Option<Uid>,
    flags: BitFlags<Flag>,
    #[builder(setter(strip_option))]
    modseq: ModSeq,
}

impl RemoteMailMetadata {
    pub fn new(uid: Option<Uid>, flags: BitFlags<Flag>, modseq: ModSeq) -> Self {
        Self { uid, flags, modseq }
    }

    pub fn modseq(&self) -> ModSeq {
        self.modseq
    }

    pub fn uid(&self) -> Option<Uid> {
        self.uid
    }

    pub fn flags(&self) -> BitFlags<Flag> {
        self.flags
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

    pub fn metadata(&self) -> &RemoteMailMetadata {
        &self.metadata
    }

    pub fn content(&self) -> &[u8] {
        self.content
    }
}

impl Debug for RemoteMail {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("RemoteMail")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}
