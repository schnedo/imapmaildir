use std::{
    fmt::{Debug, Display, Write as _},
    process,
    str::FromStr,
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use enumflags2::BitFlags;
use rustix::system::uname;
use thiserror::Error;

use crate::{
    imap::RemoteMailMetadata,
    maildir::maildir::MaildirFile,
    repository::{Flag, Uid},
};

/// Distinguishes file prefixes generated within the same nanosecond
static DELIVERY_COUNT: AtomicU64 = AtomicU64::new(0);

static PID_AND_HOSTNAME: LazyLock<(u32, String)> = LazyLock::new(|| {
    (
        process::id(),
        uname().nodename().to_string_lossy().into_owned(),
    )
});

#[derive(PartialEq, Clone)]
pub struct LocalMail {
    metadata: NewLocalMailMetadata,
    content: Vec<u8>,
}

impl LocalMail {
    pub fn new(content: Vec<u8>, metadata: NewLocalMailMetadata) -> Self {
        Self { metadata, content }
    }

    pub fn metadata(&self) -> &NewLocalMailMetadata {
        &self.metadata
    }

    pub fn content(&self) -> &[u8] {
        &self.content
    }

    pub fn unpack(self) -> (NewLocalMailMetadata, Vec<u8>) {
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
pub struct NewLocalMailMetadata {
    // todo: add modseq to handle highest_modseq transactional
    flags: BitFlags<Flag>,
    other_flags: OtherFlags,
    fileprefix: String,
}

impl NewLocalMailMetadata {
    pub fn new(flags: BitFlags<Flag>, fileprefix: String) -> Self {
        Self {
            flags,
            other_flags: OtherFlags::default(),
            fileprefix,
        }
    }
}

impl From<LocalMailMetadata> for NewLocalMailMetadata {
    fn from(value: LocalMailMetadata) -> Self {
        Self {
            flags: value.flags,
            other_flags: value.other_flags,
            fileprefix: value.fileprefix,
        }
    }
}

impl MaildirFile for NewLocalMailMetadata {
    fn filename(&self) -> String {
        self.to_string()
    }

    fn set_uid(self, uid: Uid) -> LocalMailMetadata {
        LocalMailMetadata {
            uid,
            flags: self.flags,
            other_flags: self.other_flags,
            fileprefix: self.fileprefix,
        }
    }

    fn flags(&self) -> BitFlags<Flag> {
        self.flags
    }

    fn set_flags(&mut self, flags: BitFlags<Flag>) {
        self.flags = flags;
    }

    fn other_flags(&self) -> &OtherFlags {
        &self.other_flags
    }

    fn set_other_flags(&mut self, other_flags: OtherFlags) {
        self.other_flags = other_flags;
    }

    fn prepend_to_fileprefix(&mut self, prefix: &str) {
        self.fileprefix.insert_str(0, prefix);
    }
}

impl Display for NewLocalMailMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:2,", self.fileprefix)?;
        write_flags(f, self.flags, &self.other_flags)
    }
}

impl FromStr for NewLocalMailMetadata {
    type Err = ParseLocalMailMetadataError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (head, info) = s.rsplit_once(":2,").ok_or(ParseLocalMailMetadataError {
            message: "filename should contain :2,",
        })?;
        let (flags, other_flags) = parse_flags(info);

        Ok(Self {
            flags,
            other_flags,
            fileprefix: head.into(),
        })
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LocalMailMetadata {
    uid: Uid,
    // todo: add modseq to handle highest_modseq transactional
    flags: BitFlags<Flag>,
    other_flags: OtherFlags,
    fileprefix: String,
}

impl LocalMailMetadata {
    pub fn new(uid: Uid, flags: BitFlags<Flag>, fileprefix: Option<String>) -> Self {
        let fileprefix = fileprefix.unwrap_or_else(Self::generate_file_prefix);

        Self {
            uid,
            flags,
            other_flags: OtherFlags::default(),
            fileprefix,
        }
    }

    pub fn fileprefix(&self) -> &str {
        &self.fileprefix
    }

    fn generate_file_prefix() -> String {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("should be able to get unix time");
        let secs = time.as_secs();
        let nanos = time.subsec_nanos();
        let (pid, hostname) = &*PID_AND_HOSTNAME;
        let count = DELIVERY_COUNT.fetch_add(1, Ordering::Relaxed);
        format!("{secs}.P{pid}N{nanos}Q{count}.{hostname}")
    }

    pub fn uid(&self) -> Uid {
        self.uid
    }

    pub fn has_flag(&self, flag: Flag) -> bool {
        self.flags.contains(flag)
    }
}

impl MaildirFile for LocalMailMetadata {
    fn filename(&self) -> String {
        self.to_string()
    }

    fn set_uid(mut self, uid: Uid) -> LocalMailMetadata {
        self.uid = uid;

        self
    }

    fn flags(&self) -> BitFlags<Flag> {
        self.flags
    }

    fn set_flags(&mut self, flags: BitFlags<Flag>) {
        self.flags = flags;
    }

    fn other_flags(&self) -> &OtherFlags {
        &self.other_flags
    }

    fn set_other_flags(&mut self, other_flags: OtherFlags) {
        self.other_flags = other_flags;
    }

    fn prepend_to_fileprefix(&mut self, prefix: &str) {
        self.fileprefix.insert_str(0, prefix);
    }
}

impl Display for LocalMailMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},U={}:2,", self.fileprefix, self.uid)?;
        write_flags(f, self.flags, &self.other_flags)
    }
}

/// Maildir flags without IMAP equivalent, e.g. `P` (passed) or keyword letters of other clients.
/// Kept in file names as is, but neither synced nor stored in the state.
#[derive(Debug, Default, Clone, Hash, Eq, PartialEq)]
pub struct OtherFlags(String);

impl OtherFlags {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for OtherFlags {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Splits the info part of a file name into known flags and other ones
fn parse_flags(info: &str) -> (BitFlags<Flag>, OtherFlags) {
    let mut flags = BitFlags::empty();
    let mut other = String::new();
    for flag in info.chars() {
        match Flag::try_from(flag) {
            Ok(flag) => flags |= flag,
            Err(_) => other.push(flag),
        }
    }

    (flags, OtherFlags(other))
}

/// Writes all flags in ASCII order as required by the maildir specification
fn write_flags(
    f: &mut std::fmt::Formatter<'_>,
    flags: BitFlags<Flag>,
    other_flags: &OtherFlags,
) -> std::fmt::Result {
    let mut all: Vec<char> = flags.iter().map(char::from).collect();
    all.extend(other_flags.0.chars());
    all.sort_unstable();
    all.dedup();
    all.into_iter().try_for_each(|flag| f.write_char(flag))
}

#[derive(Debug, Error)]
#[error("Missing mail {message}")]
pub struct ParseLocalMailMetadataError {
    message: &'static str,
}

impl FromStr for LocalMailMetadata {
    type Err = ParseLocalMailMetadataError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (head, info) = s.rsplit_once(":2,").ok_or(ParseLocalMailMetadataError {
            message: "filename should contain :2,",
        })?;
        let (flags, other_flags) = parse_flags(info);
        if let Some((fileprefix, uid)) = head.rsplit_once(",U=")
            && let Ok(uid) = u32::from_str(uid)
            && let Ok(uid) = uid.try_into()
        {
            Ok(Self {
                uid,
                flags,
                other_flags,
                fileprefix: fileprefix.to_string(),
            })
        } else {
            Err(ParseLocalMailMetadataError {
                message: "uid should be defined",
            })
        }
    }
}

impl From<&RemoteMailMetadata> for LocalMailMetadata {
    fn from(value: &RemoteMailMetadata) -> Self {
        Self::new(value.uid(), value.flags(), None)
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
        LocalMailMetadata::new(assert_ok!(Uid::try_from(&3)), flags, Some(prefix))
    }

    #[fixture]
    fn new_metadata(prefix: String) -> NewLocalMailMetadata {
        let flags = BitFlags::all();
        NewLocalMailMetadata::new(flags, prefix)
    }

    #[fixture]
    fn content() -> Vec<u8> {
        "asdfjklsadfjklsadfj".into()
    }

    #[fixture]
    fn mail(new_metadata: NewLocalMailMetadata, content: Vec<u8>) -> LocalMail {
        LocalMail::new(content, new_metadata)
    }

    #[rstest]
    fn test_maildir_flags_in_ascii_order(metadata: LocalMailMetadata) {
        assert_ends_with!(metadata.filename(), ":2,DFRST");
    }

    #[rstest]
    fn test_metadata_filename_is_correct(metadata: LocalMailMetadata) {
        let filename = metadata.filename();
        assert_eq!("prefix,U=3:2,DFRST", filename);
        assert_eq!(metadata, assert_ok!(filename.parse()));
    }

    #[rstest]
    fn test_metadata_filename_without_uid_is_correct(new_metadata: NewLocalMailMetadata) {
        let filename = new_metadata.filename();
        assert_eq!("prefix:2,DFRST", filename);
        assert_eq!(new_metadata, assert_ok!(filename.parse()));
    }

    #[rstest]
    fn test_metadata_from_str_errors_on_invalid_filename(
        #[values("foo", "prefix:2,s", "prefix,U=R:2,")] filename: &str,
    ) {
        let result = assert_err!(LocalMailMetadata::from_str(filename));
        assert_matches!(result, ParseLocalMailMetadataError { .. });
    }

    #[rstest]
    fn test_new_metadata_from_str_errors_without_info() {
        let result = assert_err!(NewLocalMailMetadata::from_str("foo"));
        assert_matches!(result, ParseLocalMailMetadataError { .. });
    }

    #[rstest]
    fn test_metadata_keeps_other_flags(#[values("p,U=5:2,PS", "p,U=5:2,FPSab")] filename: &str) {
        let metadata = assert_ok!(LocalMailMetadata::from_str(filename));

        assert_eq!(metadata.uid(), assert_ok!(Uid::try_from(5)));
        assert_eq!(metadata.fileprefix(), "p");
        assert_contains!(metadata.flags(), Flag::Seen);
        assert_eq!(metadata.filename(), filename);
    }

    #[rstest]
    fn test_new_metadata_keeps_other_flags() {
        let mut metadata = assert_ok!(NewLocalMailMetadata::from_str("p:2,PSa"));
        assert_eq!(metadata.flags(), BitFlags::from_flag(Flag::Seen));

        metadata.set_flags(Flag::Flagged | Flag::Seen);
        assert_eq!(metadata.filename(), "p:2,FPSa");
        let metadata = metadata.set_uid(assert_ok!(Uid::try_from(7)));
        assert_eq!(metadata.filename(), "p,U=7:2,FPSa");
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
        assert_eq!(remote.uid(), result.uid());
        assert_eq!(remote.flags(), result.flags());
    }

    #[rstest]
    fn test_local_mail_unpacks(new_metadata: NewLocalMailMetadata) {
        let content: Vec<u8> = "foo".into();
        let expected_content = content.clone();
        let expected_metadata = new_metadata.clone();

        let mail = LocalMail::new(content, new_metadata);
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
    fn test_metadata_set_uid_is_ok(metadata: LocalMailMetadata) {
        let expected = assert_ok!(Uid::try_from(4));
        assert_ne!(expected, metadata.uid());
        let metadata = metadata.set_uid(expected);
        assert_eq!(expected, metadata.uid());
    }

    #[rstest]
    fn test_metadata_set_flags_is_ok(mut metadata: LocalMailMetadata) {
        let expected = Flag::Seen | Flag::Deleted;
        assert_ne!(expected, metadata.flags());
        metadata.set_flags(expected);
        assert_eq!(expected, metadata.flags());
    }

    #[rstest]
    fn test_new_metadata_from_local_metadata_is_correct(metadata: LocalMailMetadata) {
        let expected_flags = metadata.flags();
        let expected_fileprefix = metadata.fileprefix.clone();

        let result = NewLocalMailMetadata::from(metadata);
        assert_eq!(expected_flags, result.flags());
        assert_eq!(expected_fileprefix, result.fileprefix);
    }

    #[rstest]
    fn test_new_metadata_set_uid_sets_uid(new_metadata: NewLocalMailMetadata) {
        let expected = assert_ok!(Uid::try_from(8));
        let metadata = new_metadata.set_uid(expected);

        assert_eq!(expected, metadata.uid());
    }

    #[rstest]
    fn test_new_metadata_set_flags_sets_flags(mut new_metadata: NewLocalMailMetadata) {
        let expected = Flag::Seen | Flag::Deleted;
        assert_ne!(expected, new_metadata.flags());

        new_metadata.set_flags(expected);

        assert_eq!(expected, new_metadata.flags());
    }
}
