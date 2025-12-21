use bytes::Bytes;
use derive_builder::Builder;
use enumflags2::BitFlags;
use std::fmt::{Debug, Formatter, Result};

use crate::repository::{Flag, ModSeq, Uid};

#[derive(Debug, Clone, Hash, Eq, PartialEq, Builder)]
pub struct RemoteMailMetadata {
    uid: Uid,
    flags: BitFlags<Flag>,
    #[builder(setter(strip_option))]
    modseq: ModSeq,
}

impl RemoteMailMetadata {
    pub fn new(uid: Uid, flags: BitFlags<Flag>, modseq: ModSeq) -> Self {
        Self { uid, flags, modseq }
    }

    pub fn modseq(&self) -> ModSeq {
        self.modseq
    }

    pub fn uid(&self) -> Uid {
        self.uid
    }

    pub fn flags(&self) -> BitFlags<Flag> {
        self.flags
    }
}

pub struct RemoteContent {
    #[expect(dead_code)] // Contains data that `content` borrows
    raw: Bytes,
    content: &'static [u8],
}

impl RemoteContent {
    pub fn new(raw: Bytes, content: &'static [u8]) -> Self {
        Self { raw, content }
    }

    pub fn content(&self) -> &[u8] {
        self.content
    }
}

pub struct RemoteMail {
    metadata: RemoteMailMetadata,
    content: RemoteContent,
}

impl RemoteMail {
    pub fn new(metadata: RemoteMailMetadata, content: RemoteContent) -> Self {
        Self { metadata, content }
    }

    pub fn metadata(&self) -> &RemoteMailMetadata {
        &self.metadata
    }

    pub fn content(&self) -> &[u8] {
        self.content.content()
    }
}

impl Debug for RemoteMail {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("RemoteMail")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}
