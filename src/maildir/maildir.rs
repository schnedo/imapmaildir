use std::{
    fmt::Debug,
    fs::{self, DirBuilder, OpenOptions, read_dir, remove_file},
    io::Write,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use enumflags2::BitFlags;
use log::{debug, info, trace, warn};
use thiserror::Error;

use crate::{
    imap::RemoteMail,
    maildir::{LocalMail, LocalMailMetadata},
    repository::{Flag, Uid},
};

// todo: check if Arc covers clone use case
#[derive(Debug, Clone)]
pub struct Maildir {
    new: PathBuf,
    cur: PathBuf,
    tmp: PathBuf,
}

impl Maildir {
    pub fn new(mail_dir: &Path) -> Self {
        if Maildir::load(mail_dir).is_ok() {
            panic!("unmanaged maildir found at {}", mail_dir.to_string_lossy());
        } else {
            info!("creating maildir in {:#}", mail_dir.display());
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(0o700);

            let tmp = mail_dir.join("tmp");
            builder
                .create(tmp.as_path())
                .expect("creation of tmp subdir should succeed");
            let new = mail_dir.join("new");
            builder
                .create(new.as_path())
                .expect("creation of new subdir should succeed");
            let cur = mail_dir.join("cur");
            builder
                .create(cur.as_path())
                .expect("creation of cur subdir should succeed");

            Self { new, cur, tmp }
        }
    }

    fn unchecked(mail_dir: &Path) -> Self {
        let new = mail_dir.join("new");
        let cur = mail_dir.join("cur");
        let tmp = mail_dir.join("tmp");
        Self { new, cur, tmp }
    }

    pub fn load(mail_dir: &Path) -> Result<Self> {
        let mail = Self::unchecked(mail_dir);
        trace!("loading maildir {mail:?}");
        match (
            mail.new.try_exists(),
            mail.cur.try_exists(),
            mail.tmp.try_exists(),
        ) {
            (Ok(true), Ok(true), Ok(true)) => Ok(mail),
            (Ok(false), Ok(false), Ok(false)) => Err(anyhow!("no mailbox present")),
            (Ok(_), Ok(_), Ok(_)) => panic!(
                "partially initialized maildir detected: {}",
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

    fn rename(
        &self,
        current: LocalMailMetadata,
        new: &LocalMailMetadata,
    ) -> Result<(), UpdateMailError> {
        let current_path = self.cur.join(current.filename());
        let new_path = self.cur.join(new.filename());
        match (
            current_path
                .try_exists()
                .expect("should be able to check if current name exists"),
            new_path
                .try_exists()
                .expect("should be able to check if new name exists"),
        ) {
            (true, true) => {
                if Self::is_content_identical(current_path.as_path(), new_path.as_path()) {
                    fs::rename(current_path, new_path)
                        .expect("renaming mail in maildir should succeed");

                    Ok(())
                } else {
                    panic!(
                        "moving {} to {} would overwrite mail with different content",
                        current_path.display(),
                        new_path.display()
                    );
                }
            }
            (true, false) => {
                trace!(
                    "renaming {:} to {:}",
                    current_path.display(),
                    new_path.display()
                );
                fs::rename(current_path, new_path)
                    .expect("renaming mail in maildir should succeed");

                Ok(())
            }
            (false, true) => {
                warn!(
                    "ignoring rename of {} to {}, because old file does not exist while new one does. May be due to prior crash",
                    current_path.to_string_lossy(),
                    new_path.to_string_lossy()
                );

                Ok(())
            }
            (false, false) => Err(UpdateMailError::Missing(current)),
        }
    }

    fn is_content_identical(current: &Path, new: &Path) -> bool {
        trace!(
            "checking if content of {} and {} is identical",
            current.display(),
            new.display()
        );
        let current_content = fs::read(current).expect("current file should be readable");
        let new_content = fs::read(new).expect("new file should be readable");

        current_content == new_content
    }

    pub fn update_uid(
        &self,
        entry: &mut LocalMailMetadata,
        new_uid: Uid,
    ) -> Result<(), UpdateMailError> {
        let current_mail = entry.clone();
        entry.set_uid(new_uid);

        self.rename(current_mail, entry)
    }

    pub fn update_flags(
        &self,
        entry: &mut LocalMailMetadata,
        new_flags: BitFlags<Flag>,
    ) -> Result<(), UpdateMailError> {
        debug!(
            "updating mail {} flags: {} -> {}",
            entry.uid().map_or(String::new(), |uid| uid.to_string()),
            entry.flags(),
            new_flags
        );
        let current_mail = entry.clone();
        entry.set_flags(new_flags);

        self.rename(current_mail, entry)
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

#[derive(Debug, Error, PartialEq)]
pub enum UpdateMailError {
    #[error("Missing mail {0}")]
    Missing(LocalMailMetadata),
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

#[cfg(test)]
mod tests {
    use enumflags2::BitFlag;
    use rstest::{fixture, rstest};
    use tempfile::{TempDir, tempdir};

    use super::*;

    #[fixture]
    fn temp_dir() -> TempDir {
        tempdir().expect("temporary directory should be creatable")
    }

    #[rstest]
    fn test_update_flags_errors_on_missing_mail(temp_dir: TempDir) {
        let maildir = Maildir::new(temp_dir.path());
        let mut entry = LocalMailMetadata::new(
            Some(Uid::try_from(&2).expect("2 should be valid uid")),
            Flag::empty(),
            Some("prefix".to_string()),
        );
        let expected = entry.clone();

        let result = maildir.update_flags(&mut entry, Flag::all());

        assert_eq!(result, Err(UpdateMailError::Missing(expected)));
    }

    #[rstest]
    fn test_update_uid_errors_on_missing_mail(temp_dir: TempDir) {
        let maildir = Maildir::new(temp_dir.path());
        let mut entry = LocalMailMetadata::new(
            Some(Uid::try_from(&2).expect("2 should be valid uid")),
            Flag::empty(),
            Some("prefix".to_string()),
        );
        let expected = entry.clone();

        let result = maildir.update_uid(
            &mut entry,
            Uid::try_from(&3).expect("3 should be valid uid"),
        );

        assert_eq!(result, Err(UpdateMailError::Missing(expected)));
    }
}
