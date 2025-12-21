use std::{fmt::Display, num::NonZeroU32};

#[derive(Clone, Debug, PartialEq, Copy)]
#[repr(transparent)]
pub struct UidValidity(NonZeroU32);

impl UidValidity {
    pub fn new(validity: NonZeroU32) -> Self {
        Self(validity)
    }
}

impl Display for UidValidity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<&u32> for UidValidity {
    type Error = &'static str;

    fn try_from(value: &u32) -> Result<Self, Self::Error> {
        (*value).try_into()
    }
}

impl TryFrom<u32> for UidValidity {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        NonZeroU32::new(value)
            .map(UidValidity)
            .ok_or("provided should not be zero")
    }
}

impl From<UidValidity> for u32 {
    fn from(value: UidValidity) -> Self {
        value.0.into()
    }
}
