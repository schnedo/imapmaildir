use std::{
    collections::HashSet,
    fmt::{Display, Formatter, Result},
    ops::RangeInclusive,
};
use thiserror::Error;

use crate::imap::Uid;

#[derive(Debug)]
struct SequenceRange {
    from: Uid,
    to: Option<Uid>,
}

impl SequenceRange {
    fn single(from: Uid) -> Self {
        Self { from, to: None }
    }
    fn range(from: Uid, to: Uid) -> Self {
        Self { from, to: Some(to) }
    }
    fn iter(&self) -> impl Iterator<Item = Uid> {
        let to = self.to.unwrap_or(self.from);

        self.from.range_inclusive(to)
    }
}

impl Display for SequenceRange {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        if let Some(to) = self.to {
            write!(f, "{}:{}", self.from, to)
        } else {
            write!(f, "{}", self.from)
        }
    }
}

#[derive(Debug, Error)]
#[error("No numbers in sequence set")]
pub struct EmptySetError {}

pub struct SequenceSetBuilder {
    nums: HashSet<Uid>,
}

impl SequenceSetBuilder {
    pub fn new() -> Self {
        Self {
            nums: HashSet::new(),
        }
    }

    pub fn add(&mut self, num: Uid) {
        self.nums.insert(num);
    }

    pub fn build(mut self) -> std::result::Result<SequenceSet, EmptySetError> {
        let mut sorted_nums: Vec<Uid> = self.nums.drain().collect();
        sorted_nums.sort_unstable();
        let mut sorted_nums = sorted_nums.into_iter();

        if let Some(first_num) = sorted_nums.next() {
            let mut ranges = Vec::new();
            let mut current_range = SequenceRange::single(first_num);

            for num in sorted_nums {
                if let Some(to) = current_range.to {
                    if num == to + 1 {
                        current_range.to = Some(num);
                    } else {
                        ranges.push(current_range);
                        current_range = SequenceRange::single(num);
                    }
                } else if num == current_range.from + 1 {
                    current_range.to = Some(num);
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

#[derive(Debug)]
pub struct SequenceSet {
    ranges: Vec<SequenceRange>,
}

impl SequenceSet {
    fn with_range(from: Uid, to: Uid) -> Self {
        Self {
            ranges: vec![SequenceRange::range(from, to)],
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

impl From<&Vec<RangeInclusive<u32>>> for SequenceSet {
    fn from(value: &Vec<RangeInclusive<u32>>) -> Self {
        Self {
            ranges: value
                .iter()
                .map(|range| {
                    SequenceRange::range(
                        range
                            .start()
                            .try_into()
                            .expect("received range start should be valid uid"),
                        range
                            .end()
                            .try_into()
                            .expect("received range end should be valid uid"),
                    )
                })
                .collect(),
        }
    }
}
