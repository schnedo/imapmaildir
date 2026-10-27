use std::borrow::Cow;

use crate::{
    imap::{RemoteMailMetadata, client::Error},
    repository::{Flag, KeywordSource, Keywords, Uid},
};

#[derive(Debug, Default)]
pub struct FetchAttributes<'a> {
    pub uid: Option<&'a u32>,
    pub modseq: Option<&'a u64>,
    pub flags: Option<&'a Vec<Cow<'a, str>>>,
    pub labels: Option<&'a Vec<Cow<'a, str>>>,
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
                imap_proto::AttributeValue::GmailLabels(labels) => parsed.labels = Some(labels),
                imap_proto::AttributeValue::BodySection { data, .. } => parsed.body = Some(data),
                _ => log::trace!("ignoring FETCH attribute {attribute:?}"),
            }
        }

        parsed
    }

    /// Returns `None` if UID, MODSEQ or FLAGS is missing, i.e. the FETCH carries no full metadata
    /// and may be meant for another handler. Keywords are taken from `source` and unknown if it
    /// is missing. Errors only on invalid values
    pub fn metadata(&self, source: KeywordSource) -> Result<Option<RemoteMailMetadata>, Error> {
        self.metadata_with_uid(self.uid.map(Uid::try_from).transpose()?, source)
    }

    /// Like [`Self::metadata`] but with a UID resolved elsewhere, e.g. for unsolicited FETCH
    /// responses without UID. Returns `None` if `uid`, MODSEQ or FLAGS is missing
    pub fn metadata_with_uid(
        &self,
        uid: Option<Uid>,
        source: KeywordSource,
    ) -> Result<Option<RemoteMailMetadata>, Error> {
        let (Some(uid), Some(modseq), Some(flags)) = (uid, self.modseq, self.flags) else {
            return Ok(None);
        };
        let keywords = match source {
            KeywordSource::Flags => Some(Keywords::from_flags(flags)),
            KeywordSource::GmailLabels => self.labels.map(|labels| Keywords::from_labels(labels)),
        };

        Ok(Some(
            RemoteMailMetadata::new(
                uid,
                Flag::into_bitflags(flags),
                modseq.try_into().map_err(Error::InvalidModSeq)?,
            )
            .with_keywords(keywords),
        ))
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;
    use rstest::*;

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

        let metadata = assert_some!(assert_ok!(parsed.metadata(KeywordSource::Flags)));
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

        assert_none!(assert_ok!(parsed.metadata(KeywordSource::Flags)));
        assert_some!(parsed.uid);
    }

    #[test]
    fn test_metadata_with_uid_resolves_uidless_fetch() {
        let (_, attributes) = parse_fetch(b"* 2 FETCH (FLAGS (\\Seen) MODSEQ (5))\r\n");

        let parsed = FetchAttributes::parse(&attributes);

        assert_none!(assert_ok!(parsed.metadata(KeywordSource::Flags)));
        let uid = assert_ok!(Uid::try_from(7));
        let metadata = assert_some!(assert_ok!(
            parsed.metadata_with_uid(Some(uid), KeywordSource::Flags)
        ));
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.flags(), Flag::Seen);
    }

    #[rstest]
    #[case(
        b"* 1 FETCH (FLAGS (\\Seen $Junk work) MODSEQ (7) UID 3)\r\n",
        KeywordSource::Flags,
        Some(&["$Junk", "work"][..])
    )]
    #[case(
        b"* 1 FETCH (X-GM-LABELS (\\Inbox \"my label\") FLAGS (\\Seen $Junk) MODSEQ (7) UID 3)\r\n",
        KeywordSource::GmailLabels,
        Some(&["\\Inbox", "my label"][..])
    )]
    // labels missing in the response are unknown
    #[case(
        b"* 1 FETCH (FLAGS (\\Seen $Junk) MODSEQ (7) UID 3)\r\n",
        KeywordSource::GmailLabels,
        None
    )]
    fn test_metadata_takes_keywords_from_source(
        #[case] raw: &'static [u8],
        #[case] source: KeywordSource,
        #[case] expected: Option<&[&str]>,
    ) {
        let (_, attributes) = parse_fetch(raw);

        let parsed = FetchAttributes::parse(&attributes);

        let metadata = assert_some!(assert_ok!(parsed.metadata(source)));
        assert_eq!(metadata.flags(), Flag::Seen);
        assert_eq!(metadata.keywords().cloned(), expected.map(Keywords::of));
    }
}
