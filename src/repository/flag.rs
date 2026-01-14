use std::fmt::Write as _;
use std::{
    borrow::Cow,
    fmt::{Debug, Display},
    str::FromStr,
};

use enumflags2::{BitFlags, bitflags};
use log::trace;
use thiserror::Error;

#[bitflags]
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
// todo: support keywords https://datatracker.ietf.org/doc/html/rfc3501#section-2.3.2
// DO NOT REORDER! The bitflags representation is stored in database. Changing the layout **will** break things.
pub enum Flag {
    Draft,
    Flagged,
    Answered,
    Seen,
    Deleted,
}

impl Flag {
    pub fn into_bitflags(flags: &Vec<Cow<str>>) -> BitFlags<Self, u8> {
        flags
            .iter()
            .filter_map(|flag| Flag::from_str(flag).ok())
            .collect()
    }

    pub fn format(flags: BitFlags<Self>) -> Option<String> {
        flags
            .iter()
            .map(|flag| flag.to_string())
            .reduce(|mut acc, flag| {
                write!(acc, " {flag}").expect("writing flag to formatting buffer should succeed");
                acc
            })
    }
}

impl Display for Flag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Flag::Seen => write!(f, r"\Seen"),
            Flag::Answered => write!(f, r"\Answered"),
            Flag::Flagged => write!(f, r"\Flagged"),
            Flag::Deleted => write!(f, r"\Deleted"),
            Flag::Draft => write!(f, r"\Draft"),
        }
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
pub struct UnknownFlagError {
    flag: String,
}
impl FromStr for Flag {
    type Err = UnknownFlagError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            r"\Seen" => Ok(Flag::Seen),
            r"\Answered" => Ok(Flag::Answered),
            r"\Flagged" => Ok(Flag::Flagged),
            r"\Deleted" => Ok(Flag::Deleted),
            r"\Draft" => Ok(Flag::Draft),
            r"\Recent" => {
                trace!(r"\Recent flag handled by server. skipping...");
                Err(Self::Err {
                    flag: value.to_string(),
                })
            }
            _ => {
                trace!("Encountered unhandled Flag {value}");
                Err(Self::Err {
                    flag: value.to_string(),
                })
            }
        }
    }
}
