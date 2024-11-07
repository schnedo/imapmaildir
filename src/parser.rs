use nom::{
    branch::alt,
    bytes::complete::{escaped, tag, take, take_while, take_while1},
    character::complete::{char, crlf, digit1, not_line_ending, one_of},
    combinator::opt,
    multi::separated_list1,
    sequence::{delimited, pair, preceded, separated_pair, terminated},
    IResult, Parser,
};

#[derive(Debug, PartialEq)]
pub enum Tag {
    Tag(String),
    Untagged(),
}

fn is_list_wildcard(input: char) -> bool {
    input == '%' || input == '*'
}

const QUOTED_SPECIALS: &str = "\"\\";
fn is_quoted_special(input: char) -> bool {
    QUOTED_SPECIALS.contains(input)
}

fn is_resp_special(input: char) -> bool {
    input == ']'
}

// technically CTL is missing here
fn is_atom_special(input: char) -> bool {
    input == '('
        || input == ')'
        || input == '{'
        || input == ' '
        || is_list_wildcard(input)
        || is_quoted_special(input)
        || is_resp_special(input)
}

fn is_atom_char(input: char) -> bool {
    !is_atom_special(input)
}

fn is_astring_char(input: char) -> bool {
    is_atom_char(input) || is_resp_special(input)
}

fn is_astring_char_without_plus(input: char) -> bool {
    is_astring_char(input) && input != '+'
}

fn imap_tag(input: &str) -> IResult<&str, &str> {
    take_while1(is_astring_char_without_plus)(input)
}

fn text(input: &str) -> IResult<&str, &str> {
    not_line_ending(input)
}

fn is_not_quoted_special_or_escape(input: char) -> bool {
    !(input != '\\' || is_quoted_special(input))
}

fn quoted(input: &str) -> IResult<&str, &str> {
    delimited(
        char('"'),
        escaped(
            take_while(is_not_quoted_special_or_escape),
            '\\',
            one_of(QUOTED_SPECIALS),
        ),
        char('"'),
    )(input)
}

// u32
fn number(input: &str) -> IResult<&str, &str> {
    digit1(input)
}

fn is_char8(input: char) -> bool {
    (input as u32) != 0 && (input as u32) <= 0xff
}

// number represents the number of char8s
fn literal(input: &str) -> IResult<&str, &str> {
    let (rest, number) = terminated(delimited(char('{'), number, char('}')), crlf)(input)?;
    if let Ok(char8_length) = number.parse() {
        take(char8_length)(rest)
    } else {
        nom::Err::Error(input)
    }
}

fn string(input: &str) -> IResult<&str, &str> {
    alt((quoted, literal))(input)
}

fn astring(input: &str) -> IResult<&str, &str> {
    alt((take_while1(is_astring_char), string))(input)
}

enum ResponseTextCode<'a> {
    Alert,
    BadCharset(Option<Vec<&'a str>>),
}

fn resp_text_code<'a>(input: &'a str) -> IResult<&str, ResponseTextCode<'a>> {
    alt((
        tag("ALERT").map(|_| ResponseTextCode::Alert),
        pair(
            tag("BADCHARSET"),
            opt(preceded(
                space,
                delimited(char('('), separated_list1(space, astring), char(')')),
            )),
        )
        .map(|(_, charsets)| ResponseTextCode::BadCharset(charsets)),
        //tag("PARSE"),
        //tag("PERMANENTFLAGS"),
        //tag("READ-ONLY"),
        //tag("READ-WRITE"),
        //tag("TRYCREATE"),
        //tag("UIDNEXT"),
        //tag("UIDVALIDITY"),
        //tag("UNSEEN"),
    ))(input)
}

fn resp_text(input: &str) -> IResult<&str, (Option<&str>, &str)> {
    pair(
        opt(terminated(
            delimited(char('['), resp_text_code, char(']')),
            space,
        )),
        text,
    )(input)
}

fn response_cond_state(input: &str) -> IResult<&str, (&str, &str)> {
    separated_pair(alt((tag("OK"), tag("NO"), tag("BAD"))), space, resp_text)(input)
}

fn space(input: &str) -> IResult<&str, char> {
    char(' ')(input)
}

fn response_tagged(input: &str) -> IResult<&str, (&str, &str)> {
    terminated(separated_pair(imap_tag, space, imap_tag), crlf)(input)
}

impl Tag {
    fn new(raw: &str) -> Self {
        if raw == "*" {
            Self::Untagged()
        } else {
            Self::Tag(raw.to_string())
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ResponseCode {
    Alert(String),
    BadCharset(Vec<String>),
    Capability(Vec<String>),
    Parse(String),
    PermanentFlags(Vec<String>),
    ReadOnly,
    ReadWrite,
    TryCreate,
    UidNext(u32),
    UidValidity(u32),
    Unseen(u32),
    Unknown,
}

#[derive(Debug, PartialEq)]
pub enum StatusType {
    Ok,
    No,
    Bad,
    Preauth,
    Bye,
}

impl StatusType {
    fn try_new(raw: &str) -> Self {
        match raw.to_uppercase().as_str() {
            "OK" => Self::Ok,
            "NO" => Self::No,
            "BAD" => Self::Bad,
            "PREAUTH" => Self::Preauth,
            "BYE" => Self::Bye,
            _ => panic!("Unknown Status in response"),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct StatusResponse {
    tag: Tag,
    status: StatusType,
    response_code: Option<ResponseCode>,
}

pub fn parse_response(line: &str) -> StatusResponse {
    let mut line_split = line.split(' ');
    let tag = Tag::new(line_split.next().expect("tag should exist"));
    let status = StatusType::try_new(line_split.next().expect("status should exist"));
    let response_code = if let Some(foo) = line_split.next() {
        todo!("parse response_code -> use nom?");
    } else {
        None
    };
    StatusResponse {
        tag,
        status,
        response_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK_GREETING: &str = "* OK [CAPABILITY IMAP4rev1 SASL-IR LOGIN-REFERRALS ID ENABLE IDLE LITERAL+ AUTH=PLAIN] Dovecot (Debian) ready.\r\n";

    #[test]
    fn pare_ok_greeting() {
        let result = parse_response(OK_GREETING);
        assert_eq!(
            result,
            StatusResponse {
                tag: Tag::Untagged(),
                status: StatusType::Ok,
                response_code: Some(ResponseCode::Capability(vec![
                    "IMAP4rev1".to_string(),
                    "SASL-IR".to_string(),
                    "LOGIN-REFERRALS".to_string(),
                    "ID".to_string(),
                    "ENABLE".to_string(),
                    "IDLE".to_string(),
                    "LITERAL+".to_string(),
                    "AUTH=PLAIN".to_string()
                ])),
            }
        )
    }
}
