use std::{
    fmt::Debug,
    fs::{self, DirBuilder, OpenOptions, read_dir, remove_file},
    io::Write,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use enumflags2::BitFlags;
use log::{info, trace, warn};
use rustix::system::uname;
use thiserror::Error;

use crate::{
    imap::{RemoteMail, Uid},
    maildir::maildir_repository::LocalMailMetadata,
    sync::Flag,
};

#[derive(Debug)]
pub struct Maildir {
    new: PathBuf,
    cur: PathBuf,
    tmp: PathBuf,
}

impl Maildir {
    pub fn new(mail_dir: &Path, account: &str, mailbox: &str) -> Self {
        let mut maildir_path = mail_dir.join(account);
        maildir_path.push(mailbox);

        if Maildir::load(mail_dir, account, mailbox).is_ok() {
            panic!(
                "unmanaged maildir found at {}",
                maildir_path.to_string_lossy()
            );
        } else {
            info!("creating mailbox in {:#}", maildir_path.display());
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(0o700);

            let tmp = maildir_path.join("tmp");
            builder
                .create(tmp.as_path())
                .expect("creation of tmp subdir should succeed");
            let new = maildir_path.join("new");
            builder
                .create(new.as_path())
                .expect("creation of new subdir should succeed");
            let cur = maildir_path.join("cur");
            builder
                .create(cur.as_path())
                .expect("creation of cur subdir should succeed");

            Self { new, cur, tmp }
        }
    }

    fn unchecked(mail_dir: &Path, account: &str, mailbox: &str) -> Self {
        let mut maildir_path = mail_dir.join(account);
        maildir_path.push(mailbox);
        let new = maildir_path.join("new");
        let cur = maildir_path.join("cur");
        let tmp = maildir_path.join("tmp");
        Self { new, cur, tmp }
    }

    pub fn load(mail_dir: &Path, account: &str, mailbox: &str) -> Result<Self> {
        let mail = Self::unchecked(mail_dir, account, mailbox);
        trace!("loading maildir {mail:?}");
        match (
            mail.new.try_exists(),
            mail.cur.try_exists(),
            mail.tmp.try_exists(),
        ) {
            (Ok(true), Ok(true), Ok(true)) => Ok(mail),
            (Ok(false), Ok(false), Ok(false)) => Err(anyhow!("no mailbox present")),
            (Ok(_), Ok(_), Ok(_)) => panic!(
                "partially initialized maildir detected: {}/{account}/{mailbox}",
                mail_dir.to_string_lossy()
            ),
            (_, _, _) => panic!("issue with reading {}", mail_dir.to_string_lossy()),
        }
    }

    // Algorithm
    // Technically the program should chdir into maildir_root to prevent issues if the path of
    // maildir_root changes. Setting current_dir is a process wide operation though and will mess
    // up relative file operations in the spawn_blocking threads.
    pub fn store(&self, mail: &RemoteMail) -> String {
        let file_prefix = Self::generate_file_prefix();
        let file_path = self.tmp.join(&file_prefix);

        trace!("writing to {}", file_path.display());
        let Ok(mut file) = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file_path)
        else {
            todo!("handle tmp file creation errors");
        };

        file.write_all(mail.content())
            .expect("writing new mail to tmp should succeed");
        file.sync_all()
            .expect("writing new tmp mail to disc should succeed");

        fs::rename(
            file_path,
            self.cur.join(Self::generate_filename(
                &file_prefix,
                mail.metadata().uid(),
                mail.metadata().flags(),
            )),
        )
        .expect("moving file from tmp to cur should succeed");

        file_prefix
    }

    // todo: move this to LocalMailMetadata
    fn generate_filename(file_prefix: &str, uid: Option<Uid>, flags: BitFlags<Flag>) -> String {
        let mut string_flags = String::with_capacity(6);
        for flag in flags {
            if let Ok(char_flag) = flag.try_into() {
                string_flags.push(char_flag);
            }
        }
        if let Some(uid) = uid {
            format!("{file_prefix},U={uid}:2,{string_flags}")
        } else {
            format!("{file_prefix}:2,{string_flags}")
        }
    }

    pub fn resolve(&self, filename: &str) -> PathBuf {
        self.cur.join(filename)
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

    pub fn list_cur(&self) -> impl Iterator<Item = LocalMailMetadata> {
        read_dir(self.cur.as_path())
            .expect("cur should be readable")
            .map(|entry| {
                let filename = entry
                    .expect("entry of cur should be readable")
                    .file_name()
                    .into_string()
                    .expect("converting filename from OsString to String should be possible");
                filename.parse().expect("filename should be parsable")
            })
    }

    pub fn update(&self, entry: &LocalMailMetadata, new_flags: BitFlags<Flag>) {
        let current_mail = self.cur.join(Self::generate_filename(
            entry.fileprefix(),
            entry.uid(),
            entry.flags(),
        ));
        let new_name = self.cur.join(Self::generate_filename(
            entry.fileprefix(),
            entry.uid(),
            new_flags,
        ));
        match (
            current_mail
                .try_exists()
                .expect("should be able to check if current_mail exists"),
            new_name
                .try_exists()
                .expect("should be able to check if updated flag name exists"),
        ) {
            (true, true) => todo!(
                "updating {} to {} failed, because both files already exist",
                current_mail.to_string_lossy(),
                new_name.to_string_lossy()
            ),
            (true, false) => {
                trace!("updating flags of {:}", current_mail.display());
                fs::rename(current_mail, new_name)
                    .expect("updating flags in maildir should succeed");
            }
            (false, true) => warn!(
                "ignoring update of {} to {}, because old file does not exist while new one does. May be due to prior crash",
                current_mail.to_string_lossy(),
                new_name.to_string_lossy()
            ),
            (false, false) => todo!(
                "Cannot update flags of {}, because it does not exist",
                current_mail.to_string_lossy()
            ),
        }
    }

    pub fn delete(&self, entry: &LocalMailMetadata) {
        let filename = Self::generate_filename(entry.fileprefix(), entry.uid(), entry.flags());
        let file_path = self.cur.join(filename);
        trace!("deleting {}", file_path.display());
        remove_file(file_path).expect("deletion of file should succeed");
    }
}

pub struct LocalMail {
    metadata: LocalMailMetadata,
    content: Vec<u8>,
}

impl LocalMail {
    pub fn new(content: Vec<u8>, metadata: LocalMailMetadata) -> Self {
        Self { metadata, content }
    }

    fn metadata(&self) -> &LocalMailMetadata {
        &self.metadata
    }

    fn content(&self) -> &[u8] {
        &self.content
    }
}

impl Debug for LocalMail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalMail")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl From<char> for Flag {
    fn from(value: char) -> Self {
        match value {
            'D' => Flag::Draft,
            'F' => Flag::Flagged,
            'R' => Flag::Answered,
            'S' => Flag::Seen,
            'T' => Flag::Deleted,
            _ => panic!("unknown flag"),
        }
    }
}

#[derive(Error, Debug)]
#[error("Unknown Maildir flag")]
pub struct UnknownMaildirFlagError {}

impl TryFrom<Flag> for char {
    type Error = UnknownMaildirFlagError;

    fn try_from(value: Flag) -> Result<Self, Self::Error> {
        match value {
            Flag::Seen => Ok('S'),
            Flag::Answered => Ok('R'),
            Flag::Flagged => Ok('F'),
            Flag::Deleted => Ok('T'),
            Flag::Draft => Ok('D'),
            Flag::Recent => Err(UnknownMaildirFlagError {}),
        }
    }
}
