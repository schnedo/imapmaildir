const MODIFIED_BASE64: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

pub fn quote(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| (1..=0x7f).contains(&byte) && byte != b'\r' && byte != b'\n')
    {
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
    } else {
        format!("{{{}+}}\r\n{value}", value.len())
    }
}

pub fn mailbox(name: &str) -> String {
    quote(&encode_modified_utf7(name))
}

fn encode_modified_utf7(name: &str) -> String {
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
    #[case("pässword", "{9+}\r\npässword")]
    fn test_quote(#[case] value: &str, #[case] expected: &str) {
        assert_eq!(quote(value), expected);
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
}
