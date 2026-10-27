use std::io::Write as _;
use std::ops::Range;

use compio::fs::File;

use crate::imap::transport::CommandPart;

const MODIFIED_BASE64: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

/// Octets up to which LITERAL- allows non-synchronizing literals (RFC 7888 3)
const LITERAL_MINUS_MAX_LEN: usize = 4096;

/// How literals get sent, depending on the server's support of RFC 7888
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiteralMode {
    /// All literals are non-synchronizing
    Plus,
    /// Literals up to [`LITERAL_MINUS_MAX_LEN`] octets are non-synchronizing
    Minus,
    /// All literals are synchronizing
    Sync,
}

impl LiteralMode {
    /// Whether a literal of `len` octets requires waiting for a continuation request
    pub fn is_sync(self, len: usize) -> bool {
        match self {
            Self::Plus => false,
            Self::Minus => len > LITERAL_MINUS_MAX_LEN,
            Self::Sync => true,
        }
    }
}

/// Builds a command, whose literals are synchronizing as required by [`LiteralMode`]
pub struct CommandBuilder<'a> {
    mode: LiteralMode,
    bytes: Vec<u8>,
    segments: Vec<Segment<'a>>,
    /// Start of the bytes not covered by `segments` yet
    pending: usize,
}

enum Segment<'a> {
    Bytes(Range<usize>),
    File { file: &'a File, len: usize },
    Continuation,
}

impl<'a> CommandBuilder<'a> {
    pub fn new(mode: LiteralMode, command: &str) -> Self {
        Self {
            mode,
            bytes: command.as_bytes().to_vec(),
            segments: Vec::new(),
            pending: 0,
        }
    }

    pub fn push_str(&mut self, value: &str) {
        self.bytes.extend_from_slice(value.as_bytes());
    }

    /// Appends `value` quoted, or as literal if it cannot be quoted
    pub fn push_astring(&mut self, value: &str) {
        if let Some(quoted) = quote(value) {
            self.push_str(&quoted);
        } else {
            self.push_literal_header(value.len());
            self.push_str(value);
        }
    }

    /// Appends the first `len` bytes of `file` as literal
    pub fn push_file_literal(&mut self, file: &'a File, len: usize) {
        self.push_literal_header(len);
        self.close_bytes();
        self.segments.push(Segment::File { file, len });
    }

    fn push_literal_header(&mut self, len: usize) {
        let sync = self.mode.is_sync(len);
        write!(self.bytes, "{{{len}{}}}\r\n", if sync { "" } else { "+" })
            .expect("writing literal header into vec should succeed");
        if sync {
            self.close_bytes();
            self.segments.push(Segment::Continuation);
        }
    }

    fn close_bytes(&mut self) {
        if self.pending < self.bytes.len() {
            self.segments
                .push(Segment::Bytes(self.pending..self.bytes.len()));
            self.pending = self.bytes.len();
        }
    }

    pub fn parts(&self) -> Vec<CommandPart<'_>> {
        let mut parts: Vec<_> = self
            .segments
            .iter()
            .map(|segment| match segment {
                Segment::Bytes(range) => CommandPart::Bytes(&self.bytes[range.clone()]),
                Segment::File { file, len } => CommandPart::File { file, len: *len },
                Segment::Continuation => CommandPart::Continuation,
            })
            .collect();
        if self.pending < self.bytes.len() {
            parts.push(CommandPart::Bytes(&self.bytes[self.pending..]));
        }
        parts
    }
}

/// Quotes `value`, if it is representable as quoted string
pub fn quote(value: &str) -> Option<String> {
    value
        .bytes()
        .all(|byte| (1..=0x7f).contains(&byte) && byte != b'\r' && byte != b'\n')
        .then(|| {
            let mut quoted = String::with_capacity(value.len() + 2);
            quoted.push('"');
            for c in value.chars() {
                if c == '"' || c == '\\' {
                    quoted.push('\\');
                }
                quoted.push(c);
            }
            quoted.push('"');
            quoted
        })
}

pub fn mailbox(name: &str) -> String {
    quote(&encode_modified_utf7(name))
        .expect("modified utf-7 encoded mailbox names should be quotable")
}

/// Decodes a modified UTF-7 mailbox name, falling back to the raw name if it is invalid
pub fn decode_mailbox(name: &str) -> String {
    decode_modified_utf7(name).unwrap_or_else(|| {
        log::warn!("cannot decode mailbox name {name} as modified utf-7");
        name.to_string()
    })
}

pub fn decode_modified_utf7(encoded: &str) -> Option<String> {
    let mut decoded = String::with_capacity(encoded.len());
    let mut rest = encoded;
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let shifted = &rest[start + 1..];
        let end = shifted.find('-')?;
        if end == 0 {
            decoded.push('&');
        } else {
            decoded.push_str(&decode_modified_base64(&shifted[..end])?);
        }
        rest = &shifted[end + 1..];
    }
    decoded.push_str(rest);

    Some(decoded)
}

fn decode_modified_base64(encoded: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(encoded.len() * 3 / 4);
    let mut bits = 0u32;
    let mut bit_count = 0;
    for byte in encoded.bytes() {
        let (_, index) = MODIFIED_BASE64
            .iter()
            .zip(0u32..)
            .find(|(c, _)| **c == byte)?;
        bits = ((bits << 6) | index) & 0x3fff;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            bytes.push((bits >> bit_count).to_le_bytes()[0]);
        }
    }
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();

    String::from_utf16(&units).ok()
}

pub fn encode_modified_utf7(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    let mut pending = Vec::new();
    for c in name.chars() {
        if (' '..='~').contains(&c) {
            flush_modified_base64(&mut encoded, &mut pending);
            if c == '&' {
                encoded.push_str("&-");
            } else {
                encoded.push(c);
            }
        } else {
            let mut buf = [0; 2];
            pending.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush_modified_base64(&mut encoded, &mut pending);

    encoded
}

fn flush_modified_base64(encoded: &mut String, pending: &mut Vec<u16>) {
    if pending.is_empty() {
        return;
    }
    let bytes: Vec<u8> = pending.drain(..).flat_map(u16::to_be_bytes).collect();
    encoded.push('&');
    for chunk in bytes.chunks(3) {
        let value = chunk
            .iter()
            .chain(std::iter::repeat(&0))
            .take(3)
            .fold(0u32, |value, &byte| (value << 8) | u32::from(byte));
        for i in 0..=chunk.len() {
            let index = (value >> (18 - 6 * i)) & 0x3f;
            encoded.push(char::from(MODIFIED_BASE64[index as usize]));
        }
    }
    encoded.push('-');
}

#[cfg(test)]
mod tests {
    use rstest::*;

    use super::*;

    #[rstest]
    #[case("INBOX", "\"INBOX\"")]
    #[case("my mail", "\"my mail\"")]
    #[case("a\"b\\c", "\"a\\\"b\\\\c\"")]
    fn test_quote(#[case] value: &str, #[case] expected: &str) {
        assert_eq!(quote(value).as_deref(), Some(expected));
    }

    #[rstest]
    #[case("pässword")]
    #[case("a\r\nb")]
    fn test_quote_rejects_unquotable(#[case] value: &str) {
        assert_eq!(quote(value), None);
    }

    #[rstest]
    #[case(LiteralMode::Plus, 1_000_000, false)]
    #[case(LiteralMode::Minus, 4096, false)]
    #[case(LiteralMode::Minus, 4097, true)]
    #[case(LiteralMode::Sync, 0, true)]
    fn test_literal_mode_is_sync(
        #[case] mode: LiteralMode,
        #[case] len: usize,
        #[case] expected: bool,
    ) {
        assert_eq!(mode.is_sync(len), expected);
    }

    fn rendered(builder: &CommandBuilder<'_>) -> Vec<String> {
        builder
            .parts()
            .iter()
            .map(|part| match part {
                CommandPart::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                CommandPart::File { len, .. } => format!("<file {len}>"),
                CommandPart::Continuation => "<continuation>".to_string(),
            })
            .collect()
    }

    #[rstest]
    #[case(LiteralMode::Plus, &["LOGIN \"user\" {9+}\r\npässword"])]
    #[case(LiteralMode::Minus, &["LOGIN \"user\" {9+}\r\npässword"])]
    #[case(
        LiteralMode::Sync,
        &["LOGIN \"user\" {9}\r\n", "<continuation>", "pässword"]
    )]
    fn test_command_builder_astring(#[case] mode: LiteralMode, #[case] expected: &[&str]) {
        let mut builder = CommandBuilder::new(mode, "LOGIN ");
        builder.push_astring("user");
        builder.push_str(" ");
        builder.push_astring("pässword");

        assert_eq!(rendered(&builder), expected);
    }

    #[compio::test]
    async fn test_command_builder_file_literals() {
        let dir = assertables::assert_ok!(tempfile::tempdir());
        let path = dir.path().join("mail");
        assertables::assert_ok!(std::fs::write(&path, b"x"));
        let file = assertables::assert_ok!(File::open(path).await);
        let mut builder = CommandBuilder::new(LiteralMode::Minus, "APPEND INBOX");
        builder.push_str(" ");
        builder.push_file_literal(&file, 4096);
        builder.push_str(" (\\Seen)");
        builder.push_str(" ");
        builder.push_file_literal(&file, 4097);

        assert_eq!(
            rendered(&builder),
            [
                "APPEND INBOX {4096+}\r\n",
                "<file 4096>",
                " (\\Seen) {4097}\r\n",
                "<continuation>",
                "<file 4097>"
            ]
        );
    }

    #[rstest]
    #[case("INBOX", "INBOX")]
    #[case("A&B", "A&-B")]
    #[case("Entwürfe", "Entw&APw-rfe")]
    #[case("日本語", "&ZeVnLIqe-")]
    #[case("~peter/mail/台北/日本語", "~peter/mail/&U,BTFw-/&ZeVnLIqe-")]
    fn test_encode_modified_utf7(#[case] name: &str, #[case] expected: &str) {
        assert_eq!(encode_modified_utf7(name), expected);
    }

    #[rstest]
    #[case("INBOX", "INBOX")]
    #[case("A&-B", "A&B")]
    #[case("Entw&APw-rfe", "Entwürfe")]
    #[case("&ZeVnLIqe-", "日本語")]
    #[case("~peter/mail/&U,BTFw-/&ZeVnLIqe-", "~peter/mail/台北/日本語")]
    fn test_decode_modified_utf7(#[case] encoded: &str, #[case] expected: &str) {
        assert_eq!(decode_modified_utf7(encoded).as_deref(), Some(expected));
    }

    #[rstest]
    #[case("&ZeVnLIqe")]
    #[case("&!-")]
    #[case("&AP-")]
    fn test_decode_modified_utf7_rejects_invalid(#[case] encoded: &str) {
        assert_eq!(decode_modified_utf7(encoded), None);
    }

    #[test]
    fn test_decode_mailbox_falls_back_to_raw_name() {
        assert_eq!(decode_mailbox("&ZeVnLIqe"), "&ZeVnLIqe");
    }
}
