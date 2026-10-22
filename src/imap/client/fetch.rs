use std::borrow::Cow;

use crate::{
    imap::{RemoteMailMetadata, client::Error},
    repository::{Flag, Uid},
};

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

    /// Returns `None` if UID, MODSEQ or FLAGS is missing, i.e. the FETCH carries no full metadata
    /// and may be meant for another handler. Errors only on invalid values
    pub fn metadata(&self) -> Result<Option<RemoteMailMetadata>, Error> {
        self.metadata_with_uid(self.uid.map(Uid::try_from).transpose()?)
    }

    /// Like [`Self::metadata`] but with a UID resolved elsewhere, e.g. for unsolicited FETCH
    /// responses without UID. Returns `None` if `uid`, MODSEQ or FLAGS is missing
    pub fn metadata_with_uid(&self, uid: Option<Uid>) -> Result<Option<RemoteMailMetadata>, Error> {
        let (Some(uid), Some(modseq), Some(flags)) = (uid, self.modseq, self.flags) else {
            return Ok(None);
        };

        Ok(Some(RemoteMailMetadata::new(
            uid,
            Flag::into_bitflags(flags),
            modseq.try_into().map_err(Error::InvalidModSeq)?,
        )))
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

        let metadata = assert_some!(assert_ok!(parsed.metadata()));
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

        assert_none!(assert_ok!(parsed.metadata()));
        assert_some!(parsed.uid);
    }

    #[test]
    fn test_metadata_with_uid_resolves_uidless_fetch() {
        let (_, attributes) = parse_fetch(b"* 2 FETCH (FLAGS (\\Seen) MODSEQ (5))\r\n");

        let parsed = FetchAttributes::parse(&attributes);

        assert_none!(assert_ok!(parsed.metadata()));
        let uid = assert_ok!(Uid::try_from(7));
        let metadata = assert_some!(assert_ok!(parsed.metadata_with_uid(Some(uid))));
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.flags(), Flag::Seen);
    }
}
