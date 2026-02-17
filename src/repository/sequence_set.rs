use std::{
    collections::HashSet,
    fmt::{Display, Formatter, Result},
    ops::RangeInclusive,
};
use thiserror::Error;

use crate::repository::{Uid, uid::UidRangeInclusiveIterator};

// todo: does this need to be pub?
#[derive(Debug, PartialEq)]
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

    fn try_from(value: &RangeInclusive<u32>) -> std::result::Result<Self, Self::Error> {
        if value.start() == value.end() {
            Ok(SequenceRange::single(value.start().try_into()?))
        } else {
            Ok(SequenceRange::range(
                value.start().try_into()?,
                value.end().try_into()?,
            ))
        }
    }
}

impl TryFrom<&imap_proto::UidSetMember> for SequenceRange {
    type Error = <Uid as TryFrom<u32>>::Error;

    fn try_from(value: &imap_proto::UidSetMember) -> std::result::Result<Self, Self::Error> {
        match value {
            imap_proto::UidSetMember::UidRange(range_inclusive) => range_inclusive.try_into(),
            imap_proto::UidSetMember::Uid(uid) => Ok(Self::single(uid.try_into()?)),
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

    pub fn remove(&mut self, uid: Uid) -> bool {
        self.nums.remove(&uid)
    }

    pub fn build(mut self) -> std::result::Result<SequenceSet, EmptySetError> {
        let mut sorted_nums: Vec<Uid> = self.nums.drain().collect();
        sorted_nums.sort_unstable();
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
    // todo: use nonempty-collections
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
}

impl Display for SequenceSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        if let Some(string) =
            self.ranges
                .iter()
                .map(ToString::to_string)
                .reduce(|mut acc, range| {
                    acc.push(',');
                    acc + &range
                })
        {
            write!(f, "{string}")
        } else {
            write!(f, "")
        }
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
        let member = imap_proto::UidSetMember::Uid(3);
        let result = assert_ok!(SequenceRange::try_from(&member));
        assert_eq!(assert_ok!(Uid::try_from(3)), result.start);
        assert_none!(result.end);
        let member = imap_proto::UidSetMember::UidRange(3..=5);
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
        builder.add(assert_ok!(Uid::try_from(5)));
        builder.remove(assert_ok!(Uid::try_from(5)));

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
