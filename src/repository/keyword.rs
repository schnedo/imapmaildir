use std::{borrow::Cow, fmt::Display};

/// Where the server keeps the keywords of mails
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum KeywordSource {
    /// Non-system flags of `FLAGS` (RFC 3501 2.3.2)
    #[default]
    Flags,
    /// Labels of Gmail's `X-GM-LABELS` extension, which also holds keywords set via `FLAGS`
    GmailLabels,
}

impl KeywordSource {
    pub fn as_str(self) -> &'static str {
        match self {
            KeywordSource::Flags => "flags",
            KeywordSource::GmailLabels => "gmail-labels",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "flags" => Some(KeywordSource::Flags),
            "gmail-labels" => Some(KeywordSource::GmailLabels),
            _ => None,
        }
    }
}

/// IMAP keywords or Gmail labels of a mail as sent by the server, e.g. `$Junk` or `\Inbox`.
/// Kept sorted and joined like in the state, as e.g. fetching the labels of all mails holds
/// millions of them at once, which a set per mail would multiply.
#[derive(Debug, Default, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Keywords(Box<str>);

/// Separates keywords in the state. Neither keywords nor labels can contain line breaks.
const STORED_SEPARATOR: char = '\n';

impl Keywords {
    /// Keywords of a `FLAGS` response, i.e. all flags but system ones starting with `\`
    pub fn from_flags(flags: &[Cow<str>]) -> Self {
        Self::from_unsorted(
            flags
                .iter()
                .map(AsRef::as_ref)
                .filter(|flag| !flag.starts_with('\\')),
        )
    }

    /// Labels of a `X-GM-LABELS` response
    pub fn from_labels(labels: &[Cow<str>]) -> Self {
        Self::from_unsorted(labels.iter().map(AsRef::as_ref))
    }

    pub fn from_stored(stored: &str) -> Self {
        Self::from_unsorted(stored.split(STORED_SEPARATOR))
    }

    #[cfg(test)]
    pub fn of(values: &[&str]) -> Self {
        Self::from_unsorted(values.iter().copied())
    }

    /// Sorts `keywords` and drops duplicates and ones, that cannot be stored, e.g. empty ones
    fn from_unsorted<'a>(keywords: impl IntoIterator<Item = &'a str>) -> Self {
        let mut sorted: Vec<&str> = keywords
            .into_iter()
            .filter(|keyword| !keyword.is_empty() && !keyword.contains(STORED_SEPARATOR))
            .collect();
        sorted.sort_unstable();
        sorted.dedup();
        Self::from_sorted(sorted)
    }

    /// `keywords` must be sorted, unique and storable
    fn from_sorted<'a>(keywords: impl IntoIterator<Item = &'a str>) -> Self {
        let mut joined = String::new();
        for keyword in keywords {
            if !joined.is_empty() {
                joined.push(STORED_SEPARATOR);
            }
            joined.push_str(keyword);
        }
        Self(joined.into_boxed_str())
    }

    pub fn to_stored(&self) -> String {
        self.0.to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, keyword: &str) -> bool {
        self.iter().any(|contained| contained == keyword)
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter(
            (!self.0.is_empty())
                .then(|| self.0.split(STORED_SEPARATOR))
                .into_iter()
                .flatten(),
        )
    }

    pub fn retain(&mut self, mut f: impl FnMut(&str) -> bool) {
        let retained = Self::from_sorted(self.iter().filter(|keyword| f(keyword)));
        *self = retained;
    }

    /// Keywords of `self` missing in `other`
    #[must_use]
    pub fn difference(&self, other: &Self) -> Self {
        Self::from_sorted(self.iter().filter(|keyword| !other.contains(keyword)))
    }

    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        let mut merged: Vec<&str> = self.iter().chain(other.iter()).collect();
        merged.sort_unstable();
        merged.dedup();
        Self::from_sorted(merged)
    }

    /// Whether `keyword` can be stored for `source`
    pub fn is_storable(keyword: &str, source: KeywordSource) -> bool {
        match source {
            KeywordSource::Flags => is_atom(keyword),
            KeywordSource::GmailLabels => match keyword.strip_prefix('\\') {
                Some(system) => is_atom(system),
                None => is_quotable(keyword),
            },
        }
    }
}

/// Whether `value` is representable as non-empty quoted string (RFC 3501 9)
fn is_quotable(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| (1..=0x7f).contains(&byte) && byte != b'\r' && byte != b'\n')
}

/// Whether `value` is an IMAP atom, as required for keywords (RFC 3501 9)
fn is_atom(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            (0x21..0x7f).contains(&byte)
                && !matches!(byte, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']')
        })
}

impl FromIterator<String> for Keywords {
    fn from_iter<T: IntoIterator<Item = String>>(iter: T) -> Self {
        let keywords: Vec<String> = iter.into_iter().collect();
        Self::from_unsorted(keywords.iter().map(String::as_str))
    }
}

impl<'a> FromIterator<&'a str> for Keywords {
    fn from_iter<T: IntoIterator<Item = &'a str>>(iter: T) -> Self {
        Self::from_unsorted(iter)
    }
}

/// Iterator over sorted keywords
pub struct Iter<'a>(std::iter::Flatten<std::option::IntoIter<std::str::Split<'a, char>>>);

impl<'a> Iterator for Iter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

impl<'a> IntoIterator for &'a Keywords {
    type Item = &'a str;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Display for Keywords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.iter().collect::<Vec<_>>().join(" "))
    }
}

/// Keywords added to and removed from mails
#[derive(Debug, Default, Clone, Hash, PartialEq, Eq)]
pub struct KeywordChange {
    pub added: Keywords,
    pub removed: Keywords,
}

impl KeywordChange {
    /// The change turning `old` into `new`
    pub fn between(old: &Keywords, new: &Keywords) -> Self {
        Self {
            added: new.difference(old),
            removed: old.difference(new),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }

    /// The change of applying `self` and then `next`
    #[must_use]
    pub fn then(&self, next: &Self) -> Self {
        Self {
            added: self.added.difference(&next.removed).union(&next.added),
            removed: self.removed.difference(&next.added).union(&next.removed),
        }
    }

    /// Applies the change to `keywords`
    #[must_use]
    pub fn apply(&self, keywords: &Keywords) -> Keywords {
        keywords.difference(&self.removed).union(&self.added)
    }
}

#[cfg(test)]
mod tests {
    use rstest::*;

    use super::*;

    #[test]
    fn test_from_flags_skips_system_flags() {
        let flags = vec![
            Cow::Borrowed(r"\Seen"),
            Cow::Borrowed("$Junk"),
            Cow::Borrowed("work"),
            Cow::Borrowed(r"\Recent"),
        ];

        assert_eq!(
            Keywords::from_flags(&flags),
            Keywords::of(&["$Junk", "work"])
        );
    }

    #[test]
    fn test_from_labels_keeps_system_labels() {
        let labels = vec![Cow::Borrowed(r"\Inbox"), Cow::Borrowed("my label")];

        assert_eq!(
            Keywords::from_labels(&labels),
            Keywords::of(&[r"\Inbox", "my label"])
        );
    }

    #[rstest]
    #[case(&[])]
    #[case(&["one"])]
    #[case(&["one", "two words", r"\Inbox"])]
    fn test_stored_keywords_roundtrip(#[case] values: &[&str]) {
        let expected = Keywords::of(values);

        assert_eq!(Keywords::from_stored(&expected.to_stored()), expected);
    }

    #[test]
    fn test_keywords_get_sorted_and_deduplicated_like_stored_ones() {
        let labels = vec![
            Cow::Borrowed("b"),
            Cow::Borrowed(r"\Inbox"),
            Cow::Borrowed("a"),
            Cow::Borrowed("b"),
            Cow::Borrowed(""),
            Cow::Borrowed("line\nbreak"),
        ];

        let keywords = Keywords::from_labels(&labels);

        // states store them like this, which e.g. finding pending tags compares
        assert_eq!(keywords.to_stored(), "\\Inbox\na\nb");
        assert_eq!(
            keywords.iter().collect::<Vec<_>>(),
            vec![r"\Inbox", "a", "b"]
        );
        assert!(keywords.contains("a"));
        assert!(!keywords.contains("c"));
    }

    #[test]
    fn test_keywords_combine() {
        let (one, other) = (Keywords::of(&["a", "b"]), Keywords::of(&["b", "c"]));

        assert_eq!(one.union(&other), Keywords::of(&["a", "b", "c"]));
        assert_eq!(one.difference(&other), Keywords::of(&["a"]));
        assert_eq!(one.difference(&one), Keywords::default());
        assert!(Keywords::default().is_empty());
        assert_eq!(Keywords::default().iter().count(), 0);
    }

    #[test]
    fn test_keywords_retain() {
        let mut keywords = Keywords::of(&["a", "b", "c"]);

        keywords.retain(|keyword| keyword != "b");

        assert_eq!(keywords, Keywords::of(&["a", "c"]));
    }

    #[rstest]
    #[case("two words")]
    #[case("paren(")]
    #[case(r"\Seen")]
    #[case("")]
    #[case("ü")]
    fn test_non_atoms_are_no_storable_keywords(#[case] keyword: &str) {
        assert!(!Keywords::is_storable(keyword, KeywordSource::Flags));
    }

    #[rstest]
    #[case("$Junk", true)]
    #[case("two words", true)]
    #[case(r"\Inbox", true)]
    #[case(r"\In box", false)]
    #[case("line\nbreak", false)]
    #[case("ü", false)]
    #[case("", false)]
    fn test_storable_labels(#[case] label: &str, #[case] storable: bool) {
        assert_eq!(
            Keywords::is_storable(label, KeywordSource::GmailLabels),
            storable
        );
    }

    #[test]
    fn test_change_between_keywords() {
        let change = KeywordChange::between(&Keywords::of(&["a", "b"]), &Keywords::of(&["b", "c"]));

        assert_eq!(change.added, Keywords::of(&["c"]));
        assert_eq!(change.removed, Keywords::of(&["a"]));
        assert_eq!(
            change.apply(&Keywords::of(&["a", "b"])),
            Keywords::of(&["b", "c"])
        );
    }

    #[test]
    fn test_changes_combine() {
        let first = KeywordChange {
            added: Keywords::of(&["a"]),
            removed: Keywords::of(&["b"]),
        };
        let next = KeywordChange {
            added: Keywords::of(&["b"]),
            removed: Keywords::of(&["c"]),
        };

        let combined = first.then(&next);

        assert_eq!(combined.added, Keywords::of(&["a", "b"]));
        assert_eq!(combined.removed, Keywords::of(&["c"]));
    }
}
