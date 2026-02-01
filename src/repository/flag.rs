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

#[cfg(test)]
mod tests {
    use assertables::*;
    use rstest::*;

    use super::*;

    #[rstest]
    #[case(Flag::Seen, r"\Seen")]
    #[case(Flag::Answered, r"\Answered")]
    #[case(Flag::Flagged, r"\Flagged")]
    #[case(Flag::Deleted, r"\Deleted")]
    #[case(Flag::Draft, r"\Draft")]
    fn test_flag_parses_from_string(#[case] expected: Flag, #[case] string: &str) {
        let result = assert_ok!(string.parse());
        assert_eq!(expected, result);
    }

    #[rstest]
    fn test_flag_parsing_errors_on_invalid_string(
        #[values(r"\Recent", "adskljfalk")] string: &str,
    ) {
        let result = assert_err!(Flag::from_str(string));
        assert_eq!(result.flag, string);
    }

    #[rstest]
    #[case(Flag::Seen, r"\Seen")]
    #[case(Flag::Answered, r"\Answered")]
    #[case(Flag::Flagged, r"\Flagged")]
    #[case(Flag::Deleted, r"\Deleted")]
    #[case(Flag::Draft, r"\Draft")]
    fn test_flag_serializes_to_string(#[case] flag: Flag, #[case] expected: &str) {
        let result = flag.to_string();
        assert_eq!(result, expected);
    }

    #[rstest]
    #[case(Flag::Seen, 'S')]
    #[case(Flag::Answered, 'R')]
    #[case(Flag::Flagged, 'F')]
    #[case(Flag::Deleted, 'T')]
    #[case(Flag::Draft, 'D')]
    fn test_flag_parses_from_char(#[case] expected: Flag, #[case] char: char) {
        let result = char.into();
        assert_eq!(expected, result);
    }

    #[rstest]
    fn test_flags_parse_from_multiple_strings() {
        let raw = vec![Cow::Borrowed(r"\Seen"), Cow::Borrowed(r"\Draft")];
        let result = Flag::into_bitflags(&raw);

        assert_eq!(result, Flag::Seen | Flag::Draft);
    }

    #[rstest]
    fn test_flags_serialize_to_string() {
        let flags = Flag::Seen | Flag::Draft;

        let result = assert_some!(Flag::format(flags));
        assert_eq!(result, r"\Draft \Seen");
    }
}
