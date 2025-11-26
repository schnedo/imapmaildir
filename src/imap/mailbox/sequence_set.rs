use std::{
    collections::HashSet,
    fmt::{Display, Formatter, Result},
    ops::RangeInclusive,
};
use thiserror::Error;

use crate::imap::Uid;

#[derive(Debug)]
struct SequenceRange {
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
        Self {
            start,
            end: Some(end),
        }
    }
    fn iter(&self) -> impl Iterator<Item = Uid> {
        let to = self.end.unwrap_or(self.start);

        self.start.range_inclusive(to)
    }
    fn end(&self) -> Uid {
        self.end.unwrap_or(self.start)
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

    pub fn add(&mut self, uid: Uid) {
        self.nums.insert(uid);
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

#[derive(Debug)]
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
