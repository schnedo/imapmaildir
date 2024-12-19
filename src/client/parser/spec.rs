use chrono::{DateTime, FixedOffset, NaiveTime, TimeZone};
use nom::{
    branch::alt,
    bytes::complete::{escaped, tag, take, take_while, take_while1},
    character::complete::{char, crlf, digit0, digit1, one_of},
    combinator::{all_consuming, map, opt},
    error::Error,
    multi::{many0, separated_list0, separated_list1},
    sequence::{delimited, pair, preceded, separated_pair, terminated, tuple},
    IResult, Parser,
};

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

fn is_char8(input: char) -> bool {
    (input as u32) != 0 && (input as u32) <= 0xff
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

fn is_text_char(input: char) -> bool {
    input != '\n' && input != '\r'
}

fn is_text_char_without_closing_square_bracket(input: char) -> bool {
    is_text_char(input) && input != ']'
}

fn is_not_quoted_special_or_escape(input: char) -> bool {
    !(input != '\\' || is_quoted_special(input))
}

// number represents the number of char8s
fn literal(input: &str) -> IResult<&str, &str> {
    let (rest, char8_length) = terminated(delimited(char('{'), number, char('}')), crlf)(input)?;
    let (rest, char8_sequence) = take_while(is_char8)(rest)?;
    if char8_sequence.len() as u32 == char8_length {
        Ok((rest, char8_sequence))
    } else {
        // ToDo: actually learn, how the error system in nom works
        Err(nom::Err::Error(Error::new(
            input,
            nom::error::ErrorKind::Float,
        )))
    }
}

#[derive(Debug, PartialEq)]
pub struct Tag<'a>(&'a str);
fn imap_tag(input: &str) -> IResult<&str, Tag> {
    map(take_while1(is_astring_char_without_plus), Tag)(input)
}

fn text(input: &str) -> IResult<&str, &str> {
    take_while1(is_text_char)(input)
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

fn number(input: &str) -> IResult<&str, u32> {
    let (rest, raw_number) = digit1(input)?;
    if let Ok(parsed_number) = raw_number.parse::<u32>() {
        Ok((rest, parsed_number))
    } else {
        Err(nom::Err::Error(Error::new(
            input,
            nom::error::ErrorKind::Float,
        )))
    }
}

fn two_digit(input: &str) -> IResult<&str, u32> {
    let (rest, raw_number) = take(2u32).and_then(all_consuming(digit0)).parse(input)?;
    if let Ok(parsed_number) = raw_number.parse::<u32>() {
        Ok((rest, parsed_number))
    } else {
        Err(nom::Err::Error(Error::new(
            input,
            nom::error::ErrorKind::Float,
        )))
    }
}

fn string(input: &str) -> IResult<&str, &str> {
    alt((quoted, literal))(input)
}

fn astring(input: &str) -> IResult<&str, &str> {
    alt((take_while1(is_astring_char), string))(input)
}

#[derive(Debug, PartialEq)]
pub enum Revision {
    FourRev1,
}
fn revision(input: &str) -> IResult<&str, Revision> {
    map(tag("IMAP4rev1"), |_| Revision::FourRev1)(input)
}

fn atom(input: &str) -> IResult<&str, &str> {
    take_while1(is_atom_char)(input)
}

pub struct AuthType<'a>(&'a str);
fn auth_type(input: &str) -> IResult<&str, AuthType> {
    // defined by https://datatracker.ietf.org/doc/html/rfc3501#ref-SASL
    map(atom, AuthType)(input)
}

fn capability(input: &str) -> IResult<&str, Capability> {
    // New capabilities MUST begin with "X" or be
    // registered with IANA as standard or
    // standards-track
    alt((
        map(preceded(tag("AUTH="), auth_type), |auth| {
            Capability::AuthType(auth.0)
        }),
        map(revision, Capability::Revision),
        map(atom, Capability::Custom),
    ))(input)
}

fn capability_data(input: &str) -> IResult<&str, Vec<Capability>> {
    preceded(
        preceded(tag("CAPABILITY"), space),
        separated_list1(space, capability),
    )(input)
}

fn nz_number(input: &str) -> IResult<&str, u32> {
    // technically first digit must not be 0, but server should handle this
    number(input)
}

fn flag_keyword(input: &str) -> IResult<&str, Flag> {
    map(atom, Flag::Keyword)(input)
}

fn flag_extension(input: &str) -> IResult<&str, Flag> {
    //; Future expansion.  Client implementations
    //; MUST accept flag-extension flags.  Server
    //; implementations MUST NOT generate
    //; flag-extension flags except as defined by
    //; future standard or standards-track
    //; revisions of this specification.
    map(preceded(char('\\'), atom), Flag::Extension)(input)
}

#[derive(Debug, PartialEq)]
pub enum Flag<'a> {
    Answered,
    Flagged,
    Deleted,
    Seen,
    Draft,
    Keyword(&'a str),
    Extension(&'a str),
    // technically flag-perm, not flag as defined by bakus-naur, but easier to parse
    Wildcard,
    // technically flag-fetch, not flag as defined by bakus-naur, but easier to parse
    Recent,
}
fn flag(input: &str) -> IResult<&str, Flag> {
    alt((
        map(tag("\\Answered"), |_| Flag::Answered),
        map(tag("\\Flagged"), |_| Flag::Flagged),
        map(tag("\\Deleted"), |_| Flag::Deleted),
        map(tag("\\Seen"), |_| Flag::Seen),
        map(tag("\\Draft"), |_| Flag::Draft),
        map(tag("\\*"), |_| Flag::Wildcard),
        map(tag("\\Recent"), |_| Flag::Recent),
        flag_keyword,
        flag_extension,
    ))(input)
}

#[derive(Debug, PartialEq)]
pub enum ResponseTextCode<'a> {
    Alert,
    BadCharset(Option<Vec<&'a str>>),
    Capability(Vec<Capability<'a>>),
    Parse,
    PermanentFlags(Vec<Flag<'a>>),
    ReadOnly,
    ReadWrite,
    TryCreate,
    UidNext(u32),
    UidValidity(u32),
    Unseen(u32),
    Custom(&'a str, Option<&'a str>),
}

fn resp_text_code(input: &str) -> IResult<&str, ResponseTextCode<'_>> {
    alt((
        tag("ALERT").map(|_| ResponseTextCode::Alert),
        preceded(
            tag("BADCHARSET"),
            opt(preceded(
                space,
                delimited(char('('), separated_list1(space, astring), char(')')),
            )),
        )
        .map(ResponseTextCode::BadCharset),
        capability_data.map(ResponseTextCode::Capability),
        tag("PARSE").map(|_| ResponseTextCode::Alert),
        delimited(
            separated_pair(tag("PERMANENTFLAGS"), space, char('(')),
            many0(flag),
            char(')'),
        )
        .map(ResponseTextCode::PermanentFlags),
        tag("READ-ONLY").map(|_| ResponseTextCode::Alert),
        tag("READ-WRITE").map(|_| ResponseTextCode::Alert),
        tag("TRYCREATE").map(|_| ResponseTextCode::Alert),
        separated_pair(tag("UIDNEXT"), space, nz_number)
            .map(|(_, number)| ResponseTextCode::UidNext(number)),
        separated_pair(tag("UIDVALIDITY"), space, nz_number)
            .map(|(_, number)| ResponseTextCode::UidValidity(number)),
        separated_pair(tag("UNSEEN"), space, nz_number)
            .map(|(_, number)| ResponseTextCode::Unseen(number)),
        pair(
            atom,
            opt(preceded(
                space,
                take_while1(is_text_char_without_closing_square_bracket),
            )),
        )
        .map(|(key, value)| ResponseTextCode::Custom(key, value)),
    ))(input)
}

#[derive(Debug, PartialEq)]
pub struct ResponseText<'a> {
    pub code: Option<ResponseTextCode<'a>>,
    pub text: &'a str,
}
fn resp_text(input: &str) -> IResult<&str, ResponseText> {
    map(
        pair(
            opt(terminated(
                delimited(char('['), resp_text_code, char(']')),
                space,
            )),
            text,
        ),
        |(code, text)| ResponseText { code, text },
    )(input)
}

#[derive(Debug, PartialEq)]
pub enum Status {
    Ok,
    Bad,
    No,
}
#[derive(Debug, PartialEq)]
pub struct ResponseCondState<'a> {
    status: Status,
    text: ResponseText<'a>,
}
fn resp_cond_state(input: &str) -> IResult<&str, ResponseCondState> {
    map(
        separated_pair(
            alt((
                map(tag("OK"), |_| Status::Ok),
                map(tag("NO"), |_| Status::Ok),
                map(tag("BAD"), |_| Status::Ok),
            )),
            space,
            resp_text,
        ),
        |(status, text)| ResponseCondState { status, text },
    )(input)
}

fn space(input: &str) -> IResult<&str, char> {
    char(' ')(input)
}

fn nstring(input: &str) -> IResult<&str, &str> {
    alt((tag("NIL"), string))(input)
}

fn uniqueid(input: &str) -> IResult<&str, u32> {
    nz_number(input) // strictly ascending
}

fn date_month(input: &str) -> IResult<&str, u32> {
    alt((
        map(tag("Jan"), |_| 1),
        map(tag("Feb"), |_| 2),
        map(tag("Mar"), |_| 3),
        map(tag("Apr"), |_| 4),
        map(tag("May"), |_| 5),
        map(tag("Jun"), |_| 6),
        map(tag("Jul"), |_| 7),
        map(tag("Aug"), |_| 8),
        map(tag("Sep"), |_| 9),
        map(tag("Oct"), |_| 10),
        map(tag("Nov"), |_| 11),
        map(tag("Dec"), |_| 12),
    ))(input)
}

fn date_year(input: &str) -> IResult<&str, u32> {
    number(input) // technically 4DIGIT
}

fn date_day_fixed(input: &str) -> IResult<&str, u32> {
    preceded(opt(space), number)(input) // technically (SP DIGIT) / 2DIGIT
}

fn time(input: &str) -> IResult<&str, (u32, u32, u32)> {
    // technically hh:mm:ss, not number:number:number
    tuple((number, delimited(char(':'), number, char(':')), number))(input)
}
enum PlusMinus {
    Plus,
    Minus,
}
fn zone(input: &str) -> IResult<&str, FixedOffset> {
    map(
        tuple((
            alt((
                map(char('+'), |_| PlusMinus::Plus),
                map(char('-'), |_| PlusMinus::Minus),
            )),
            two_digit,
            two_digit,
        )),
        |(plus_minus, hh, mm)| {
            let seconds = (mm * 60 + hh * 60 * 60)
                .try_into()
                .expect("seconds should be in i32 range");
            match plus_minus {
                PlusMinus::Plus => {
                    FixedOffset::west_opt(seconds).expect("west timezone should be parseable")
                }
                PlusMinus::Minus => {
                    FixedOffset::east_opt(seconds).expect("east timezone should be parseable")
                }
            }
        },
    )(input)
}

fn date_time(input: &str) -> IResult<&str, DateTime<FixedOffset>> {
    map(
        delimited(
            char('"'),
            tuple((
                date_day_fixed,
                delimited(char('-'), date_month, char('-')),
                date_year,
                delimited(space, time, space),
                zone,
            )),
            char('"'),
        ),
        |(day, month, year, (hour, min, sec), zone)| {
            zone.with_ymd_and_hms(year as i32, month, day, hour, min, sec)
                .unwrap()
        },
    )(input) // strictly ascending
}

fn resp_cond_auth(input: &str) -> IResult<&str, ResponseText> {
    preceded(pair(alt((tag("OK"), tag("PREAUTH"))), space), resp_text)(input)
}

fn resp_cond_bye(input: &str) -> IResult<&str, ResponseText> {
    preceded(pair(tag("BYE"), space), resp_text)(input)
}

fn response_fatal(input: &str) -> IResult<&str, ResponseText> {
    // Server closes connection immediately
    delimited(tag("*"), resp_cond_bye, crlf)(input)
}

fn msg_att_static(input: &str) -> IResult<&str, Vec<Flag>> {
    alt((
        separated_pair(tag("ENVELOPE"), space, envelope),
        separated_pair(tag("INTERNALDATE"), space, date_time),
        separated_pair(tag("RFC822.TEXT"), space, nstring),
        separated_pair(tag("RFC822.HEADER"), space, nstring),
        separated_pair(tag("RFC822"), space, nstring),
        separated_pair(tag("RFC822.SIZE"), space, number),
        separated_pair(tag("BODYSTRUCTURE"), space, body),
        separated_pair(tag("BODY"), space, body),
        separated_pair(
            tuple((
                tag("BODY"),
                section,
                opt(delimited(char('<'), number, char('>'))),
            )),
            space,
            nstring,
        ),
        separated_pair(tag("UID"), space, uniqueid),
    ))(input)
}

fn msg_att_dynamic(input: &str) -> IResult<&str, Vec<Flag>> {
    map(
        separated_pair(
            tag("FLAGS"),
            space,
            delimited(char('('), separated_list0(space, flag), char(')')),
        ),
        |(_, flags)| flags,
    )(input)
}

fn msg_att(input: &str) -> IResult<&str, &str> {
    delimited(
        char('('),
        separated_list1(space, alt((msg_att_dynamic, msg_att_static))),
        char(')'),
    )(input)
}

enum MessageDataType<'a> {
    Expunge,
    Fetch(&'a str),
}
fn message_data(input: &str) -> IResult<&str, (u32, MessageDataType)> {
    separated_pair(
        nz_number,
        space,
        alt((
            map(tag("EXPUNGE"), |_| MessageDataType::Expunge),
            map(separated_pair(tag("FETCH"), space, msg_att), |(_, attr)| {
                MessageDataType::Fetch(attr)
            }),
        )),
    )(input)
}

#[derive(Debug, PartialEq)]
pub struct TaggedResponse<'a> {
    tag: Tag<'a>,
    state: ResponseCondState<'a>,
}
fn response_tagged(input: &str) -> IResult<&str, TaggedResponse> {
    map(
        terminated(separated_pair(imap_tag, space, resp_cond_state), crlf),
        |(tag, state)| TaggedResponse { tag, state },
    )(input)
}

pub fn greeting(input: &str) -> IResult<&str, ResponseText> {
    delimited(
        pair(tag("*"), space),
        alt((resp_cond_auth, resp_cond_bye)),
        crlf,
    )(input)
}

#[derive(Debug, PartialEq)]
pub enum ResponseLine<'a> {
    CapabilityData(Vec<Capability<'a>>),
    CondBye(ResponseText<'a>),
    CondState(ResponseCondState<'a>),
    Tagged(TaggedResponse<'a>),
    Fatal(ResponseText<'a>),
}
pub fn response_done(input: &str) -> IResult<&str, ResponseLine> {
    alt((
        map(response_tagged, ResponseLine::Tagged),
        map(response_fatal, ResponseLine::Fatal),
    ))(input)
}

pub fn response_data(input: &str) -> IResult<&str, ResponseLine> {
    delimited(
        pair(tag("*"), space),
        alt((
            map(resp_cond_state, ResponseLine::CondState),
            map(resp_cond_bye, ResponseLine::CondBye),
            map(capability_data, ResponseLine::CapabilityData),
        )),
        crlf,
    )(input)
}

#[derive(Debug, PartialEq)]
pub enum Capability<'a> {
    AuthType(&'a str),
    Custom(&'a str),
    // technically not a capability as defined in bakus-naur, but easier to type this way
    Revision(Revision),
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK_GREETING: &str = "* OK [CAPABILITY IMAP4rev1 SASL-IR LOGIN-REFERRALS ID ENABLE IDLE LITERAL+ AUTH=PLAIN] Dovecot (Debian) ready.\r\n";

    #[test]
    fn parse_ok_greeting() {
        let (rest, response) = greeting(OK_GREETING).expect("response should be parseable");
        assert_eq!(
            response,
            ResponseText {
                code: Some(ResponseTextCode::Capability(vec![
                    Capability::Revision(Revision::FourRev1),
                    Capability::Custom("SASL-IR"),
                    Capability::Custom("LOGIN-REFERRALS"),
                    Capability::Custom("ID"),
                    Capability::Custom("ENABLE"),
                    Capability::Custom("IDLE"),
                    Capability::Custom("LITERAL+"),
                    Capability::AuthType("PLAIN"),
                ])),
                text: "Dovecot (Debian) ready."
            }
        );
        assert_eq!(rest, "")
    }
}
