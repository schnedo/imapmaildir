use std::{
    fmt::Debug,
    fs::{self, DirBuilder, OpenOptions, read_dir, remove_file},
    io::{self, Write},
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
};

use enumflags2::BitFlags;
use log::{debug, info, trace, warn};
use thiserror::Error;

use crate::{
    imap::RemoteMail,
    maildir::{LocalMail, LocalMailMetadata, local_mail::ParseLocalMailMetadataError},
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
    pub fn try_new(mail_dir: &Path) -> Result<Self, MaildirCreationError<'_>> {
        match Self::load(mail_dir) {
            Ok(_) | Err(MaildirLoadError::Partial(_)) => {
                Err(MaildirCreationError::Exists(mail_dir))
            }
            Err(MaildirLoadError::Io(path, kind)) => Err(MaildirCreationError::Io(path, kind)),
            Err(_) => {
                info!("creating maildir in {:#}", mail_dir.display());
                let mut builder = DirBuilder::new();
                builder.recursive(true).mode(0o700);

                let tmp = mail_dir.join("tmp");
                let new = mail_dir.join("new");
                let cur = mail_dir.join("cur");

                match (
                    builder.create(tmp.as_path()),
                    builder.create(new.as_path()),
                    builder.create(cur.as_path()),
                ) {
                    (Ok(()), Ok(()), Ok(())) => Ok(Self { new, cur, tmp }),
                    (Err(e), _, _) => Err(MaildirCreationError::Io(tmp, e.kind())),
                    (_, Err(e), _) => Err(MaildirCreationError::Io(new, e.kind())),
                    (_, _, Err(e)) => Err(MaildirCreationError::Io(cur, e.kind())),
                }
            }
        }
    }

    fn unchecked(mail_dir: &Path) -> Self {
        let new = mail_dir.join("new");
        let cur = mail_dir.join("cur");
        let tmp = mail_dir.join("tmp");
        Self { new, cur, tmp }
    }

    pub fn load(mail_dir: &Path) -> Result<Self, MaildirLoadError<'_>> {
        let mail = Self::unchecked(mail_dir);
        trace!("loading maildir {mail:?}");
        match (
            mail.new.try_exists(),
            mail.cur.try_exists(),
            mail.tmp.try_exists(),
        ) {
            (Ok(true), Ok(true), Ok(true)) => Ok(mail),
            (Ok(false), Ok(false), Ok(false)) => Err(MaildirLoadError::Missing(mail_dir)),
            (Ok(_), Ok(_), Ok(_)) => Err(MaildirLoadError::Partial(mail_dir)),
            (Err(e), _, _) => Err(MaildirLoadError::Io(mail.new, e.kind())),
            (_, Err(e), _) => Err(MaildirLoadError::Io(mail.cur, e.kind())),
            (_, _, Err(e)) => Err(MaildirLoadError::Io(mail.tmp, e.kind())),
        }
    }

    // Algorithm
    // Technically the program should chdir into maildir_root to prevent issues if the path of
    // maildir_root changes. Setting current_dir is a process wide operation though and will mess
    // up relative file operations in the spawn_blocking threads.
    pub fn store(&self, mail: &RemoteMail) -> Result<LocalMailMetadata, MaildirError> {
        let new_local_metadata = LocalMailMetadata::from(mail.metadata());
        let file_path = self.tmp.join(new_local_metadata.fileprefix());

        trace!("writing to {}", file_path.display());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file_path)?;

        file.write_all(mail.content())?;
        file.sync_all()?;

        Self::rename(file_path, self.get_path_of(&new_local_metadata))?;

        Ok(new_local_metadata)
    }

    pub fn list_cur(
        &self,
    ) -> io::Result<
        impl Iterator<Item = Result<LocalMailMetadata, MaildirListError>> + Debug + 'static,
    > {
        let dir_contents = read_dir(self.cur.as_path())?;
        Ok(dir_contents.map(|entry| {
            let entry = entry.map_err(|e| MaildirListError::Io(e.kind()))?;
            let filename = entry.file_name().into_string().map_err(|os_filename| {
                MaildirListError::InvalidFilename(format!(
                    "Cannot convert {} from OsString to String",
                    os_filename.display()
                ))
            })?;
            filename.parse().map_err(|e: ParseLocalMailMetadataError| {
                MaildirListError::ParseFilename(e.message().to_string())
            })
        }))
    }

    pub fn read(&self, metadata: LocalMailMetadata) -> io::Result<LocalMail> {
        Ok(LocalMail::new(
            fs::read(self.get_path_of(&metadata))?,
            metadata,
        ))
    }

    fn get_path_of(&self, mail: &LocalMailMetadata) -> PathBuf {
        self.cur.join(mail.filename())
    }

    fn rename(current: PathBuf, new: PathBuf) -> Result<(), MaildirError> {
        match (current.try_exists()?, new.try_exists()?) {
            (true, true) => {
                if Self::is_content_identical(current.as_path(), new.as_path())? {
                    fs::rename(&current, &new).map_err(MaildirError::from)
                } else {
                    Err(MaildirError::Existing {
                        from: current,
                        to: new,
                    })
                }
            }
            (true, false) => {
                trace!("renaming {:} to {:}", current.display(), new.display());
                fs::rename(current, new)?;

                Ok(())
            }
            (false, true) => {
                warn!(
                    "ignoring rename of {} to {}, because old file does not exist while new one does. May be due to prior crash",
                    current.to_string_lossy(),
                    new.to_string_lossy()
                );

                Ok(())
            }
            (false, false) => Err(MaildirError::Missing(current)),
        }
    }

    fn is_content_identical(current: &Path, new: &Path) -> io::Result<bool> {
        trace!(
            "checking if content of {} and {} is identical",
            current.display(),
            new.display()
        );
        let current_content = fs::read(current)?;
        let new_content = fs::read(new)?;

        Ok(current_content == new_content)
    }

    pub fn update_uid(
        &self,
        entry: &mut LocalMailMetadata,
        new_uid: Uid,
    ) -> Result<(), MaildirError> {
        let current_mail = self.get_path_of(entry);
        entry.set_uid(new_uid);
        let new_mail = self.get_path_of(entry);

        Self::rename(current_mail, new_mail)
    }

    pub fn update_flags(
        &self,
        entry: &mut LocalMailMetadata,
        new_flags: BitFlags<Flag>,
    ) -> Result<(), MaildirError> {
        debug!(
            "updating mail {} flags: {} -> {}",
            entry.uid().map_or(String::new(), |uid| uid.to_string()),
            entry.flags(),
            new_flags
        );
        let current_mail = self.get_path_of(entry);
        entry.set_flags(new_flags);
        let new_mail = self.get_path_of(entry);

        Self::rename(current_mail, new_mail)
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
pub enum MaildirLoadError<'a> {
    #[error("Found partially existing maildir at {0}")]
    Partial(&'a Path),
    #[error("No maildir found at {0}")]
    Missing(&'a Path),
    #[error("IO error during loading of maildir directory at {0}: {1}")]
    Io(PathBuf, io::ErrorKind),
}

#[derive(Debug, Error, PartialEq)]
pub enum MaildirCreationError<'a> {
    #[error("Found preexisting cur, tmp and/or new directories at {0}")]
    Exists(&'a Path),
    #[error("IO error during creation of maildir directory at {0}: {1}")]
    Io(PathBuf, io::ErrorKind),
}

#[derive(Debug, Error, PartialEq)]
pub enum MaildirListError {
    #[error("Non utf-8 filename {0}")]
    InvalidFilename(String),
    #[error("Incorrect format of mail: {0}")]
    ParseFilename(String),
    #[error("IO error trying to list maildir file")]
    Io(io::ErrorKind),
}

#[derive(Debug, Error)]
pub enum MaildirError {
    #[error("Missing mail {0}")]
    Missing(PathBuf),
    #[error("Moving {from} to {to} would overwrite mail with different content")]
    Existing { from: PathBuf, to: PathBuf },
    #[error("IO error during manipulation of mail {0}")]
    Io(io::Error),
}

impl From<io::Error> for MaildirError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<Flag> for char {
    fn from(value: Flag) -> Self {
        match value {
            Flag::Seen => 'S',
            Flag::Answered => 'R',
            Flag::Flagged => 'F',
            Flag::Deleted => 'T',
            Flag::Draft => 'D',
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        ffi::OsString,
        os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    };

    use assertables::*;
    use enumflags2::BitFlag;
    use rstest::*;
    use tempfile::{TempDir, tempdir};

    use crate::{
        imap::{RemoteContent, RemoteMailMetadata},
        repository::ModSeq,
    };

    use super::*;

    #[fixture]
    fn temp_dir() -> TempDir {
        assert_ok!(tempdir())
    }

    struct TestMaildir {
        dir: TempDir,
        maildir: Maildir,
    }

    #[fixture]
    fn maildir(temp_dir: TempDir) -> TestMaildir {
        TestMaildir {
            maildir: assert_ok!(Maildir::try_new(temp_dir.path())),
            dir: temp_dir,
        }
    }

    #[fixture]
    fn new_mail() -> RemoteMail {
        let metadata = RemoteMailMetadata::new(Uid::MAX, Flag::all(), ModSeq::try_from(8).unwrap());
        let content = RemoteContent::empty();

        RemoteMail::new(metadata, content)
    }

    #[fixture]
    fn local_mail() -> LocalMail {
        LocalMail::new(
            "foo".into(),
            LocalMailMetadata::new(Some(Uid::MAX), Flag::all(), Some("prefix".to_string())),
        )
    }

    #[rstest]
    fn test_new_creates_maildir_dirs(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        assert_ok!(Maildir::try_new(maildir_path));

        assert!(maildir_path.join("cur").exists());
        assert!(maildir_path.join("new").exists());
        assert!(maildir_path.join("tmp").exists());
    }

    #[rstest]
    fn test_new_errors_on_existing_dir(
        temp_dir: TempDir,
        #[values("cur", "tmp", "new")] dir: &str,
    ) {
        let maildir_path = temp_dir.path();
        let cur = maildir_path.join(dir);
        assert_ok!(fs::create_dir(cur));

        let maybe_maildir = Maildir::try_new(maildir_path);

        assert_matches!(maybe_maildir, Err(MaildirCreationError::Exists(_)));
    }

    #[rstest]
    fn test_new_errors_on_unreadable_dir(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        let mut permissions = assert_ok!(fs::metadata(maildir_path)).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(maildir_path, permissions));

        let result = Maildir::try_new(maildir_path);
        let result = assert_err!(result);
        assert_matches!(result, MaildirCreationError::Io(_, _));
    }

    #[rstest]
    fn test_load_loads_exisiting_dir(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        assert_ok!(fs::create_dir(maildir_path.join("cur")));
        assert_ok!(fs::create_dir(maildir_path.join("new")));
        assert_ok!(fs::create_dir(maildir_path.join("tmp")));

        assert!(Maildir::load(maildir_path).is_ok());
    }

    #[rstest]
    fn test_load_errors_on_missing_dir(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        assert_matches!(
            Maildir::load(maildir_path),
            Err(MaildirLoadError::Missing(_))
        );
    }

    #[rstest]
    fn test_load_errors_on_partial_existing_dir(
        temp_dir: TempDir,
        #[values("cur", "tmp", "new")] dir: &str,
    ) {
        let maildir_path = temp_dir.path();
        assert_ok!(fs::create_dir(maildir_path.join(dir)));

        assert_matches!(
            Maildir::load(maildir_path),
            Err(MaildirLoadError::Partial(_))
        );
    }

    #[rstest]
    fn test_load_errors_on_unreadable_dir(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        let mut permissions = assert_ok!(fs::metadata(maildir_path)).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(maildir_path, permissions));

        let result = Maildir::load(maildir_path);
        let result = assert_err!(result);
        assert_matches!(result, MaildirLoadError::Io(_, _));
    }

    #[rstest]
    fn test_store_stores_mail(maildir: TestMaildir, new_mail: RemoteMail) {
        let maildir = maildir.maildir;

        let result = assert_ok!(maildir.store(&new_mail));
        let expected = LocalMailMetadata::new(
            Some(new_mail.metadata().uid()),
            new_mail.metadata().flags(),
            Some(result.fileprefix().to_string()),
        );

        assert_eq!(result, expected);
    }

    #[rstest]
    fn test_store_errors_on_missing_dir(
        maildir: TestMaildir,
        new_mail: RemoteMail,
        #[values("tmp", "cur")] dir: &str,
    ) {
        assert_ok!(fs::remove_dir(maildir.dir.path().join(dir)));

        let result = assert_err!(maildir.maildir.store(&new_mail));
        if let MaildirError::Io(error) = result {
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        } else {
            panic!("result should be io error")
        }
    }

    #[rstest]
    fn test_list_cur_lists_all_mails(maildir: TestMaildir) {
        let maildir = maildir.maildir;
        let mail1 =
            LocalMailMetadata::new(Uid::try_from(&1).ok(), Flag::all(), Some("1".to_string()));
        let mail1_path = maildir.cur.join(mail1.filename());
        assert_ok!(fs::write(mail1_path, "1"));
        let mail2 =
            LocalMailMetadata::new(Uid::try_from(&2).ok(), Flag::all(), Some("2".to_string()));
        let mail2_path = maildir.cur.join(mail2.filename());
        assert_ok!(fs::write(mail2_path, "2"));

        let expected = HashSet::from([mail1, mail2]);
        let result: Result<HashSet<_>, _> = assert_ok!(maildir.list_cur()).collect();
        let result = assert_ok!(result);

        assert_eq!(result, expected);
    }

    #[rstest]
    fn test_list_cur_errors_on_unreadable_cur_dir(maildir: TestMaildir) {
        let maildir = maildir.maildir;
        assert_ok!(fs::remove_dir(&maildir.cur));

        let result = maildir.list_cur();

        let result = assert_err!(result);
        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    fn test_list_cur_errors_on_unparsable_filename(maildir: TestMaildir) {
        let maildir = maildir.maildir;
        assert_ok!(fs::write(maildir.cur.join("asfdasdofj"), ""));

        let expected: Vec<Result<LocalMailMetadata, _>> = vec![Err(
            MaildirListError::ParseFilename("filename should contain :2,".to_string()),
        )];
        let result: Vec<_> = assert_ok!(maildir.list_cur()).collect();

        assert_eq!(result, expected);
    }

    #[rstest]
    fn test_list_cur_errors_on_non_utf8_filename(maildir: TestMaildir) {
        let maildir = maildir.maildir;
        let filename = OsString::from_vec(vec![255]);
        assert_ok!(fs::write(maildir.cur.join(filename), ""));

        let mut result = assert_ok!(maildir.list_cur());
        let file_read = assert_some!(result.next());
        let read_error = assert_err!(file_read);
        assert_matches!(read_error, MaildirListError::InvalidFilename(_));
    }

    #[rstest]
    fn test_read_reads_mail(maildir: TestMaildir, local_mail: LocalMail) {
        let maildir = maildir.maildir;
        let (metadata, expected_content) = local_mail.unpack();
        assert_ok!(fs::write(
            maildir.cur.join(metadata.filename()),
            &expected_content
        ));
        let expected_metadata = metadata.clone();

        let result = assert_ok!(maildir.read(metadata));
        let (metadata, content) = result.unpack();
        assert_eq!(metadata, expected_metadata);
        assert_eq!(content, expected_content);
    }

    #[rstest]
    fn test_read_errors_on_io_error(maildir: TestMaildir, local_mail: LocalMail) {
        let maildir = maildir.maildir;
        let (metadata, _) = local_mail.unpack();

        let result = assert_err!(maildir.read(metadata));
        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    fn test_rename_renames_file(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert_ok!(fs::write(&current, ""));
        let expected_current = current.clone();
        let new = temp_dir.path().join("b");
        let expected_new = new.clone();
        assert!(!new.exists());

        assert_ok!(Maildir::rename(current, new));
        assert!(!expected_current.exists());
        assert!(expected_new.exists());
    }

    #[rstest]
    fn test_rename_succeeds_on_missing_source_but_existing_target(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert!(!current.exists());
        let expected_current = current.clone();
        let new = temp_dir.path().join("b");
        assert_ok!(fs::write(&new, ""));
        let expected_new = new.clone();

        assert_ok!(Maildir::rename(current, new));
        assert!(!expected_current.exists());
        assert!(expected_new.exists());
    }

    #[rstest]
    fn test_rename_succeeds_if_target_with_same_content_exists(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert_ok!(fs::write(&current, "foo"));
        let expected_current = current.clone();
        let new = temp_dir.path().join("b");
        assert_ok!(fs::write(&new, "foo"));
        let expected_new = new.clone();

        assert_ok!(Maildir::rename(current, new));
        assert!(!expected_current.exists());
        assert!(expected_new.exists());
    }

    #[rstest]
    fn test_rename_errors_if_target_with_different_content_exists(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert_ok!(fs::write(&current, "foo"));
        let expected_current = current.clone();
        let new = temp_dir.path().join("b");
        assert_ok!(fs::write(&new, "foobar"));
        let expected_new = new.clone();

        let result = assert_err!(Maildir::rename(current, new));
        match result {
            MaildirError::Existing { from, to } => {
                assert_eq!(from, expected_current);
                assert_eq!(to, expected_new);
            }
            _ => panic!("rename result should be MaildirError::Existing"),
        }
        assert!(expected_current.exists());
        assert!(expected_new.exists());
    }

    #[rstest]
    fn test_rename_errors_on_unwritable_target(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert_ok!(fs::write(&current, "foo"));
        let expected_current = current.clone();
        let mut permissions = assert_ok!(temp_dir.path().metadata()).permissions();
        let original_permissions = permissions.clone();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(temp_dir.path(), permissions));
        let new = temp_dir.path().join("b");
        assert!(!new.exists());
        let expected_new = new.clone();

        let result = assert_err!(Maildir::rename(current, new));
        assert_matches!(result, MaildirError::Io(_));
        assert_ok!(fs::set_permissions(temp_dir.path(), original_permissions));
        assert!(expected_current.exists());
        assert!(!expected_new.exists());
    }

    #[rstest]
    fn test_rename_errors_on_missing_source_and_missing_target(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert!(!current.exists());
        let expected_current = current.clone();
        let new = temp_dir.path().join("b");
        assert!(!current.exists());
        let expected_new = new.clone();

        let result = assert_err!(Maildir::rename(current, new));
        match result {
            MaildirError::Missing(path_buf) => {
                assert_eq!(path_buf, expected_current);
            }
            _ => panic!("rename result should be MaildirError::Missing"),
        }
        assert!(!expected_current.exists());
        assert!(!expected_new.exists());
    }

    #[rstest]
    fn test_update_uid_errors_on_missing_mail(maildir: TestMaildir) {
        let maildir = maildir.maildir;
        let mut entry = LocalMailMetadata::new(
            Some(Uid::try_from(&2).expect("2 should be valid uid")),
            Flag::empty(),
            Some("prefix".to_string()),
        );
        let expected = maildir.get_path_of(&entry);

        let result = assert_err!(maildir.update_uid(
            &mut entry,
            Uid::try_from(&3).expect("3 should be valid uid"),
        ));

        if let MaildirError::Missing(path_buf) = result {
            assert_eq!(path_buf, expected);
        } else {
            panic!("result should be missing error")
        }
    }

    #[rstest]
    fn test_update_flags_errors_on_missing_mail(maildir: TestMaildir) {
        let maildir = maildir.maildir;
        let mut entry = LocalMailMetadata::new(
            Some(Uid::try_from(&2).expect("2 should be valid uid")),
            Flag::empty(),
            Some("prefix".to_string()),
        );
        let expected = maildir.get_path_of(&entry);

        let result = assert_err!(maildir.update_flags(&mut entry, Flag::all()));

        if let MaildirError::Missing(path_buf) = result {
            assert_eq!(path_buf, expected);
        } else {
            panic!("result should be missing error")
        }
    }
}
