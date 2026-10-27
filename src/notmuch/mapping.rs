use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::{
    imap::{decode_modified_utf7, encode_modified_utf7},
    repository::{KeywordSource, Keywords},
};

/// Tags notmuch manages itself, e.g. from maildir flags or while indexing
const DEFAULT_IGNORED_TAGS: [&str; 10] = [
    "unread",
    "replied",
    "flagged",
    "draft",
    "passed",
    "deleted",
    "new",
    "attachment",
    "signed",
    "encrypted",
];

/// Gmail's system labels with tag equivalents. `\Starred` and `\Draft` are synced as flags.
/// `\Trash` and `\Spam` move mails out of the synced mailbox of all mails, so that tagging
/// them would delete mails.
const GMAIL_SYSTEM_LABELS: [(&str, &str); 3] = [
    ("\\Inbox", "inbox"),
    ("\\Important", "important"),
    ("\\Sent", "sent"),
];

/// Longest tag notmuch stores (`NOTMUCH_TAG_MAX`)
const MAX_TAG_LEN: usize = 200;

/// Tag added by `notmuch new` to every mail, which is no keyword except on Gmail
const INBOX_TAG: &str = "inbox";

/// Configuration of the synced tags
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagConfig {
    /// Tags, that are neither synced to nor from the server
    pub ignored_tags: Vec<String>,
    /// Tags of keywords, that are not named like them, e.g. `$Junk` = `spam`
    pub tags: BTreeMap<String, String>,
}

impl Default for TagConfig {
    fn default() -> Self {
        Self {
            ignored_tags: DEFAULT_IGNORED_TAGS.map(str::to_string).to_vec(),
            tags: BTreeMap::new(),
        }
    }
}

/// Maps keywords of a server to notmuch tags and back. Tags without keyword are not synced.
#[derive(Debug, Clone)]
pub struct TagMapping {
    source: KeywordSource,
    ignored: HashSet<String>,
    to_tag: HashMap<String, String>,
    to_keyword: HashMap<String, String>,
}

impl TagMapping {
    pub fn new(config: &TagConfig, source: KeywordSource) -> Self {
        let mut ignored: HashSet<String> = config.ignored_tags.iter().cloned().collect();
        let mut to_tag: HashMap<String, String> = HashMap::new();
        let mut to_keyword: HashMap<String, String> = HashMap::new();
        match source {
            KeywordSource::Flags => {
                ignored.insert(INBOX_TAG.to_string());
            }
            KeywordSource::GmailLabels => {
                for (label, tag) in GMAIL_SYSTEM_LABELS {
                    if config.tags.contains_key(label) {
                        // e.g. `inbox` of `notmuch new` would sync as a second tag of the label,
                        // unless configured for another keyword
                        if !config.tags.values().any(|configured| configured == tag) {
                            ignored.insert(tag.to_string());
                        }
                    } else {
                        to_tag.insert(label.to_string(), tag.to_string());
                        to_keyword.insert(tag.to_string(), label.to_string());
                    }
                }
            }
        }
        // a tag of several configured keywords stays with the first one
        let mut configured = HashSet::new();
        for (keyword, tag) in &config.tags {
            to_tag.insert(keyword.clone(), tag.clone());
            if configured.insert(tag) {
                to_keyword.insert(tag.clone(), keyword.clone());
            }
        }

        Self {
            source,
            ignored,
            to_tag,
            to_keyword,
        }
    }

    /// Identifies the mapping, whose changes require syncing all tags again
    pub fn fingerprint(&self) -> String {
        let mut ignored: Vec<&String> = self.ignored.iter().collect();
        ignored.sort();
        let mut tags: Vec<_> = self.to_tag.iter().collect();
        tags.sort();
        let mut keywords: Vec<_> = self.to_keyword.iter().collect();
        keywords.sort();
        format!(
            "{}\n{ignored:?}\n{tags:?}\n{keywords:?}",
            self.source.as_str()
        )
    }

    /// Tag of `keyword`, if it is synced
    pub fn tag(&self, keyword: &str) -> Option<String> {
        let tag = if let Some(tag) = self.to_tag.get(keyword) {
            tag.clone()
        } else {
            match self.source {
                KeywordSource::Flags => keyword.to_string(),
                KeywordSource::GmailLabels if keyword.starts_with('\\') => return None,
                KeywordSource::GmailLabels => {
                    // labels, that do not encode back the same, would change when synced back
                    let decoded = decode_modified_utf7(keyword)?;
                    if encode_modified_utf7(&decoded) != keyword {
                        return None;
                    }
                    decoded
                }
            }
        };
        if self
            .to_keyword
            .get(&tag)
            .is_some_and(|mapped| mapped != keyword)
        {
            // the tag belongs to another keyword
            return None;
        }

        // notmuch cannot store NUL, which e.g. `&AAA-` decodes to, failing all tag writes
        (!tag.is_empty()
            && tag.len() <= MAX_TAG_LEN
            && !tag.contains(char::is_control)
            && !self.ignored.contains(&tag))
        .then_some(tag)
    }

    /// Keyword of `tag`, if it is synced
    pub fn keyword(&self, tag: &str) -> Option<String> {
        if self.ignored.contains(tag) {
            return None;
        }
        if let Some(keyword) = self.to_keyword.get(tag) {
            return Some(keyword.clone());
        }
        let keyword = match self.source {
            KeywordSource::Flags => tag.to_string(),
            KeywordSource::GmailLabels if tag.starts_with('\\') => return None,
            KeywordSource::GmailLabels => encode_modified_utf7(tag),
        };
        if !Keywords::is_storable(&keyword, self.source) {
            return None;
        }

        // e.g. a keyword with another tag must not get a second one
        (self.tag(&keyword).as_deref() == Some(tag)).then_some(keyword)
    }

    pub fn is_synced(&self, tag: &str) -> bool {
        self.keyword(tag).is_some()
    }

    /// Tags of the synced `keywords`
    pub fn tags(&self, keywords: &Keywords) -> BTreeSet<String> {
        keywords
            .iter()
            .filter_map(|keyword| self.tag(keyword))
            .collect()
    }

    /// Keywords of the synced `tags`
    pub fn keywords<'a>(&self, tags: impl IntoIterator<Item = &'a str>) -> Keywords {
        tags.into_iter()
            .filter_map(|tag| self.keyword(tag))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;
    use rstest::*;

    use super::*;
    use crate::notmuch::test_support::set;

    fn mapping(source: KeywordSource, tags: &[(&str, &str)]) -> TagMapping {
        TagMapping::new(
            &TagConfig {
                tags: tags
                    .iter()
                    .map(|(keyword, tag)| ((*keyword).to_string(), (*tag).to_string()))
                    .collect(),
                ..TagConfig::default()
            },
            source,
        )
    }

    #[rstest]
    #[case("work", Some("work"))]
    #[case("$Junk", Some("spam"))]
    #[case("unread", None)]
    #[case("inbox", None)]
    #[case("spam", None)]
    fn test_keywords_map_to_tags(#[case] keyword: &str, #[case] tag: Option<&str>) {
        let mapping = mapping(KeywordSource::Flags, &[("$Junk", "spam")]);

        assert_eq!(mapping.tag(keyword).as_deref(), tag);
    }

    #[rstest]
    #[case("work", Some("work"))]
    #[case("spam", Some("$Junk"))]
    #[case("$Junk", None)]
    #[case("unread", None)]
    #[case("inbox", None)]
    #[case("two words", None)]
    fn test_tags_map_to_keywords(#[case] tag: &str, #[case] keyword: Option<&str>) {
        let mapping = mapping(KeywordSource::Flags, &[("$Junk", "spam")]);

        assert_eq!(mapping.keyword(tag).as_deref(), keyword);
    }

    #[rstest]
    #[case("\\Inbox", Some("inbox"))]
    #[case("\\Spam", None)]
    #[case("a&b", None)]
    #[case("\\Starred", None)]
    #[case("my label", Some("my label"))]
    #[case("&AOQ-pfel", Some("äpfel"))]
    #[case("&AAA-", None)]
    #[case("a&AAo-b", None)]
    fn test_gmail_labels_map_to_tags(#[case] label: &str, #[case] tag: Option<&str>) {
        let mapping = mapping(KeywordSource::GmailLabels, &[]);

        assert_eq!(mapping.tag(label).as_deref(), tag);
    }

    #[rstest]
    #[case("inbox", Some("\\Inbox"))]
    #[case("my label", Some("my label"))]
    #[case("äpfel", Some("&AOQ-pfel"))]
    #[case("\\Starred", None)]
    #[case("unread", None)]
    fn test_tags_map_to_gmail_labels(#[case] tag: &str, #[case] label: Option<&str>) {
        let mapping = mapping(KeywordSource::GmailLabels, &[]);

        assert_eq!(mapping.keyword(tag).as_deref(), label);
    }

    #[test]
    fn test_overlong_tags_are_not_synced() {
        let mapping = mapping(KeywordSource::Flags, &[]);
        let long = "a".repeat(MAX_TAG_LEN + 1);

        assert_none!(mapping.tag(&long));
        assert_none!(mapping.keyword(&long));
    }

    #[test]
    fn test_label_decoding_to_configured_tag_is_not_synced() {
        let mapping = mapping(KeywordSource::GmailLabels, &[("Obst", "äpfel")]);

        assert_none!(mapping.tag("&AOQ-pfel"));
        assert_eq!(mapping.keyword("äpfel").as_deref(), Some("Obst"));
    }

    #[test]
    fn test_fingerprint_changes_with_configuration() {
        let plain = mapping(KeywordSource::Flags, &[]);
        let renamed = mapping(KeywordSource::Flags, &[("$Junk", "spam")]);

        assert_eq!(
            plain.fingerprint(),
            mapping(KeywordSource::Flags, &[]).fingerprint()
        );
        assert_ne!(plain.fingerprint(), renamed.fingerprint());
        assert_ne!(
            plain.fingerprint(),
            mapping(KeywordSource::GmailLabels, &[]).fingerprint()
        );
    }

    #[test]
    fn test_configured_tag_overrides_gmail_system_label() {
        let mapping = mapping(KeywordSource::GmailLabels, &[("Inbox-Archive", "inbox")]);

        assert_eq!(mapping.keyword("inbox").as_deref(), Some("Inbox-Archive"));
        assert_none!(mapping.tag("\\Inbox"));
        assert_eq!(mapping.tag("Inbox-Archive").as_deref(), Some("inbox"));
    }

    #[rstest]
    #[case("\\Inbox", "inbox")]
    #[case("\\Important", "important")]
    #[case("\\Sent", "sent")]
    fn test_configured_gmail_system_label_drops_default_tag(
        #[case] label: &str,
        #[case] default: &str,
    ) {
        let mapping = mapping(KeywordSource::GmailLabels, &[(label, "renamed")]);

        assert_eq!(mapping.tag(label).as_deref(), Some("renamed"));
        assert_eq!(mapping.keyword("renamed").as_deref(), Some(label));
        assert_none!(mapping.keyword(default));
    }

    #[test]
    fn test_default_tag_of_configured_gmail_system_label_can_map_another_label() {
        let mapping = mapping(
            KeywordSource::GmailLabels,
            &[("\\Inbox", "in"), ("Inbox-Archive", "inbox")],
        );

        assert_eq!(mapping.keyword("inbox").as_deref(), Some("Inbox-Archive"));
        assert_eq!(mapping.keyword("in").as_deref(), Some("\\Inbox"));
        assert_eq!(mapping.tag("\\Inbox").as_deref(), Some("in"));
    }

    #[test]
    fn test_ignored_tags_are_not_synced() {
        let mapping = TagMapping::new(
            &TagConfig {
                ignored_tags: vec!["todo".to_string()],
                tags: BTreeMap::new(),
            },
            KeywordSource::Flags,
        );

        assert_none!(mapping.keyword("todo"));
        assert_none!(mapping.tag("todo"));
        assert_eq!(mapping.keyword("unread").as_deref(), Some("unread"));
    }

    #[test]
    fn test_keyword_sets_map_both_ways() {
        let mapping = mapping(KeywordSource::Flags, &[("$Junk", "spam")]);
        let keywords = Keywords::of(&["$Junk", "work", "$Forwarded"]);

        let tags = mapping.tags(&keywords);

        assert_eq!(tags, set(&["$Forwarded", "spam", "work"]));
        assert_eq!(mapping.keywords(tags.iter().map(String::as_str)), keywords);
    }
}
