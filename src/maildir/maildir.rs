use std::{
    fmt::Debug,
    fs::{self, DirBuilder, OpenOptions, read_dir, remove_file},
    io::Write,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use enumflags2::BitFlags;
use log::{info, trace, warn};
use thiserror::Error;

use crate::{
    maildir::maildir_repository::LocalMailMetadata,
    repository::{RemoteMail, Uid},
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
    pub fn store(&self, mail: &RemoteMail) -> LocalMailMetadata {
        let new_local_metadata =
            LocalMailMetadata::new(Some(mail.metadata().uid()), mail.metadata().flags(), None);
        let file_path = self.tmp.join(new_local_metadata.fileprefix());

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

        fs::rename(file_path, self.cur.join(new_local_metadata.filename()))
            .expect("moving file from tmp to cur should succeed");

        new_local_metadata
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

    pub fn read(&self, metadata: LocalMailMetadata) -> LocalMail {
        LocalMail::new(
            fs::read(self.cur.join(metadata.filename())).expect("mail contents should be readable"),
            metadata,
        )
    }

    fn rename(current: &Path, new: &Path) {
        match (
            current
                .try_exists()
                .expect("should be able to check if current name exists"),
            new.try_exists()
                .expect("should be able to check if new name exists"),
        ) {
            (true, true) => todo!(
                "updating {} to {} failed, because both files already exist",
                current.to_string_lossy(),
                new.to_string_lossy()
            ),
            (true, false) => {
                trace!("renaming {:} to {:}", current.display(), new.display());
                fs::rename(current, new).expect("renaming mail in maildir should succeed");
            }
            (false, true) => warn!(
                "ignoring rename of {} to {}, because old file does not exist while new one does. May be due to prior crash",
                current.to_string_lossy(),
                new.to_string_lossy()
            ),
            (false, false) => todo!(
                "Cannot rename {}, because it does not exist",
                current.to_string_lossy()
            ),
        }
    }

    pub fn update_uid(&self, entry: &mut LocalMailMetadata, new_uid: Uid) {
        let current_mail = self.cur.join(entry.filename());
        entry.set_uid(new_uid);
        let new_name = self.cur.join(entry.filename());
        Self::rename(&current_mail, &new_name);
    }

    pub fn update_flags(&self, entry: &mut LocalMailMetadata, new_flags: BitFlags<Flag>) {
        let current_mail = self.cur.join(entry.filename());
        entry.set_flags(new_flags);
        let new_name = self.cur.join(entry.filename());
        Self::rename(&current_mail, &new_name);
    }

    pub fn delete(&self, entry: &LocalMailMetadata) {
        let file_path = self.cur.join(entry.filename());
        trace!("deleting {}", file_path.display());
        match remove_file(&file_path) {
            Ok(()) => {}
            Err(e) => {
                if let std::io::ErrorKind::NotFound = e.kind() {
                    trace!("{} already gone", &file_path.display());
                } else {
                    todo!("handle deletion error {e:?}")
                }
            }
        }
    }
}

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
