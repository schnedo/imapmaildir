use std::{
    fmt::Display,
    num::NonZeroU32,
    ops::{Add, AddAssign},
};

#[derive(Debug, PartialEq, Clone, Copy, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Uid(NonZeroU32);

impl Uid {
    pub const MAX: Self = Self(NonZeroU32::MAX);

    pub fn range_inclusive(self, end: Self) -> UidRangeInclusiveIterator {
        UidRangeInclusiveIterator::new(self, end)
    }
}

impl AddAssign<u32> for Uid {
    fn add_assign(&mut self, rhs: u32) {
        self.0 = self.0.saturating_add(rhs);
    }
}

impl Add<u32> for Uid {
    type Output = Uid;

    fn add(self, rhs: u32) -> Self::Output {
        Uid(self.0.saturating_add(rhs))
    }
}

impl TryFrom<&u32> for Uid {
    type Error = <Self as TryFrom<u32>>::Error;

    fn try_from(value: &u32) -> Result<Self, Self::Error> {
        Self::try_from(*value)
    }
}

impl Display for Uid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<u32> for Uid {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Ok(Self(
            NonZeroU32::new(value).ok_or("Cannot convert u32 to nonzero")?,
        ))
    }
}

impl TryFrom<i64> for Uid {
    type Error = &'static str;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if let Ok(num) = value.try_into() {
            NonZeroU32::new(num)
                .ok_or("Cannot convert u32 to nonzero")
                .map(Self)
        } else {
            Err("i64 too large")
        }
    }
}

impl From<Uid> for u32 {
    fn from(value: Uid) -> Self {
        value.0.into()
    }
}

impl From<&Uid> for u32 {
    fn from(value: &Uid) -> Self {
        value.0.into()
    }
}

pub struct UidRangeInclusiveIterator {
    current: u32,
    end: u32,
}

impl UidRangeInclusiveIterator {
    fn new(start: Uid, end: Uid) -> Self {
        debug_assert!(
            start <= end,
            "inclusive range end should be larger than start"
        );
        Self {
            current: start.0.get() - 1,
            end: end.0.get(),
        }
    }
}

impl Iterator for UidRangeInclusiveIterator {
    type Item = Uid;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current == self.end {
            None
        } else {
            self.current += 1;

            Some(self.current.try_into().expect("n cannot be none here"))
        }
    }
}
