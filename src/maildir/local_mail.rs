use std::{
    fmt::{Debug, Display},
    process,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use enumflags2::BitFlags;
use rustix::system::uname;
use thiserror::Error;

use crate::{
    imap::RemoteMailMetadata,
    repository::{Flag, Uid},
};

#[derive(PartialEq, Clone)]
pub struct LocalMail {
    metadata: LocalMailMetadata,
    // todo: consider streaming this
    content: Vec<u8>,
}

impl LocalMail {
    pub fn new(content: Vec<u8>, metadata: LocalMailMetadata) -> Self {
        Self { metadata, content }
    }

    pub fn metadata(&self) -> &LocalMailMetadata {
        &self.metadata
    }

    pub fn unpack(self) -> (LocalMailMetadata, Vec<u8>) {
        (self.metadata, self.content)
    }
}

impl Debug for LocalMail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalMail")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LocalMailMetadata {
    // todo: different struct for new local mail that has no uid yet
    uid: Option<Uid>,
    // todo: add modseq to handle highest_modseq transactional
    flags: BitFlags<Flag>,
    // todo: Cow?
    fileprefix: String,
}

impl LocalMailMetadata {
    pub fn new(uid: Option<Uid>, flags: BitFlags<Flag>, fileprefix: Option<String>) -> Self {
        let fileprefix = fileprefix.unwrap_or_else(Self::generate_file_prefix);

        Self {
            uid,
            flags,
            fileprefix,
        }
    }

    // todo: consider allowing custom prefix/name for user provided mails in maildir
    pub fn fileprefix(&self) -> &str {
        &self.fileprefix
    }

    pub fn filename(&self) -> String {
        self.to_string()
    }

    pub fn uid(&self) -> Option<Uid> {
        self.uid
    }

    pub fn set_uid(&mut self, uid: Uid) {
        self.uid = Some(uid);
    }

    pub fn flags(&self) -> BitFlags<Flag> {
        self.flags
    }

    pub fn set_flags(&mut self, flags: BitFlags<Flag>) {
        self.flags = flags;
    }

    fn generate_file_prefix() -> String {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("should be able to get unix time");
        let secs = time.as_secs();
        let nanos = time.subsec_nanos();
        let hostname = uname();
        let hostname = hostname.nodename().to_string_lossy();
        let pid = process::id();
        format!("{secs}.P{pid}N{nanos}.{hostname}")
    }

    fn string_flags(&self) -> String {
        self.flags().iter().map(char::from).collect()
    }
}

impl Display for LocalMailMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string_flags = self.string_flags();
        if let Some(uid) = self.uid {
            write!(f, "{},U={uid}:2,{string_flags}", self.fileprefix)
        } else {
            write!(f, "{}:2,{string_flags}", self.fileprefix)
        }
    }
}

#[derive(Debug, Error)]
#[error("Missing mail {message}")]
pub struct ParseLocalMailMetadataError {
    message: &'static str,
}

impl ParseLocalMailMetadataError {
    pub fn message(&self) -> &'static str {
        self.message
    }
}

impl FromStr for LocalMailMetadata {
    type Err = ParseLocalMailMetadataError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (head, flags) = s.rsplit_once(":2,").ok_or(ParseLocalMailMetadataError {
            message: "filename should contain :2,",
        })?;
        let flags: Result<BitFlags<Flag>, ParseLocalMailMetadataError> = flags
            .chars()
            .map(|flag| {
                Flag::try_from(flag).map_err(|_| ParseLocalMailMetadataError {
                    message: "char flag should be parsable",
                })
            })
            .collect();
        let flags = flags?;
        if let Some((fileprefix, uid)) = head.rsplit_once(",U=") {
            let uid = uid
                .parse::<u32>()
                .map_err(|_| ParseLocalMailMetadataError {
                    message: "uid field should be u32",
                })?
                .try_into()
                .ok();
            Ok(Self {
                uid,
                flags,
                fileprefix: fileprefix.to_string(),
            })
        } else {
            Ok(Self {
                uid: None,
                flags,
                fileprefix: head.to_string(),
            })
        }
    }
}

impl From<&RemoteMailMetadata> for LocalMailMetadata {
    fn from(value: &RemoteMailMetadata) -> Self {
        Self::new(Some(value.uid()), value.flags(), None)
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;
    use enumflags2::BitFlag;
    use rstest::*;

    use crate::repository::ModSeq;

    use super::*;

    #[fixture]
    fn prefix() -> String {
        "prefix".to_string()
    }

    #[fixture]
    fn metadata(prefix: String) -> LocalMailMetadata {
        let flags = BitFlags::all();
        LocalMailMetadata::new(Uid::try_from(&3).ok(), flags, Some(prefix))
    }

    #[fixture]
    fn content() -> Vec<u8> {
        "asdfjklsadfjklsadfj".into()
    }

    #[fixture]
    fn mail(metadata: LocalMailMetadata, content: Vec<u8>) -> LocalMail {
        LocalMail::new(content, metadata)
    }

    #[rstest]
    fn test_maildir_flags_in_ascii_order(metadata: LocalMailMetadata) {
        assert_eq!(metadata.string_flags(), "DFRST");
    }

    #[rstest]
    fn test_metadata_filename_is_correct(metadata: LocalMailMetadata) {
        let filename = metadata.filename();
        assert_eq!("prefix,U=3:2,DFRST", filename);
        assert_eq!(metadata, assert_ok!(filename.parse()));
    }

    #[rstest]
    fn test_metadata_filename_without_uid_is_correct(mut metadata: LocalMailMetadata) {
        metadata.uid = None;
        let filename = metadata.filename();
        assert_eq!("prefix:2,DFRST", filename);
        assert_eq!(metadata, assert_ok!(filename.parse()));
    }

    #[rstest]
    fn test_from_str_errors_on_invalid_filename(
        #[values("foo", "prefix:2,s", "prefix,U=R:2,")] filename: &str,
    ) {
        let result = assert_err!(LocalMailMetadata::from_str(filename));
        assert_matches!(result, ParseLocalMailMetadataError { .. });
    }

    #[rstest]
    fn test_generate_file_prefix_is_unique() {
        assert_ne!(
            LocalMailMetadata::generate_file_prefix(),
            LocalMailMetadata::generate_file_prefix()
        );
    }

    #[rstest]
    fn test_from_remote_mail_metadata_is_consistent() {
        let remote =
            RemoteMailMetadata::new(Uid::MAX, Flag::all(), assert_ok!(ModSeq::try_from(3)));
        let result = LocalMailMetadata::from(&remote);
        assert_eq!(remote.uid(), assert_some!(result.uid()));
        assert_eq!(remote.flags(), result.flags());
    }

    #[rstest]
    fn test_local_mail_unpacks(metadata: LocalMailMetadata) {
        let content: Vec<u8> = "foo".into();
        let expected_content = content.clone();
        let expected_metadata = metadata.clone();

        let mail = LocalMail::new(content, metadata);
        assert_eq!(&expected_metadata, mail.metadata());
        let (metadata, content) = mail.unpack();
        assert_eq!(expected_metadata, metadata);
        assert_eq!(expected_content, content);
    }

    #[rstest]
    fn test_local_mail_debug_format_does_not_include_content(mail: LocalMail, content: Vec<u8>) {
        assert_not_contains!(&format!("{mail:?}"), &format!("{content:?}"));
    }

    #[rstest]
    fn test_metadata_fileprefix_getter_is_ok(metadata: LocalMailMetadata, prefix: String) {
        assert_eq!(prefix, metadata.fileprefix());
    }

    #[rstest]
    fn test_metadata_set_uid_is_ok(mut metadata: LocalMailMetadata) {
        let expected = assert_ok!(Uid::try_from(4));
        assert_ne!(Some(expected), metadata.uid());
        metadata.set_uid(expected);
        assert_eq!(Some(expected), metadata.uid());
    }

    #[rstest]
    fn test_metadata_set_flags_is_ok(mut metadata: LocalMailMetadata) {
        let expected = Flag::Seen | Flag::Deleted;
        assert_ne!(expected, metadata.flags());
        metadata.set_flags(expected);
        assert_eq!(expected, metadata.flags());
    }

    #[rstest]
    fn test_parse_local_mail_metadata_error_returns_message() {
        let message = "aaosdfojsa";
        let e = ParseLocalMailMetadataError { message };

        assert_eq!(message, e.message());
    }
}
