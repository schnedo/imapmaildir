use std::{borrow::Cow, fmt::Debug};

use enumflags2::{BitFlags, bitflags};

#[bitflags]
#[repr(u8)]
#[derive(Copy, Clone, Debug)]
pub enum Flag {
    Seen,
    Answered,
    Flagged,
    Deleted,
    Draft,
    Recent,
}

impl Flag {
    pub fn into_bitflags(flags: &Vec<Cow<str>>) -> BitFlags<Flag, u8> {
        flags
            .iter()
            .filter_map(|flag| <&str as TryInto<Flag>>::try_into(flag.as_ref()).ok())
            .collect()
    }
}
