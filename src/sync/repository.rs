use std::{borrow::Cow, fmt::Debug};

use enumflags2::{BitFlags, bitflags};
use log::trace;
use thiserror::Error;

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

impl From<char> for Flag {
    fn from(value: char) -> Self {
        match value {
            'D' => Flag::Draft,
            'F' => Flag::Flagged,
            'R' => Flag::Answered,
            'S' => Flag::Seen,
            'T' => Flag::Deleted,
            _ => panic!("unknown flag"),
        }
    }
}

#[derive(Error, Debug)]
#[error("unknown flag {flag}")]
pub struct UnknownFlagError<'a> {
    flag: &'a str,
}
impl<'a> TryFrom<&'a str> for Flag {
    type Error = UnknownFlagError<'a>;

    fn try_from(value: &'a str) -> std::result::Result<Self, Self::Error> {
        match value {
            "\\Seen" => Ok(Flag::Seen),
            "\\Answered" => Ok(Flag::Answered),
            "\\Flagged" => Ok(Flag::Flagged),
            "\\Deleted" => Ok(Flag::Deleted),
            "\\Draft" => Ok(Flag::Draft),
            "\\Recent" => Ok(Flag::Recent),
            _ => {
                trace!("Encountered unhandled Flag {value}");
                Err(Self::Error { flag: value })
            }
        }
    }
}
