use std::{
    fmt::{Debug, Display},
    process,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use enumflags2::BitFlags;
use rustix::system::uname;

use crate::repository::{Flag, Uid};

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

// todo: check if Arc would cover Clone use cases
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LocalMailMetadata {
    // todo: different struct for new local mail that has no uid yet
    uid: Option<Uid>,
    // todo: add modseq to handle highest_modseq transactional
    flags: BitFlags<Flag>,
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
        let mut string_flags = String::with_capacity(6);
        for flag in self.flags {
            if let Ok(char_flag) = flag.try_into() {
                string_flags.push(char_flag);
            }
        }

        string_flags
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

impl FromStr for LocalMailMetadata {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (head, flags) = s.rsplit_once(":2,").ok_or("filename should contain :2,")?;
        let flags = flags.chars().map(Flag::from).collect();
        if let Some((fileprefix, uid)) = head.rsplit_once(",U=") {
            let uid = uid
                .parse::<u32>()
                .map_err(|_| "uid field should be u32")?
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_maildir_flags_in_ascii_order() {
        let flags = BitFlags::all();
        let metadata = LocalMailMetadata::new(Uid::try_from(&3).ok(), flags, None);

        assert_eq!(metadata.string_flags(), "DFRST");
    }
}
