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

#[cfg(test)]
mod tests {
    use assertables::*;
    use rstest::*;

    use super::*;

    #[rstest]
    fn test_range_inclusive_is_correct_range() {
        let start = assert_ok!(Uid::try_from(1u32));
        let end = assert_ok!(Uid::try_from(5u32));

        let expected: Vec<_> = (1u32..=5u32)
            .map(|n| assert_ok!(Uid::try_from(n)))
            .collect();
        let result: Vec<_> = start.range_inclusive(end).collect();

        assert_eq!(expected, result);
    }

    #[rstest]
    fn test_uid_add() {
        let a = assert_ok!(Uid::try_from(1u32));

        let result = a + 5u32;
        let expected = assert_ok!(Uid::try_from(6u32));
        assert_eq!(expected, result);
    }

    #[rstest]
    fn test_uid_add_assign() {
        let mut a = assert_ok!(Uid::try_from(1u32));

        a += 5u32;
        let expected = assert_ok!(Uid::try_from(6u32));
        assert_eq!(expected, a);
    }

    #[rstest]
    fn test_uid_serializes_to_correct_string() {
        let a = assert_ok!(Uid::try_from(1u32));

        assert_eq!("1", a.to_string());
    }

    #[rstest]
    fn test_uid_from_u32_and_refu32_is_the_same() {
        let a = assert_ok!(Uid::try_from(1u32));
        let b = assert_ok!(Uid::try_from(&1u32));

        assert_eq!(a, b);
    }

    #[rstest]
    fn test_uid_from_and_into_u32_are_consistent() {
        let expected = 1u32;
        let a = assert_ok!(Uid::try_from(expected));

        assert_eq!(expected, a.into());
        assert_eq!(expected, (&a).into());
    }
}
