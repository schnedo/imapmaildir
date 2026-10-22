use std::{
    collections::HashSet,
    fmt::{Display, Formatter, Result, Write as _},
    ops::RangeInclusive,
};
use thiserror::Error;

use crate::repository::{Uid, uid::UidRangeInclusiveIterator};

#[derive(Debug, PartialEq, Clone)]
pub struct SequenceRange {
    start: Uid,
    end: Option<Uid>,
}

impl SequenceRange {
    fn single(uid: Uid) -> Self {
        Self {
            start: uid,
            end: None,
        }
    }
    fn range(start: Uid, end: Uid) -> Self {
        debug_assert!(start < end);
        Self {
            start,
            end: Some(end),
        }
    }
    fn iter(&self) -> UidRangeInclusiveIterator {
        let to = self.end.unwrap_or(self.start);

        self.start.range_inclusive(to)
    }
    fn end(&self) -> Uid {
        self.end.unwrap_or(self.start)
    }
}

impl IntoIterator for SequenceRange {
    type Item = Uid;

    type IntoIter = UidRangeInclusiveIterator;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Display for SequenceRange {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        if let Some(to) = self.end {
            write!(f, "{}:{}", self.start, to)
        } else {
            write!(f, "{}", self.start)
        }
    }
}

impl TryFrom<&RangeInclusive<u32>> for SequenceRange {
    type Error = <Uid as TryFrom<u32>>::Error;

    /// Normalizes reversed ranges, as servers may send e.g. `4:2` for `2:4` (RFC 3501 9)
    fn try_from(value: &RangeInclusive<u32>) -> std::result::Result<Self, Self::Error> {
        let (start, end) = (
            *value.start().min(value.end()),
            *value.start().max(value.end()),
        );
        if start == end {
            Ok(SequenceRange::single(start.try_into()?))
        } else {
            Ok(SequenceRange::range(start.try_into()?, end.try_into()?))
        }
    }
}

impl TryFrom<&imap_proto::rfc4315::UidSetMember> for SequenceRange {
    type Error = <Uid as TryFrom<u32>>::Error;

    fn try_from(
        value: &imap_proto::rfc4315::UidSetMember,
    ) -> std::result::Result<Self, Self::Error> {
        match value {
            imap_proto::rfc4315::UidSetMember::UidRange(range_inclusive) => {
                range_inclusive.try_into()
            }
            imap_proto::rfc4315::UidSetMember::Uid(uid) => Ok(Self::single(uid.try_into()?)),
        }
    }
}

#[derive(Debug, Error, Default)]
#[error("No numbers in sequence set")]
pub struct EmptySetError {}

#[derive(Default, Debug, PartialEq, Clone)]
pub struct SequenceSetBuilder {
    nums: HashSet<Uid>,
}

impl SequenceSetBuilder {
    pub fn add(&mut self, uid: Uid) {
        self.nums.insert(uid);
    }

    pub fn build(mut self) -> std::result::Result<SequenceSet, EmptySetError> {
        let mut sorted_nums: Vec<Uid> = self.nums.drain().collect();
        sorted_nums.sort_unstable();
        SequenceSet::from_sorted(sorted_nums)
    }
}

impl SequenceSet {
    /// Builds a set from ascending, duplicate-free UIDs without rehashing them like
    /// [`SequenceSetBuilder`]
    pub fn from_sorted(
        sorted_nums: impl IntoIterator<Item = Uid>,
    ) -> std::result::Result<Self, EmptySetError> {
        let mut sorted_nums = sorted_nums.into_iter();

        if let Some(first_num) = sorted_nums.next() {
            let mut ranges = Vec::new();
            let mut current_range = SequenceRange::single(first_num);

            for num in sorted_nums {
                if num == current_range.end() + 1 {
                    current_range.end = Some(num);
                } else {
                    ranges.push(current_range);
                    current_range = SequenceRange::single(num);
                }
            }

            ranges.push(current_range);

            Ok(SequenceSet { ranges })
        } else {
            Err(EmptySetError {})
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct SequenceSet {
    ranges: Vec<SequenceRange>,
}

impl SequenceSet {
    fn with_range(start: Uid, end: Uid) -> Self {
        Self {
            ranges: vec![SequenceRange::range(start, end)],
        }
    }

    pub fn all() -> Self {
        Self::with_range(1u32.try_into().expect("1 should be nonzero"), Uid::MAX)
    }

    pub fn iter(&self) -> impl Iterator<Item = Uid> {
        self.ranges.iter().flat_map(SequenceRange::iter)
    }

    /// Splits the set into sets, whose textual representation does not exceed `max_len` unless
    /// consisting of a single range, as servers limit the length of command lines (RFC 7162 4)
    pub fn chunks(&self, max_len: usize) -> Vec<SequenceSet> {
        let mut chunks = Vec::new();
        let mut ranges = Vec::new();
        let mut len = 0;
        for range in &self.ranges {
            let range_len = range.to_string().len();
            if !ranges.is_empty() && len + 1 + range_len > max_len {
                chunks.push(SequenceSet {
                    ranges: std::mem::take(&mut ranges),
                });
                len = 0;
            }
            len += range_len + usize::from(!ranges.is_empty());
            ranges.push(range.clone());
        }
        if !ranges.is_empty() {
            chunks.push(SequenceSet { ranges });
        }

        chunks
    }
}

impl Display for SequenceSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        for (i, range) in self.ranges.iter().enumerate() {
            if i > 0 {
                f.write_char(',')?;
            }
            write!(f, "{range}")?;
        }

        Ok(())
    }
}

impl TryFrom<&Vec<RangeInclusive<u32>>> for SequenceSet {
    type Error = <Uid as TryFrom<u32>>::Error;

    fn try_from(value: &Vec<RangeInclusive<u32>>) -> std::result::Result<Self, Self::Error> {
        let ranges: std::result::Result<Vec<_>, Self::Error> =
            value.iter().map(std::convert::TryInto::try_into).collect();

        Ok(Self { ranges: ranges? })
    }
}

impl From<Uid> for SequenceSet {
    fn from(uid: Uid) -> Self {
        Self {
            ranges: vec![SequenceRange::single(uid)],
        }
    }
}

impl TryFrom<&Vec<Uid>> for SequenceSet {
    type Error = EmptySetError;

    fn try_from(value: &Vec<Uid>) -> std::result::Result<Self, Self::Error> {
        value
            .iter()
            .fold(SequenceSetBuilder::default(), |mut builder, uid| {
                builder.add(*uid);
                builder
            })
            .build()
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;
    use rstest::*;

    use super::*;

    #[fixture]
    fn uid_single() -> Uid {
        assert_ok!(Uid::try_from(1))
    }

    #[fixture]
    fn sequence_range_single(uid_single: Uid) -> SequenceRange {
        SequenceRange::single(uid_single)
    }

    #[fixture]
    fn uid_start() -> Uid {
        assert_ok!(Uid::try_from(4))
    }

    #[fixture]
    fn uid_end() -> Uid {
        assert_ok!(Uid::try_from(9))
    }

    #[fixture]
    fn sequence_range(uid_start: Uid, uid_end: Uid) -> SequenceRange {
        SequenceRange::range(uid_start, uid_end)
    }

    #[rstest]
    fn test_chunks_limits_length_of_sets() {
        let uids: Vec<Uid> = (1..=20)
            .map(|uid| assert_ok!(Uid::try_from(uid * 2)))
            .collect();
        let set = assert_ok!(SequenceSet::try_from(&uids));

        let chunks = set.chunks(10);

        assert!(chunks.iter().all(|chunk| chunk.to_string().len() <= 10));
        assert_eq!(
            chunks
                .iter()
                .flat_map(SequenceSet::iter)
                .collect::<Vec<_>>(),
            uids
        );
    }

    #[rstest]
    fn test_chunks_keeps_short_set() {
        let set = SequenceSet::all();

        let chunks = set.chunks(10);

        assert_eq!(chunks, vec![SequenceSet::all()]);
    }

    #[rstest]
    fn test_sequence_range_normalizes_reversed_range() {
        let range = assert_ok!(SequenceRange::try_from(&RangeInclusive::new(5, 3)));

        assert_eq!("3:5", range.to_string());
        assert_eq!(3, range.into_iter().count());
    }

    #[rstest]
    fn test_sequence_range_displays_correctly(
        sequence_range: SequenceRange,
        sequence_range_single: SequenceRange,
    ) {
        assert_eq!("1", sequence_range_single.to_string());
        assert_eq!("4:9", sequence_range.to_string());
    }

    #[rstest]
    fn test_sequence_range_into_iter_returns_uid_from_start_to_end_inclusive(
        uid_single: Uid,
        sequence_range_single: SequenceRange,
        uid_start: Uid,
        uid_end: Uid,
        sequence_range: SequenceRange,
    ) {
        assert_eq!(
            sequence_range_single.into_iter().collect::<Vec<_>>(),
            uid_single.range_inclusive(uid_single).collect::<Vec<_>>(),
        );
        assert_eq!(
            sequence_range.into_iter().collect::<Vec<_>>(),
            uid_start.range_inclusive(uid_end).collect::<Vec<_>>(),
        );
    }

    #[rstest]
    fn test_sequence_range_from_u32_range_is_correct() {
        let start = 3u32;
        let end = 9u32;
        let range = assert_ok!(SequenceRange::try_from(&(start..=end)));
        assert_eq!(range.start, assert_ok!(start.try_into()));
        assert_eq!(assert_some!(range.end), assert_ok!(end.try_into()));
    }

    #[rstest]
    #[case(0, 3)]
    #[case(1, 0)]
    fn test_sequence_range_from_u32_range_fails_if_start_or_end_is_invalid_uid(
        #[case] start: u32,
        #[case] end: u32,
    ) {
        assert_err!(SequenceRange::try_from(&(start..=end)));
    }

    #[rstest]
    fn test_sequence_range_from_uid_set_member_is_correct() {
        let member = imap_proto::rfc4315::UidSetMember::Uid(3);
        let result = assert_ok!(SequenceRange::try_from(&member));
        assert_eq!(assert_ok!(Uid::try_from(3)), result.start);
        assert_none!(result.end);
        let member = imap_proto::rfc4315::UidSetMember::UidRange(3..=5);
        let result = assert_ok!(SequenceRange::try_from(&member));
        assert_eq!(assert_ok!(Uid::try_from(3)), result.start);
        assert_eq!(Some(assert_ok!(Uid::try_from(5))), result.end);
    }

    #[rstest]
    #[case("1:4294967295", SequenceSet::all())]
    #[case(
        "1:4",
        assert_ok!(
            SequenceSet::try_from(
                &assert_ok!(Uid::try_from(1))
                    .range_inclusive(assert_ok!(Uid::try_from(4)))
                    .collect::<Vec<_>>()
            )
        )
    )]
    #[case(
        "3:5,12,14:15",
        assert_ok!(
            SequenceSet::try_from(
                &vec![
                    assert_ok!(Uid::try_from(3)),
                    assert_ok!(Uid::try_from(4)),
                    assert_ok!(Uid::try_from(5)),
                    assert_ok!(Uid::try_from(12)),
                    assert_ok!(Uid::try_from(14)),
                    assert_ok!(Uid::try_from(15)),
                ]
            )
        )
    )]
    #[case(
        "1:4",
        assert_ok!(
            SequenceSet::try_from(
                #[expect(clippy::single_range_in_vec_init)]
                &vec![
                    (1u32..=4u32),
                ]
            )
        )
    )]
    #[case(
        "3:9,12,14:15",
        assert_ok!(
            SequenceSet::try_from(
                &vec![
                    (3u32..=9u32),
                    (12..=12),
                    (14..=15),
                ]
            )
        )
    )]
    fn test_sequence_set_displays_correctly(#[case] str: &str, #[case] set: SequenceSet) {
        assert_eq!(str, set.to_string());
    }

    #[rstest]
    fn test_sequence_set_builder_builds_correct_sequence_set() {
        let mut builder = SequenceSetBuilder::default();
        builder.add(assert_ok!(Uid::try_from(4)));
        builder.add(assert_ok!(Uid::try_from(4)));

        assert_eq!("4", assert_ok!(builder.build()).to_string());
    }

    #[rstest]
    fn test_sequence_set_builder_errors_on_empty_set() {
        let builder = SequenceSetBuilder::default();

        let result = assert_err!(builder.build());
        assert_matches!(result, EmptySetError {});
    }

    #[rstest]
    fn test_sequence_set_iter_returns_correct_uids() {
        let first_range = 1u32..=4;
        let second_range = 7u32..=7;
        let third_range = 9u32..=10;

        let set = assert_ok!(SequenceSet::try_from(&vec![
            first_range.clone(),
            second_range.clone(),
            third_range.clone()
        ]));
        assert_eq!(
            first_range
                .chain(second_range)
                .chain(third_range)
                .map(|n| assert_ok!(Uid::try_from(n)))
                .collect::<Vec<_>>(),
            set.iter().collect::<Vec<_>>()
        );
    }
}
