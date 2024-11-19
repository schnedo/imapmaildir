use nom::{
    branch::alt,
    bytes::complete::{escaped, tag, take_while, take_while1},
    character::complete::{char, crlf, digit1, one_of},
    combinator::{map, opt},
    error::Error,
    multi::{many0, separated_list1},
    sequence::{delimited, pair, preceded, separated_pair, terminated},
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
pub(super) struct Tag<'a>(&'a str);
pub(super) fn imap_tag(input: &str) -> IResult<&str, Tag> {
    map(take_while1(is_astring_char_without_plus), |raw| Tag(raw))(input)
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

fn string(input: &str) -> IResult<&str, &str> {
    alt((quoted, literal))(input)
}

fn astring(input: &str) -> IResult<&str, &str> {
    alt((take_while1(is_astring_char), string))(input)
}

#[derive(Debug, PartialEq)]
pub(super) enum Revision {
    FourRev1,
}
fn revision(input: &str) -> IResult<&str, Revision> {
    map(tag("IMAP4rev1"), |_| Revision::FourRev1)(input)
}

fn atom(input: &str) -> IResult<&str, &str> {
    take_while1(is_atom_char)(input)
}

pub(super) struct AuthType<'a>(&'a str);
fn auth_type(input: &str) -> IResult<&str, AuthType> {
    // defined by https://datatracker.ietf.org/doc/html/rfc3501#ref-SASL
    map(atom, |auth| AuthType(auth))(input)
}

#[derive(Debug, PartialEq)]
pub(super) enum Capability<'a> {
    AuthType(&'a str),
    Custom(&'a str),
    // technically not a capability as defined in bakus-naur, but easier to type this way
    Revision(Revision),
}
fn capability(input: &str) -> IResult<&str, Capability> {
    // New capabilities MUST begin with "X" or be
    // registered with IANA as standard or
    // standards-track
    alt((
        map(preceded(tag("AUTH="), auth_type), |auth| {
            Capability::AuthType(auth.0)
        }),
        map(revision, |r| Capability::Revision(r)),
        map(atom, |a| Capability::Custom(a)),
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
    map(atom, |a| Flag::Keyword(a))(input)
}

fn flag_extension(input: &str) -> IResult<&str, Flag> {
    //; Future expansion.  Client implementations
    //; MUST accept flag-extension flags.  Server
    //; implementations MUST NOT generate
    //; flag-extension flags except as defined by
    //; future standard or standards-track
    //; revisions of this specification.
    map(preceded(char('\\'), atom), |a| Flag::Extension(a))(input)
}

#[derive(Debug, PartialEq)]
pub(super) enum Flag<'a> {
    Answered,
    Flagged,
    Deleted,
    Seen,
    Draft,
    Keyword(&'a str),
    Extension(&'a str),
    // technically flag-perm, not flag as defined by bakus-naur, but easier to parse
    Wildcard,
}
fn flag(input: &str) -> IResult<&str, Flag> {
    alt((
        map(tag("\\Answered"), |_| Flag::Answered),
        map(tag("\\Flagged"), |_| Flag::Flagged),
        map(tag("\\Deleted"), |_| Flag::Deleted),
        map(tag("\\Seen"), |_| Flag::Seen),
        map(tag("\\Draft"), |_| Flag::Draft),
        map(tag("\\*"), |_| Flag::Wildcard),
        flag_keyword,
        flag_extension,
        //does not include \"Recent"
    ))(input)
}

#[derive(Debug, PartialEq)]
enum ResponseTextCode<'a> {
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

fn resp_text_code<'a>(input: &'a str) -> IResult<&str, ResponseTextCode<'a>> {
    alt((
        tag("ALERT").map(|_| ResponseTextCode::Alert),
        preceded(
            tag("BADCHARSET"),
            opt(preceded(
                space,
                delimited(char('('), separated_list1(space, astring), char(')')),
            )),
        )
        .map(|charsets| ResponseTextCode::BadCharset(charsets)),
        capability_data.map(|capabilities| ResponseTextCode::Capability(capabilities)),
        tag("PARSE").map(|_| ResponseTextCode::Alert),
        delimited(
            separated_pair(tag("PERMANENTFLAGS"), space, char('(')),
            many0(flag),
            char(')'),
        )
        .map(|flags| ResponseTextCode::PermanentFlags(flags)),
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
pub(super) struct ResponseText<'a> {
    code: Option<ResponseTextCode<'a>>,
    text: &'a str,
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
pub(super) enum Status {
    Ok,
    Bad,
    No,
}
#[derive(Debug, PartialEq)]
pub(super) struct ResponseCondState<'a> {
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

#[derive(Debug, PartialEq)]
pub(super) struct TaggedResponse<'a> {
    tag: Tag<'a>,
    state: ResponseCondState<'a>,
}
fn response_tagged(input: &str) -> IResult<&str, TaggedResponse> {
    map(
        terminated(separated_pair(imap_tag, space, resp_cond_state), crlf),
        |(tag, state)| TaggedResponse { tag, state },
    )(input)
}

pub(super) fn greeting(input: &str) -> IResult<&str, ResponseText> {
    delimited(
        pair(tag("*"), space),
        alt((resp_cond_auth, resp_cond_bye)),
        crlf,
    )(input)
}

#[derive(Debug, PartialEq)]
pub(super) enum ResponseDone<'a> {
    Tagged(TaggedResponse<'a>),
    Fatal(ResponseText<'a>),
}
pub(super) fn response_done(input: &str) -> IResult<&str, ResponseDone> {
    alt((
        map(response_tagged, |tagged| ResponseDone::Tagged(tagged)),
        map(response_fatal, |fatal| ResponseDone::Fatal(fatal)),
    ))(input)
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
