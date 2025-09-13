use std::fmt::{Display, Formatter, Result};

#[derive(Debug)]
pub struct SequenceRange {
    from: u32,
    to: Option<u32>,
}

impl SequenceRange {
    pub fn single(from: u32) -> Self {
        Self { from, to: None }
    }
    pub fn range(from: u32, to: u32) -> Self {
        Self { from, to: Some(to) }
    }

    pub fn len(&self) -> usize {
        self.to.map_or(1, |to| {
            usize::try_from(to - self.from).expect("converting u32 to usize should succeed") + 1
        })
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

#[derive(Debug)]
pub struct SequenceSet {
    ranges: Vec<SequenceRange>,
}

impl SequenceSet {
    pub fn with_range(from: u32, to: u32) -> Self {
        Self {
            ranges: vec![SequenceRange::range(from, to)],
        }
    }

    pub fn from_ranges(ranges: Vec<SequenceRange>) -> Self {
        Self { ranges }
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
