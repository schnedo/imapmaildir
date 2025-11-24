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
            Flag::Seen => write!(f, "\\Seen"),
            Flag::Answered => write!(f, "\\Answered"),
            Flag::Flagged => write!(f, "\\Flagged"),
            Flag::Deleted => write!(f, "\\Deleted"),
            Flag::Draft => write!(f, "\\Draft"),
            Flag::Recent => write!(f, "\\Recent"),
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
            "\\Seen" => Ok(Flag::Seen),
            "\\Answered" => Ok(Flag::Answered),
            "\\Flagged" => Ok(Flag::Flagged),
            "\\Deleted" => Ok(Flag::Deleted),
            "\\Draft" => Ok(Flag::Draft),
            "\\Recent" => Ok(Flag::Recent),
            _ => {
                trace!("Encountered unhandled Flag {value}");
                Err(Self::Err {
                    flag: value.to_string(),
                })
            }
        }
    }
}
