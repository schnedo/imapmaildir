use std::{
    fmt::Debug,
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use enumflags2::BitFlags;
use log::{debug, info, trace, warn};
use rustix::path::Arg;
use thiserror::Error;
use tokio::sync::{
    Mutex,
    mpsc::{self, error::SendError},
};

use crate::{
    imap::RemoteMail,
    maildir::{
        LocalMail, LocalMailMetadata,
        local_mail::{NewLocalMailMetadata, ParseLocalMailMetadataError},
        watcher::{self, Watch},
    },
    repository::{Flag, Uid},
};

pub trait MaildirFile {
    fn filename(&self) -> String;
    fn set_uid(self, uid: Uid) -> LocalMailMetadata;
    fn flags(&self) -> BitFlags<Flag>;
    fn set_flags(&mut self, flags: BitFlags<Flag>);
    fn additional_flags_compared_to(&self, other: &Self) -> BitFlags<Flag> {
        let mut additional_flags = self.flags();
        additional_flags.remove(other.flags());

        additional_flags
    }
    fn removed_flags_compared_to(&self, other: &Self) -> BitFlags<Flag> {
        let mut removed_flags = other.flags();
        removed_flags.remove(self.flags());

        removed_flags
    }
}

// todo: check if Arc covers clone use case
#[derive(Debug, Clone)]
pub struct Maildir {
    new: PathBuf,
    cur: PathBuf,
    tmp: PathBuf,
    watch: Arc<Mutex<Option<Watch>>>,
}

impl Maildir {
    pub fn try_init(mail_dir: &Path) -> Result<Self, InitError> {
        match Self::load(mail_dir) {
            Ok(_) | Err(LoadError::Partial(_)) => Err(InitError::Exists(mail_dir.to_path_buf())),
            Err(LoadError::Io(path, kind)) => Err(InitError::Io(path, kind)),
            Err(_) => {
                info!("creating maildir in {:#}", mail_dir.display());
                let mut builder = DirBuilder::new();
                builder.recursive(true).mode(0o700);

                let unchecked = Self::unchecked(mail_dir);

                match (
                    builder.create(unchecked.tmp.as_path()),
                    builder.create(unchecked.new.as_path()),
                    builder.create(unchecked.cur.as_path()),
                ) {
                    (Ok(()), Ok(()), Ok(())) => Ok(unchecked),
                    (Err(e), _, _) => Err(InitError::Io(unchecked.tmp, e.kind())),
                    (_, Err(e), _) => Err(InitError::Io(unchecked.new, e.kind())),
                    (_, _, Err(e)) => Err(InitError::Io(unchecked.cur, e.kind())),
                }
            }
        }
    }

    fn unchecked(mail_dir: &Path) -> Self {
        let new = mail_dir.join("new");
        let cur = mail_dir.join("cur");
        let tmp = mail_dir.join("tmp");
        Self {
            new,
            cur,
            tmp,
            watch: Arc::new(Mutex::new(None)),
        }
    }

    pub fn load(mail_dir: &Path) -> Result<Self, LoadError> {
        let mail = Self::unchecked(mail_dir);
        trace!("loading maildir {mail:?}");
        match (
            // todo: this should check for directories (.metadata().is_dir), not just existence
            // todo: check for read/write mode
            mail.new.try_exists(),
            mail.cur.try_exists(),
            mail.tmp.try_exists(),
        ) {
            (Ok(true), Ok(true), Ok(true)) => Ok(mail),
            (Ok(new_exists), Ok(true), Ok(tmp_exists)) => {
                warn!(
                    "Found partially existing maildir with intact \"cur\" directory. Recreating \"tmp\" and \"new\"..."
                );
                if !new_exists && let Err(e) = fs::create_dir(&mail.new) {
                    return Err(LoadError::Io(mail_dir.to_path_buf(), e.kind()));
                }
                if !tmp_exists && let Err(e) = fs::create_dir(&mail.tmp) {
                    return Err(LoadError::Io(mail_dir.to_path_buf(), e.kind()));
                }

                Ok(mail)
            }
            (Ok(false), Ok(false), Ok(false)) => Err(LoadError::Missing(mail_dir.to_path_buf())),
            // todo: could probably repair this if new and tmp are empty dirs/files
            (Ok(_), Ok(_), Ok(_)) => Err(LoadError::Partial(mail_dir.to_path_buf())),
            (Err(e), _, _) => Err(LoadError::Io(mail.new, e.kind())),
            (_, Err(e), _) => Err(LoadError::Io(mail.cur, e.kind())),
            (_, _, Err(e)) => Err(LoadError::Io(mail.tmp, e.kind())),
        }
    }

    #[expect(clippy::too_many_lines)]
    pub async fn watch(self, buffer_size: usize) -> mpsc::Receiver<Change> {
        let (change_tx, change_rx) = mpsc::channel(buffer_size);
        let (watch, mut rx) = Watch::new(&self.cur, buffer_size);
        let mut my_watch = self.watch.lock().await;
        *my_watch = Some(watch);
        drop(my_watch);
        tokio::spawn(async move {
            while let Some(change) = rx.recv().await {
                trace!("handling filechange {change:?}");
                match change {
                    watcher::Change::Rename { from, to } => {
                        let from_entry: Result<LocalMailMetadata, _> = from.parse();
                        let to_entry: Result<MaildirEntry, _> = to.parse();
                        match (from_entry, to_entry) {
                            (Ok(from), Ok(to)) => {
                                let to = match to {
                                    MaildirEntry::New(to) => {
                                        let old_to = to.filename();
                                        let to = to.set_uid(from.uid());
                                        Self::rename_new_mail(
                                            self.watch.lock().await.as_ref(),
                                            self.cur.join(old_to),
                                            self.cur.join(to.filename()),
                                        )
                                        .await
                                        .expect("renaming file should succeed");

                                        to
                                    }
                                    MaildirEntry::MaybeTracked(to) => {
                                        if from.uid() == to.uid() {
                                            to
                                        } else {
                                            let old_to = to.filename();
                                            let to = to.set_uid(from.uid());
                                            Self::rename_new_mail(
                                                self.watch.lock().await.as_ref(),
                                                self.cur.join(old_to),
                                                self.cur.join(to.filename()),
                                            )
                                            .await
                                            .expect("renaming file should succeed");

                                            to
                                        }
                                    }
                                };

                                change_tx
                                    .send(Change::Rename { from, to })
                                    .await
                                    .expect("change sender should still be open");
                            }
                            (Ok(from), Err(_)) => {
                                Self::send_new_unstructured_change(
                                    &self.watch,
                                    &self.cur,
                                    &change_tx,
                                    to,
                                )
                                .await;
                                change_tx
                                    .send(Change::Deletion(from))
                                    .await
                                    .expect("change sender should still be open");
                            }
                            (Err(_), Ok(to)) => {
                                warn!("ignoring deletion of untracked file {from}");
                                self.send_new_structured_change(&change_tx, to).await;
                            }
                            (Err(_), Err(_)) => {
                                warn!("ignoring deletion of untracked file {from}");
                                Self::send_new_unstructured_change(
                                    &self.watch,
                                    &self.cur,
                                    &change_tx,
                                    to,
                                )
                                .await;
                            }
                        }
                    }
                    watcher::Change::New(filename) => {
                        let entry: Result<MaildirEntry, _> = filename.parse();
                        if let Ok(entry) = entry {
                            self.send_new_structured_change(&change_tx, entry).await;
                        } else {
                            Self::send_new_unstructured_change(
                                &self.watch,
                                &self.cur,
                                &change_tx,
                                filename,
                            )
                            .await;
                        }
                    }
                    watcher::Change::Deletion(filename) => {
                        let entry: Result<LocalMailMetadata, _> = filename.parse();
                        if let Ok(entry) = entry {
                            change_tx
                                .send(Change::Deletion(entry))
                                .await
                                .expect("change sender should still be open");
                        } else {
                            warn!("ignoring removal of untracked file {filename}");
                        }
                    }
                }
            }
        });

        change_rx
    }

    async fn send_new_structured_change(
        &self,
        change_tx: &mpsc::Sender<Change>,
        entry: MaildirEntry,
    ) {
        let to = match entry {
            MaildirEntry::New(entry) => entry,
            MaildirEntry::MaybeTracked(to) => self
                .remove_uid(to)
                .await
                .expect("removing uid of new file should succeed"),
        };
        let content = self.read_content(&to).expect("mail should be readable");

        change_tx
            .send(Change::New(LocalMail::new(content, to)))
            .await
            .expect("change sender should still be open");
    }

    async fn send_new_unstructured_change(
        watch: &Arc<Mutex<Option<Watch>>>,
        cur: &Path,
        change_tx: &mpsc::Sender<Change>,
        filename: String,
    ) {
        let metadata = Self::handle_new_unstructured_file(watch, cur, filename)
            .await
            .expect("new file should be handleable");
        let content =
            fs::read(cur.join(metadata.filename())).expect("mail content should be readable");

        change_tx
            .send(Change::New(LocalMail::new(content, metadata)))
            .await
            .expect("change sender should still be open");
    }

    // Algorithm
    // Technically the program should chdir into maildir_root to prevent issues if the path of
    // maildir_root changes. Setting current_dir is a process wide operation though and will mess
    // up relative file operations in the spawn_blocking threads.
    pub async fn store(&self, mail: &RemoteMail) -> Result<LocalMailMetadata, Error> {
        let new_local_metadata = LocalMailMetadata::from(mail.metadata());
        let file_path = self.tmp.join(new_local_metadata.fileprefix());

        trace!("writing to {}", file_path.display());
        if let Some(watch) = self.watch.lock().await.as_ref() {
            watch.ignore_next_update_for_file(&file_path).await;
        }
        let mut file = OpenOptions::new()
            .mode(0o400)
            .write(true)
            .create_new(true)
            .open(&file_path)?;

        file.write_all(mail.content())?;
        file.sync_all()?;

        Self::rename(
            self.watch.lock().await.as_ref(),
            file_path,
            self.get_path_of(&new_local_metadata),
        )
        .await?;

        Ok(new_local_metadata)
    }

    pub fn list_cur(&self) -> io::Result<mpsc::Receiver<Result<MaildirEntry, MaildirListError>>> {
        let dir_contents = fs::read_dir(self.cur.as_path())?;
        let watch = self.watch.clone();
        let cur = self.cur.clone();
        let (list_tx, list_rx) = mpsc::channel(32);
        tokio::spawn(async move {
            for entry in dir_contents {
                match entry {
                    Ok(entry) => {
                        let filename = entry.file_name().into_string().map_err(|os_filename| {
                            MaildirListError::InvalidFilename(format!(
                                "Cannot convert {} from OsString to String",
                                os_filename.display()
                            ))
                        });
                        match filename {
                            Err(err) => list_tx.send(Err(err)).await?,
                            Ok(filename) => {
                                match (
                                    LocalMailMetadata::from_str(&filename),
                                    NewLocalMailMetadata::from_str(&filename),
                                ) {
                                    (Ok(metadata), _) => {
                                        list_tx
                                            .send(Ok(MaildirEntry::MaybeTracked(metadata)))
                                            .await?;
                                    }
                                    (_, Ok(metadata)) => {
                                        list_tx.send(Ok(MaildirEntry::New(metadata))).await?;
                                    }
                                    (Err(_), Err(_)) => {
                                        match Self::handle_new_unstructured_file(
                                            &watch, &cur, filename,
                                        )
                                        .await
                                        {
                                            Ok(metadata) => {
                                                list_tx
                                                    .send(Ok(MaildirEntry::New(metadata)))
                                                    .await?;
                                            }
                                            Err(err) => {
                                                list_tx.send(Err(err.into())).await?;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => list_tx.send(Err(err.into())).await?,
                }
            }

            Ok::<(), SendError<_>>(())
        });

        Ok(list_rx)
    }

    async fn rename_new_mail(
        watch: Option<&Watch>,
        current: PathBuf,
        new: PathBuf,
    ) -> io::Result<()> {
        if let Err(error) = Self::rename(watch, current, new).await {
            match error {
                Error::Existing { from, mut to } => {
                    let file_name = to
                        .file_name()
                        .unwrap_or_else(|| unreachable!("new name has a file name"))
                        .to_os_string();
                    let mut new_name = String::from("1");
                    new_name += file_name.to_str().unwrap();
                    to.set_file_name(new_name);

                    Box::pin(Self::rename_new_mail(watch, from, to)).await
                }
                Error::Io(error) => Err(error),
                Error::Missing(_) => {
                    unreachable!("Listed new mail should still be available")
                }
            }
        } else {
            Ok(())
        }
    }

    pub fn read_content(&self, metadata: &impl MaildirFile) -> io::Result<Vec<u8>> {
        let mailpath = self.get_path_of(metadata);
        trace!("Getting content of {}", mailpath.display());
        fs::read(mailpath)
    }

    fn get_path_of(&self, mail: &impl MaildirFile) -> PathBuf {
        self.cur.join(mail.filename())
    }

    async fn rename(watch: Option<&Watch>, current: PathBuf, new: PathBuf) -> Result<(), Error> {
        // todo: write and match error instead of checking if exists
        match (current.try_exists()?, new.try_exists()?) {
            (true, true) => {
                if Self::is_content_identical(current.as_path(), new.as_path())? {
                    warn!(
                        "Removing {} during rename to {}: target name is already present with identical content",
                        current.display(),
                        new.display()
                    );
                    if let Some(watch) = watch {
                        watch.ignore_next_update_for_file(&new).await;
                    }
                    fs::remove_file(&current).map_err(Error::from)
                } else {
                    Err(Error::Existing {
                        from: current,
                        to: new,
                    })
                }
            }
            (true, false) => {
                trace!("renaming {:} to {:}", current.display(), new.display());
                if let Some(watch) = watch {
                    watch.ignore_next_update_for_file(&current).await;
                }
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
            (false, false) => Err(Error::Missing(current)),
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

    pub async fn update_uid(
        &self,
        entry: impl MaildirFile,
        new_uid: Uid,
    ) -> Result<LocalMailMetadata, Error> {
        let current_mail = self.get_path_of(&entry);
        let entry = entry.set_uid(new_uid);
        let new_mail = self.get_path_of(&entry);
        Self::rename(self.watch.lock().await.as_ref(), current_mail, new_mail).await?;

        Ok(entry)
    }

    pub async fn remove_uid(
        &self,
        entry: LocalMailMetadata,
    ) -> Result<NewLocalMailMetadata, Error> {
        let current = self.get_path_of(&entry);
        let new_metadata = NewLocalMailMetadata::from(entry);
        Self::rename(
            self.watch.lock().await.as_ref(),
            current,
            self.get_path_of(&new_metadata),
        )
        .await?;

        Ok(new_metadata)
    }

    pub async fn update_flags(
        &self,
        entry: &mut impl MaildirFile,
        new_flags: BitFlags<Flag>,
    ) -> Result<(), Error> {
        debug!("updating mail {} flags: {}", entry.filename(), new_flags);
        let current_mail = self.get_path_of(entry);
        entry.set_flags(new_flags);
        let new_mail = self.get_path_of(entry);

        Self::rename(self.watch.lock().await.as_ref(), current_mail, new_mail).await
    }

    pub async fn delete(&self, entry: &impl MaildirFile) -> io::Result<()> {
        let file_path = self.get_path_of(entry);
        if let Some(watch) = &self.watch.lock().await.as_ref() {
            watch.ignore_next_update_for_file(&file_path).await;
        }
        trace!("deleting {}", file_path.display());
        fs::remove_file(&file_path).or_else(|e| {
            if let std::io::ErrorKind::NotFound = e.kind() {
                trace!("{} already gone", &file_path.display());
                Ok(())
            } else {
                Err(e)
            }
        })
    }

    async fn handle_new_unstructured_file(
        watch: &Arc<Mutex<Option<Watch>>>,
        cur: &Path,
        filename: String,
    ) -> io::Result<NewLocalMailMetadata> {
        let current = cur.join(&filename);
        let metadata = NewLocalMailMetadata::new(Flag::Seen.into(), filename);
        let new = cur.join(metadata.filename());
        Self::rename_new_mail(watch.lock().await.as_ref(), current, new).await?;

        Ok(metadata)
    }
}

#[derive(Debug)]
pub enum Change {
    Deletion(LocalMailMetadata),
    New(LocalMail),
    Rename {
        from: LocalMailMetadata,
        to: LocalMailMetadata,
    },
}

#[derive(Debug, Error, PartialEq)]
pub enum LoadError {
    #[error("Found partially existing maildir at {0}")]
    Partial(PathBuf),
    #[error("No maildir found at {0}")]
    Missing(PathBuf),
    #[error("IO error during loading of maildir directory at {0}: {1}")]
    Io(PathBuf, io::ErrorKind),
}

#[derive(Debug, Error, PartialEq)]
pub enum InitError {
    #[error("Found preexisting cur, tmp and/or new directories at {0}")]
    Exists(PathBuf),
    #[error("IO error during creation of maildir directory at {0}: {1}")]
    Io(PathBuf, io::ErrorKind),
}

#[derive(Debug, Error, PartialEq)]
pub enum MaildirListError {
    #[error("Non utf-8 filename {0}")]
    InvalidFilename(String),
    #[error("IO error trying to list maildir file")]
    Io(io::ErrorKind),
}

impl From<io::Error> for MaildirListError {
    fn from(value: io::Error) -> Self {
        Self::Io(value.kind())
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Missing mail {0}")]
    Missing(PathBuf),
    #[error("Moving {from} to {to} would overwrite mail with different content")]
    Existing { from: PathBuf, to: PathBuf },
    #[error("IO error during manipulation of mail {0}")]
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

// todo: remove Eq, PartialEq and Hash
#[derive(Debug, Eq, PartialEq, Hash)]
pub enum MaildirEntry {
    New(NewLocalMailMetadata),
    MaybeTracked(LocalMailMetadata),
}

impl FromStr for MaildirEntry {
    type Err = ParseLocalMailMetadataError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match (
            LocalMailMetadata::from_str(s),
            NewLocalMailMetadata::from_str(s),
        ) {
            (Ok(metadata), _) => Ok(Self::MaybeTracked(metadata)),
            (_, Ok(metadata)) => Ok(Self::New(metadata)),
            (Err(_), Err(e)) => Err(e),
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
        maildir::LocalMail,
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
            maildir: assert_ok!(Maildir::try_init(temp_dir.path())),
            dir: temp_dir,
        }
    }

    #[fixture]
    fn new_mail() -> RemoteMail {
        let metadata = RemoteMailMetadata::new(Uid::MAX, Flag::all(), ModSeq::try_from(8).unwrap());
        let content = RemoteContent::from_string(String::new());

        RemoteMail::new(metadata, content)
    }

    #[fixture]
    fn local_mail() -> LocalMail {
        LocalMail::new(
            "foo".into(),
            NewLocalMailMetadata::new(Flag::all(), "prefix".to_string()),
        )
    }

    #[fixture]
    fn metadata() -> LocalMailMetadata {
        LocalMailMetadata::new(
            assert_ok!(Uid::try_from(84)),
            Flag::Seen.into(),
            Some("prefix".to_string()),
        )
    }

    #[rstest]
    fn test_init_creates_maildir_dirs(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        assert_ok!(Maildir::try_init(maildir_path));

        assert!(maildir_path.join("cur").exists());
        assert!(maildir_path.join("new").exists());
        assert!(maildir_path.join("tmp").exists());
    }

    #[rstest]
    fn test_init_errors_on_existing_dir(
        temp_dir: TempDir,
        #[values("cur", "tmp", "new")] dir: &str,
    ) {
        let maildir_path = temp_dir.path();
        let cur = maildir_path.join(dir);
        assert_ok!(fs::create_dir(cur));

        let maybe_maildir = Maildir::try_init(maildir_path);

        assert_matches!(maybe_maildir, Err(InitError::Exists(_)));
    }

    #[rstest]
    fn test_new_errors_on_unreadable_dir(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        let mut permissions = assert_ok!(fs::metadata(maildir_path)).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(maildir_path, permissions));

        let result = Maildir::try_init(maildir_path);
        let result = assert_err!(result);
        assert_matches!(result, InitError::Io(_, _));
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
        assert_matches!(Maildir::load(maildir_path), Err(LoadError::Missing(_)));
    }

    #[rstest]
    fn test_load_errors_on_partial_existing_dir(
        temp_dir: TempDir,
        #[values("tmp", "new")] dir: &str,
    ) {
        let maildir_path = temp_dir.path();
        assert_ok!(fs::create_dir(maildir_path.join(dir)));

        assert_matches!(Maildir::load(maildir_path), Err(LoadError::Partial(_)));
    }

    #[rstest]
    fn test_load_recreates_tmp_and_cur_if_missing(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        assert_ok!(fs::create_dir(maildir_path.join("cur")));

        assert_ok!(Maildir::load(maildir_path));
    }

    #[rstest]
    fn test_load_errors_on_unreadable_dir(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        let mut permissions = assert_ok!(fs::metadata(maildir_path)).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(maildir_path, permissions));

        let result = Maildir::load(maildir_path);
        let result = assert_err!(result);
        assert_matches!(result, LoadError::Io(_, _));
    }

    #[rstest]
    #[tokio::test]
    async fn test_store_stores_mail(maildir: TestMaildir, new_mail: RemoteMail) {
        let maildir = maildir.maildir;

        let result = assert_ok!(maildir.store(&new_mail).await);
        let expected = LocalMailMetadata::new(
            new_mail.metadata().uid(),
            new_mail.metadata().flags(),
            Some(result.fileprefix().to_string()),
        );

        assert_eq!(result, expected);
        assert!(assert_ok!(fs::exists(maildir.get_path_of(&expected))));
    }

    #[rstest]
    #[tokio::test]
    async fn test_store_errors_on_missing_dir(
        maildir: TestMaildir,
        new_mail: RemoteMail,
        #[values("tmp", "cur")] dir: &str,
    ) {
        assert_ok!(fs::remove_dir(maildir.dir.path().join(dir)));

        let result = assert_err!(maildir.maildir.store(&new_mail).await);
        if let Error::Io(error) = result {
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        } else {
            panic!("result should be io error")
        }
    }

    #[rstest]
    #[tokio::test]
    async fn test_list_cur_lists_all_mails(maildir: TestMaildir) {
        let maildir = maildir.maildir;
        let mail1 = LocalMailMetadata::new(
            assert_ok!(Uid::try_from(&1)),
            Flag::all(),
            Some("1".to_string()),
        );
        let mail1_path = maildir.cur.join(mail1.filename());
        assert_ok!(fs::File::create_new(mail1_path));
        let mail2 = LocalMailMetadata::new(
            assert_ok!(Uid::try_from(&2)),
            Flag::all(),
            Some("2".to_string()),
        );
        let mail2_path = maildir.cur.join(mail2.filename());
        assert_ok!(fs::File::create_new(mail2_path));
        let mail3 = NewLocalMailMetadata::new(Flag::all(), "3".to_string());
        let mail3_path = maildir.cur.join(mail3.filename());
        assert_ok!(fs::File::create_new(mail3_path));

        let expected = HashSet::from([
            MaildirEntry::MaybeTracked(mail1),
            MaildirEntry::MaybeTracked(mail2),
            MaildirEntry::New(mail3),
        ]);
        let mut list_rx = assert_ok!(maildir.list_cur());
        let mut result: HashSet<_> = HashSet::new();
        while let Some(entry) = list_rx.recv().await {
            let entry = assert_ok!(entry);
            result.insert(entry);
        }

        assert_eq!(result, expected);
    }

    #[rstest]
    #[tokio::test]
    async fn test_list_cur_errors_on_unreadable_cur_dir(maildir: TestMaildir) {
        let maildir = maildir.maildir;
        assert_ok!(fs::remove_dir(&maildir.cur));

        let result = maildir.list_cur();

        let result = assert_err!(result);
        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    #[tokio::test]
    async fn test_list_cur_errors_on_non_utf8_filename(maildir: TestMaildir) {
        let maildir = maildir.maildir;
        let filename = OsString::from_vec(vec![255]);
        assert_ok!(fs::write(maildir.cur.join(filename), ""));

        let mut result = assert_ok!(maildir.list_cur());
        let file_read = assert_some!(result.recv().await);
        let read_error = assert_err!(file_read);
        assert_matches!(read_error, MaildirListError::InvalidFilename(_));
    }

    #[rstest]
    #[tokio::test]
    async fn test_list_cur_renames_file_according_to_new_local_metadata(maildir: TestMaildir) {
        let maildir = maildir.maildir;
        let fileprefix = "f";
        let mail = maildir.cur.join(fileprefix);
        let metadata = NewLocalMailMetadata::new(Flag::Seen.into(), fileprefix.to_string());
        assert_ok!(fs::File::create_new(&mail));

        let mut list_rx = assert_ok!(maildir.list_cur());
        while list_rx.recv().await.is_some() {}

        assert!(!mail.exists());
        assert!(maildir.get_path_of(&metadata).exists());
    }

    #[rstest]
    #[tokio::test]
    async fn test_list_cur_renames_file_with_additional_prefix_if_new_name_exists_with_different_content(
        maildir: TestMaildir,
    ) {
        let maildir = maildir.maildir;
        let fileprefix = "f";
        let mail = maildir.cur.join(fileprefix);
        let metadata = NewLocalMailMetadata::new(Flag::Seen.into(), fileprefix.to_string());
        assert_ok!(fs::File::create_new(&mail));
        let existing_mail = maildir.get_path_of(&metadata);
        assert_ok!(fs::write(&existing_mail, "1"));
        let expected_mail = maildir.get_path_of(&NewLocalMailMetadata::new(
            Flag::Seen.into(),
            "1".to_string() + fileprefix,
        ));

        let mut list_rx = assert_ok!(maildir.list_cur());
        while list_rx.recv().await.is_some() {}

        assert!(!mail.exists());
        assert!(existing_mail.exists());
        assert!(expected_mail.exists());
    }

    #[rstest]
    #[tokio::test]
    async fn test_rename_new_mail_errors_io_error(temp_dir: TempDir) {
        let non_existent = temp_dir.path().join("foo");
        let new = temp_dir.path().join("bar");
        let metadata = assert_ok!(temp_dir.path().metadata());
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o400);
        assert_ok!(fs::set_permissions(temp_dir.path(), permissions));

        assert_err!(Maildir::rename_new_mail(None, non_existent, new).await);
    }

    #[rstest]
    fn test_read_reads_mail(maildir: TestMaildir, local_mail: LocalMail) {
        let maildir = maildir.maildir;
        let (metadata, expected_content) = local_mail.unpack();
        assert_ok!(fs::write(
            maildir.cur.join(metadata.filename()),
            &expected_content
        ));

        let result = assert_ok!(maildir.read_content(&metadata));
        assert_eq!(result, expected_content);
    }

    #[rstest]
    fn test_read_errors_on_io_error(maildir: TestMaildir, local_mail: LocalMail) {
        let maildir = maildir.maildir;
        let (metadata, _) = local_mail.unpack();

        let result = assert_err!(maildir.read_content(&metadata));
        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    #[tokio::test]
    async fn test_rename_renames_file(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert_ok!(fs::write(&current, ""));
        let expected_current = current.clone();
        let new = temp_dir.path().join("b");
        let expected_new = new.clone();
        assert!(!new.exists());

        assert_ok!(Maildir::rename(None, current, new).await);
        assert!(!expected_current.exists());
        assert!(expected_new.exists());
    }

    #[rstest]
    #[tokio::test]
    async fn test_rename_succeeds_on_missing_source_but_existing_target(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert!(!current.exists());
        let expected_current = current.clone();
        let new = temp_dir.path().join("b");
        assert_ok!(fs::write(&new, ""));
        let expected_new = new.clone();

        assert_ok!(Maildir::rename(None, current, new).await);
        assert!(!expected_current.exists());
        assert!(expected_new.exists());
    }

    #[rstest]
    #[tokio::test]
    async fn test_rename_succeeds_if_target_with_same_content_exists(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert_ok!(fs::write(&current, "foo"));
        let expected_current = current.clone();
        let new = temp_dir.path().join("b");
        assert_ok!(fs::write(&new, "foo"));
        let expected_new = new.clone();

        assert_ok!(Maildir::rename(None, current, new).await);
        assert!(!expected_current.exists());
        assert!(expected_new.exists());
    }

    #[rstest]
    #[tokio::test]
    async fn test_rename_errors_if_target_with_different_content_exists(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert_ok!(fs::write(&current, "foo"));
        let expected_current = current.clone();
        let new = temp_dir.path().join("b");
        assert_ok!(fs::write(&new, "foobar"));
        let expected_new = new.clone();

        let result = assert_err!(Maildir::rename(None, current, new).await);
        match result {
            Error::Existing { from, to } => {
                assert_eq!(from, expected_current);
                assert_eq!(to, expected_new);
            }
            _ => panic!("rename result should be MaildirError::Existing"),
        }
        assert!(expected_current.exists());
        assert!(expected_new.exists());
    }

    #[rstest]
    #[tokio::test]
    async fn test_rename_errors_on_unwritable_target(temp_dir: TempDir) {
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

        let result = assert_err!(Maildir::rename(None, current, new).await);
        assert_matches!(result, Error::Io(_));
        assert_ok!(fs::set_permissions(temp_dir.path(), original_permissions));
        assert!(expected_current.exists());
        assert!(!expected_new.exists());
    }

    #[rstest]
    #[tokio::test]
    async fn test_rename_errors_on_missing_source_and_missing_target(temp_dir: TempDir) {
        let current = temp_dir.path().join("a");
        assert!(!current.exists());
        let expected_current = current.clone();
        let new = temp_dir.path().join("b");
        assert!(!current.exists());
        let expected_new = new.clone();

        let result = assert_err!(Maildir::rename(None, current, new).await);
        match result {
            Error::Missing(path_buf) => {
                assert_eq!(path_buf, expected_current);
            }
            _ => panic!("rename result should be MaildirError::Missing"),
        }
        assert!(!expected_current.exists());
        assert!(!expected_new.exists());
    }

    #[rstest]
    #[tokio::test]
    async fn test_update_uid_updates_uid(maildir: TestMaildir, local_mail: LocalMail) {
        let maildir = maildir.maildir;
        let (entry, content) = local_mail.unpack();
        assert_ok!(fs::write(maildir.cur.join(entry.filename()), &content));

        let expected_uid = assert_ok!(Uid::try_from(&3));
        let entry = assert_ok!(maildir.update_uid(entry, expected_uid).await);

        let result_uid = entry.uid();
        assert_eq!(result_uid, expected_uid);
        assert!(maildir.get_path_of(&entry).exists());
    }

    #[rstest]
    #[tokio::test]
    async fn test_update_flags_errors_on_missing_mail(maildir: TestMaildir, local_mail: LocalMail) {
        let maildir = maildir.maildir;
        let (mut entry, content) = local_mail.unpack();
        assert_ok!(fs::write(maildir.cur.join(entry.filename()), &content));

        let expected_flags = Flag::empty();
        assert_ok!(maildir.update_flags(&mut entry, expected_flags).await);

        assert_eq!(entry.flags(), expected_flags);
        assert!(maildir.get_path_of(&entry).exists());
    }

    #[rstest]
    #[tokio::test]
    async fn test_delete_deletes_existing_mail(maildir: TestMaildir, local_mail: LocalMail) {
        let maildir = maildir.maildir;
        let (entry, content) = local_mail.unpack();
        assert_ok!(fs::write(maildir.get_path_of(&entry), &content));

        assert_ok!(maildir.delete(&entry).await);
        assert!(!assert_ok!(fs::exists(maildir.get_path_of(&entry))));
    }

    #[rstest]
    #[tokio::test]
    async fn test_delete_succeeds_on_already_gone_mail(
        maildir: TestMaildir,
        local_mail: LocalMail,
    ) {
        let maildir = maildir.maildir;
        let (entry, ..) = local_mail.unpack();

        assert_ok!(maildir.delete(&entry).await);
        assert!(!assert_ok!(fs::exists(maildir.get_path_of(&entry))));
    }

    #[rstest]
    #[tokio::test]
    async fn test_delete_propagates_deletion_error(maildir: TestMaildir, local_mail: LocalMail) {
        let (entry, ..) = local_mail.unpack();
        let mut permissions = assert_ok!(maildir.dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(maildir.dir.path(), permissions));

        let result = assert_err!(maildir.maildir.delete(&entry).await);
        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    #[tokio::test]
    async fn test_remove_uid_removes_uid(maildir: TestMaildir, metadata: LocalMailMetadata) {
        let maildir = maildir.maildir;
        assert_ok!(fs::File::create_new(maildir.get_path_of(&metadata)));
        let old_metadata = metadata.clone();

        let new_metadata = assert_ok!(maildir.remove_uid(metadata).await);
        assert!(assert_ok!(fs::exists(maildir.get_path_of(&new_metadata))));
        assert!(!assert_ok!(fs::exists(maildir.get_path_of(&old_metadata))));
    }

    #[rstest]
    #[tokio::test]
    async fn test_remove_uid_errors_if_new_file_with_different_content_exists(
        maildir: TestMaildir,
        metadata: LocalMailMetadata,
    ) {
        let maildir = maildir.maildir;
        assert_ok!(fs::File::create_new(maildir.get_path_of(&metadata)));
        let new_entry = NewLocalMailMetadata::from(metadata.clone());
        assert_ok!(fs::write(maildir.get_path_of(&new_entry), "2"));
        let old_metadata = metadata.clone();

        let result = assert_err!(maildir.remove_uid(metadata).await);
        assert_matches!(result, Error::Existing { .. });
        assert!(assert_ok!(fs::exists(maildir.get_path_of(&old_metadata))));
    }
}
