use std::{
    fmt::{Debug, Display},
    process,
    str::FromStr,
    sync::LazyLock,
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

static PID_AND_HOSTNAME: LazyLock<(u32, String)> = LazyLock::new(|| {
    (
        process::id(),
        uname().nodename().to_string_lossy().into_owned(),
    )
});

#[derive(PartialEq, Clone)]
pub struct LocalMail {
    metadata: NewLocalMailMetadata,
    // todo: consider streaming this
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

fn parse_flags(flags: &str) -> (BitFlags<Flag>, String) {
    let mut known = BitFlags::empty();
    let mut extra: Vec<char> = Vec::new();
    for flag in flags.chars() {
        if let Ok(flag) = Flag::try_from(flag) {
            known |= flag;
        } else {
            extra.push(flag);
        }
    }
    extra.sort_unstable();
    extra.dedup();

    (known, extra.into_iter().collect())
}

fn format_flags(flags: BitFlags<Flag>, extra_flags: &str) -> String {
    let mut chars: Vec<char> = flags
        .iter()
        .map(char::from)
        .chain(extra_flags.chars())
        .collect();
    chars.sort_unstable();

    chars.into_iter().collect()
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct NewLocalMailMetadata {
    flags: BitFlags<Flag>,
    extra_flags: String,
    fileprefix: String,
}

impl NewLocalMailMetadata {
    pub fn new(flags: BitFlags<Flag>, fileprefix: String) -> Self {
        Self {
            flags,
            extra_flags: String::new(),
            fileprefix,
        }
    }
}

impl From<LocalMailMetadata> for NewLocalMailMetadata {
    fn from(value: LocalMailMetadata) -> Self {
        Self {
            flags: value.flags,
            extra_flags: value.extra_flags,
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
            extra_flags: self.extra_flags,
            fileprefix: self.fileprefix,
        }
    }

    fn flags(&self) -> BitFlags<Flag> {
        self.flags
    }

    fn set_flags(&mut self, flags: BitFlags<Flag>) {
        self.flags = flags;
    }
}

impl Display for NewLocalMailMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string_flags = format_flags(self.flags, &self.extra_flags);
        write!(f, "{}:2,{string_flags}", self.fileprefix)
    }
}

impl FromStr for NewLocalMailMetadata {
    type Err = ParseLocalMailMetadataError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((head, flags)) = s.rsplit_once(":2,") {
            let (flags, extra_flags) = parse_flags(flags);
            Ok(Self {
                flags,
                extra_flags,
                fileprefix: head.into(),
            })
        } else {
            Err(ParseLocalMailMetadataError {
                message: "filename should contain :2,",
            })
        }
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LocalMailMetadata {
    uid: Uid,
    // todo: add modseq to handle highest_modseq transactional
    flags: BitFlags<Flag>,
    extra_flags: String,
    fileprefix: String,
}

impl LocalMailMetadata {
    pub fn new(uid: Uid, flags: BitFlags<Flag>, fileprefix: Option<String>) -> Self {
        let fileprefix = fileprefix.unwrap_or_else(Self::generate_file_prefix);

        Self {
            uid,
            flags,
            extra_flags: String::new(),
            fileprefix,
        }
    }

    #[must_use]
    pub fn with_extra_flags(mut self, extra_flags: String) -> Self {
        self.extra_flags = extra_flags;

        self
    }

    pub fn fileprefix(&self) -> &str {
        &self.fileprefix
    }

    pub fn extra_flags(&self) -> &str {
        &self.extra_flags
    }

    fn generate_file_prefix() -> String {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("should be able to get unix time");
        let secs = time.as_secs();
        let nanos = time.subsec_nanos();
        let (pid, hostname) = &*PID_AND_HOSTNAME;
        format!("{secs}.P{pid}N{nanos}.{hostname}")
    }

    pub fn uid(&self) -> Uid {
        self.uid
    }

    fn string_flags(&self) -> String {
        format_flags(self.flags, &self.extra_flags)
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
}

impl Display for LocalMailMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string_flags = self.string_flags();
        write!(f, "{},U={}:2,{string_flags}", self.fileprefix, self.uid)
    }
}

#[derive(Debug, Error)]
#[error("Missing mail {message}")]
pub struct ParseLocalMailMetadataError {
    message: &'static str,
}

impl FromStr for LocalMailMetadata {
    type Err = ParseLocalMailMetadataError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (head, flags) = s.rsplit_once(":2,").ok_or(ParseLocalMailMetadataError {
            message: "filename should contain :2,",
        })?;
        let (flags, extra_flags) = parse_flags(flags);
        if let Some((fileprefix, uid)) = head.rsplit_once(",U=")
            && let Ok(uid) = u32::from_str(uid)
            && let Ok(uid) = uid.try_into()
        {
            Ok(Self {
                uid,
                flags,
                extra_flags,
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
        assert_eq!(metadata.string_flags(), "DFRST");
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
    fn test_metadata_keeps_unknown_flags(
        #[values("prefix,U=3:2,PSa", "prefix,U=3:2,aSP")] filename: &str,
    ) {
        let metadata = assert_ok!(LocalMailMetadata::from_str(filename));
        assert_eq!(metadata.flags(), Flag::Seen);
        assert_eq!(metadata.extra_flags(), "Pa");
        assert_eq!(metadata.filename(), "prefix,U=3:2,PSa");
    }

    #[rstest]
    fn test_new_metadata_keeps_unknown_flags() {
        let mut metadata = assert_ok!(NewLocalMailMetadata::from_str("foo:2,PS"));
        assert_eq!(metadata.flags(), Flag::Seen);
        metadata.set_flags(Flag::Seen | Flag::Answered);
        assert_eq!(metadata.filename(), "foo:2,PRS");
        let tracked = metadata.set_uid(assert_ok!(Uid::try_from(4)));
        assert_eq!(tracked.extra_flags(), "P");
        assert_eq!(tracked.filename(), "foo,U=4:2,PRS");
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
