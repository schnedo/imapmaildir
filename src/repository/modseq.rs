use std::{fmt::Display, num::NonZeroU64};

#[repr(transparent)]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModSeq(NonZeroU64);

impl TryFrom<u64> for ModSeq {
    type Error = &'static str;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Ok(Self(
            NonZeroU64::new(value).ok_or("Cannot convert u64 to nonzero")?,
        ))
    }
}

impl TryFrom<&u64> for ModSeq {
    type Error = <Self as TryFrom<u64>>::Error;

    fn try_from(value: &u64) -> Result<Self, Self::Error> {
        Self::try_from(*value)
    }
}

impl From<ModSeq> for u64 {
    fn from(value: ModSeq) -> Self {
        value.0.into()
    }
}

impl From<&ModSeq> for u64 {
    fn from(value: &ModSeq) -> Self {
        value.0.into()
    }
}

impl Display for ModSeq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;
    use rstest::*;

    use super::*;

    #[rstest]
    fn test_from_and_into_i64_are_consistent() {
        let expected = 8u64;
        let expected_ref = &8u64;
        let modseq = assert_ok!(ModSeq::try_from(expected));
        let modseq_ref = assert_ok!(ModSeq::try_from(expected_ref));
        assert_eq!(modseq, modseq_ref);
        assert_eq!(expected, modseq.into());
        assert_eq!(expected, (&modseq).into());
    }

    #[rstest]
    fn test_modseq_serializes_to_string() {
        let modseq = assert_ok!(ModSeq::try_from(8u64));
        assert_eq!("8", modseq.to_string());
    }
}
