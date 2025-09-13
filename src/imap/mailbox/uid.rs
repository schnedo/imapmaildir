use std::{fmt::Display, num::NonZeroU32};

#[derive(Debug, PartialEq, Clone, Copy, Eq, Hash)]
#[repr(transparent)]
pub struct Uid(NonZeroU32);

impl Uid {
    pub fn max() -> Self {
        Self(NonZeroU32::MAX)
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
