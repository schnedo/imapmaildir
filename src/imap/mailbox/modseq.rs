use std::{fmt::Display, num::NonZeroU64};

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
