use bytes::Bytes;
use derive_builder::Builder;
use enumflags2::BitFlags;
use std::fmt::{Debug, Formatter, Result};

use crate::repository::{Flag, ModSeq, Uid};

// todo: check if Arc covers Clone use cases
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

    #[cfg(test)]
    pub fn from_string(value: String) -> Self {
        let raw: Bytes = value.into();
        let content = unsafe {
            use std::mem::transmute;
            transmute::<&[u8], &[u8]>(raw.as_ref())
        };

        Self { raw, content }
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

#[cfg(test)]
mod tests {
    use assertables::*;
    use enumflags2::BitFlag;
    use rstest::*;

    use super::*;

    #[fixture]
    fn uid() -> Uid {
        assert_ok!(Uid::try_from(5))
    }

    #[fixture]
    fn flags() -> BitFlags<Flag> {
        Flag::all()
    }

    #[fixture]
    fn modseq() -> ModSeq {
        assert_ok!(ModSeq::try_from(9))
    }

    #[fixture]
    fn content() -> RemoteContent {
        RemoteContent::from_string("dkdkdjaj".to_string())
    }

    #[fixture]
    fn metadata(uid: Uid, flags: BitFlags<Flag>, modseq: ModSeq) -> RemoteMailMetadata {
        RemoteMailMetadata::new(uid, flags, modseq)
    }

    #[fixture]
    fn mail(metadata: RemoteMailMetadata, content: RemoteContent) -> RemoteMail {
        RemoteMail::new(metadata, content)
    }

    #[rstest]
    fn test_remote_mail_debug_serialization_skips_content(
        mail: RemoteMail,
        content: RemoteContent,
    ) {
        assert_not_contains!(&format!("{mail:?}"), &format!("{:?}", content.content()));
    }

    #[rstest]
    fn test_metadata_is_consistent(
        metadata: RemoteMailMetadata,
        uid: Uid,
        flags: BitFlags<Flag>,
        modseq: ModSeq,
    ) {
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.flags(), flags);
        assert_eq!(metadata.modseq(), modseq);
    }

    #[rstest]
    fn test_mail_is_consistent(
        mail: RemoteMail,
        content: RemoteContent,
        metadata: RemoteMailMetadata,
    ) {
        assert_eq!(mail.metadata(), &metadata);
        assert_eq!(mail.content(), content.content());
    }
}
