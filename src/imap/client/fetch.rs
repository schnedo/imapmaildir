use std::borrow::Cow;

use crate::{imap::RemoteMailMetadata, repository::Flag};

#[derive(Debug, Default)]
pub struct FetchAttributes<'a> {
    pub uid: Option<&'a u32>,
    pub modseq: Option<&'a u64>,
    pub flags: Option<&'a Vec<Cow<'a, str>>>,
    pub body: Option<&'a Option<Cow<'a, [u8]>>>,
}

impl<'a> FetchAttributes<'a> {
    pub fn parse(attributes: &'a [imap_proto::AttributeValue<'a>]) -> Self {
        let mut parsed = Self::default();
        for attribute in attributes {
            match attribute {
                imap_proto::AttributeValue::Uid(uid) => parsed.uid = Some(uid),
                imap_proto::AttributeValue::ModSeq(modseq) => parsed.modseq = Some(modseq),
                imap_proto::AttributeValue::Flags(flags) => parsed.flags = Some(flags),
                imap_proto::AttributeValue::BodySection { data, .. } => parsed.body = Some(data),
                _ => log::trace!("ignoring FETCH attribute {attribute:?}"),
            }
        }

        parsed
    }

    pub fn metadata(&self) -> Option<RemoteMailMetadata> {
        let (Some(uid), Some(modseq), Some(flags)) = (self.uid, self.modseq, self.flags) else {
            return None;
        };

        Some(RemoteMailMetadata::new(
            uid.try_into().expect("received uid should be nonzero"),
            Flag::into_bitflags(flags),
            modseq
                .try_into()
                .expect("received modseq should be nonzero"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;

    use super::*;

    fn parse_fetch(raw: &'static [u8]) -> (u32, Vec<imap_proto::AttributeValue<'static>>) {
        let (_, response) = assert_ok!(imap_proto::Response::parse(raw));
        let imap_proto::Response::Fetch(sequence, attributes) = response else {
            panic!("response should be FETCH");
        };
        (sequence, attributes)
    }

    #[test]
    fn test_parse_is_independent_of_attribute_order() {
        let (_, attributes) =
            parse_fetch(b"* 1 FETCH (FLAGS (\\Seen) MODSEQ (7) UID 3 BODY[] {2}\r\nhi)\r\n");

        let parsed = FetchAttributes::parse(&attributes);

        let metadata = assert_some!(parsed.metadata());
        assert_eq!(u32::from(metadata.uid()), 3);
        assert_eq!(metadata.flags(), Flag::Seen);
        assert_eq!(
            parsed.body.and_then(Option::as_deref),
            Some(b"hi".as_slice())
        );
    }

    #[test]
    fn test_metadata_is_none_without_flags() {
        let (_, attributes) = parse_fetch(b"* 1 FETCH (MODSEQ (7) UID 3)\r\n");

        let parsed = FetchAttributes::parse(&attributes);

        assert_none!(parsed.metadata());
        assert_some!(parsed.uid);
    }
}
