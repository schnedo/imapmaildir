use std::{
    fmt::Debug,
    fs, io,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use compio::{BufResult, fs::DirBuilder, io::AsyncWriteAtExt as _};
use crossfire::{
    AsyncRx, AsyncTx, SendError,
    spsc::{self, Array},
};
use enumflags2::BitFlags;
use futures::lock::Mutex;
use rustix::path::Arg;
use thiserror::Error;

use crate::{
    imap::RemoteMail,
    maildir::{
        LocalMailMetadata,
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

#[derive(Debug, Clone)]
pub struct Maildir {
    new: PathBuf,
    cur: PathBuf,
    tmp: PathBuf,
    watch: Arc<Mutex<Option<Watch>>>,
}

impl Maildir {
    pub async fn try_init(mail_dir: &Path) -> Result<Self, InitError> {
        match Self::load(mail_dir).await {
            Ok(_) | Err(LoadError::Partial(_)) => Err(InitError::Exists(mail_dir.to_path_buf())),
            Err(LoadError::Io(path, kind)) => Err(InitError::Io(path, kind)),
            Err(_) => {
                log::info!("creating maildir in {:#}", mail_dir.display());
                let mut builder = DirBuilder::new();
                builder.recursive(true).mode(0o700);

                let unchecked = Self::unchecked(mail_dir);

                match futures::join!(
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

    pub async fn load(mail_dir: &Path) -> Result<Self, LoadError> {
        let mail = Self::unchecked(mail_dir);
        log::trace!("loading maildir {mail:?}");
        match futures::join!(
            // todo: this should check for directories (.metadata().is_dir), not just existence
            // todo: check for read/write mode
            try_exists(&mail.new),
            try_exists(&mail.cur),
            try_exists(&mail.tmp),
        ) {
            (Ok(true), Ok(true), Ok(true)) => Ok(mail),
            (Ok(new_exists), Ok(true), Ok(tmp_exists)) => {
                log::warn!(
                    "Found partially existing maildir with intact \"cur\" directory. Recreating \"tmp\" and \"new\"..."
                );
                let create_missing = |exists: bool, dir: &Path| {
                    let dir = dir.to_path_buf();
                    async move {
                        if exists {
                            Ok(())
                        } else {
                            compio::fs::create_dir(dir).await
                        }
                    }
                };
                if let Err(e) = futures::try_join!(
                    create_missing(new_exists, &mail.new),
                    create_missing(tmp_exists, &mail.tmp),
                ) {
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

    pub async fn watch(self, buffer_size: usize) -> Result<AsyncRx<Array<Change>>, Error> {
        let (change_tx, change_rx) = spsc::bounded_async(buffer_size);
        let (watch, rx) = Watch::new(self.new.clone(), &self.cur, buffer_size)?;
        let mut my_watch = self.watch.lock().await;
        *my_watch = Some(watch);
        drop(my_watch);
        compio::runtime::spawn(async move {
            while let Ok(change) = rx.recv().await {
                log::trace!("handling filechange {change:?}");
                match self.handle_change(change, &change_tx).await {
                    Ok(Ok(())) => {}
                    Ok(Err(SendError(_))) => break,
                    Err(e) => {
                        // receiver being gone means nobody is interested in the error anymore
                        let _ = change_tx.send(Change::Error(e)).await;
                        break;
                    }
                }
            }
        })
        .detach();

        Ok(change_rx)
    }

    /// Outer error: handling the change failed, inner error: change receiver is gone
    async fn handle_change(
        &self,
        change: watcher::Change,
        change_tx: &AsyncTx<Array<Change>>,
    ) -> Result<Result<(), SendError<Change>>, Error> {
        match change {
            watcher::Change::Rename { from, to } => {
                let from_entry: Result<LocalMailMetadata, _> = from.parse();
                let to_entry: Result<MaildirEntry, _> = to.parse();
                match (from_entry, to_entry) {
                    (Ok(from), Ok(to)) => {
                        let to = match to {
                            MaildirEntry::MaybeTracked(to) if from.uid() == to.uid() => to,
                            MaildirEntry::New(to) => self.track_renamed(to, &from).await?,
                            MaildirEntry::MaybeTracked(to) => self.track_renamed(to, &from).await?,
                        };

                        Ok(change_tx.send(Change::Rename { from, to }).await)
                    }
                    (Ok(from), Err(_)) => {
                        if let Err(e) = Self::send_new_unstructured_change(
                            &self.watch,
                            &self.cur,
                            change_tx,
                            to,
                        )
                        .await?
                        {
                            return Ok(Err(e));
                        }
                        Ok(change_tx.send(Change::Deletion(from)).await)
                    }
                    (Err(_), Ok(to)) => {
                        log::warn!("ignoring deletion of untracked file {from}");
                        self.send_new_structured_change(change_tx, to).await
                    }
                    (Err(_), Err(_)) => {
                        log::warn!("ignoring deletion of untracked file {from}");
                        Self::send_new_unstructured_change(&self.watch, &self.cur, change_tx, to)
                            .await
                    }
                }
            }
            watcher::Change::New(filename) => {
                let entry: Result<MaildirEntry, _> = filename.parse();
                if let Ok(entry) = entry {
                    self.send_new_structured_change(change_tx, entry).await
                } else {
                    Self::send_new_unstructured_change(&self.watch, &self.cur, change_tx, filename)
                        .await
                }
            }
            watcher::Change::Overflow => Ok(change_tx.send(Change::Overflow).await),
            watcher::Change::Deletion(filename) => {
                let entry: Result<LocalMailMetadata, _> = filename.parse();
                if let Ok(entry) = entry {
                    Ok(change_tx.send(Change::Deletion(entry)).await)
                } else {
                    log::warn!("ignoring removal of untracked file {filename}");
                    Ok(Ok(()))
                }
            }
            watcher::Change::Error(e) => Err(Error::Watch(e)),
        }
    }

    /// Renames file of `to` to carry the uid of `from`
    async fn track_renamed(
        &self,
        to: impl MaildirFile,
        from: &LocalMailMetadata,
    ) -> Result<LocalMailMetadata, Error> {
        let old_to = to.filename();
        let to = to.set_uid(from.uid());
        Self::rename_new_mail(
            self.watch.lock().await.as_ref(),
            self.cur.join(old_to),
            self.cur.join(to.filename()),
        )
        .await?;

        Ok(to)
    }

    async fn send_new_structured_change(
        &self,
        change_tx: &AsyncTx<Array<Change>>,
        entry: MaildirEntry,
    ) -> Result<Result<(), SendError<Change>>, Error> {
        let to = match entry {
            MaildirEntry::New(entry) => entry,
            MaildirEntry::MaybeTracked(to) => self.remove_uid(to).await?,
        };

        Ok(change_tx.send(Change::New(to)).await)
    }

    async fn send_new_unstructured_change(
        watch: &Arc<Mutex<Option<Watch>>>,
        cur: &Path,
        change_tx: &AsyncTx<Array<Change>>,
        filename: String,
    ) -> Result<Result<(), SendError<Change>>, Error> {
        let metadata = Self::handle_new_unstructured_file(watch, cur, filename).await?;

        Ok(change_tx.send(Change::New(metadata)).await)
    }

    // Algorithm
    // Technically the program should chdir into maildir_root to prevent issues if the path of
    // maildir_root changes. Setting current_dir is a process wide operation though and will mess
    // up relative file operations in the spawn_blocking threads.
    pub async fn write_tmp(&self, mail: &RemoteMail) -> Result<LocalMailMetadata, Error> {
        let new_local_metadata = LocalMailMetadata::from(mail.metadata());
        let file_path = self.tmp_path_of(&new_local_metadata);

        log::trace!("writing to {}", file_path.display());
        let mut file = compio::fs::OpenOptions::new()
            .mode(0o400)
            .write(true)
            .create_new(true)
            .open(&file_path)
            .await?;
        let BufResult(written, _) = file.write_all_at(mail.content().clone(), 0).await;
        written?;
        file.sync_all().await?;
        file.close().await?;

        Ok(new_local_metadata)
    }

    pub async fn move_from_tmp(&self, mail: &LocalMailMetadata) -> Result<(), Error> {
        let file_path = self.tmp_path_of(mail);
        let new_path = self.get_path_of(mail);
        log::trace!("renaming {} to {}", file_path.display(), new_path.display());
        let watch = self.watch.lock().await.clone();
        Self::ignoring(
            watch.as_ref(),
            &new_path,
            compio::fs::rename(&file_path, &new_path),
        )
        .await?;

        Ok(())
    }

    pub async fn recover_from_tmp(&self, mail: &LocalMailMetadata) -> Result<bool, Error> {
        if try_exists(&self.tmp_path_of(mail)).await? {
            self.move_from_tmp(mail).await?;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn tmp_path_of(&self, mail: &LocalMailMetadata) -> PathBuf {
        self.tmp.join(mail.fileprefix())
    }

    pub async fn move_new_to_cur(&self) -> io::Result<()> {
        watcher::move_new_to_cur(&self.new, &self.cur).await
    }

    pub fn list_cur(&self) -> io::Result<AsyncRx<Array<Result<MaildirEntry, MaildirListError>>>> {
        let dir_contents = fs::read_dir(self.cur.as_path())?;
        let watch = self.watch.clone();
        let cur = self.cur.clone();
        let (list_tx, list_rx) = spsc::bounded_async(32);
        compio::runtime::spawn(async move {
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
        })
        .detach();

        Ok(list_rx)
    }

    async fn rename_new_mail(
        watch: Option<&Watch>,
        current: PathBuf,
        new: PathBuf,
    ) -> Result<(), Error> {
        if let Err(error) = Self::rename(watch, current, new).await {
            match error {
                Error::Existing { from, mut to } => {
                    let file_name = to
                        .file_name()
                        .unwrap_or_else(|| unreachable!("new name has a file name"))
                        .to_os_string();
                    let mut new_name = String::from("1");
                    new_name += &file_name.to_string_lossy();
                    to.set_file_name(new_name);

                    Box::pin(Self::rename_new_mail(watch, from, to)).await
                }
                error => Err(error),
            }
        } else {
            Ok(())
        }
    }

    pub async fn read_content(&self, metadata: &impl MaildirFile) -> io::Result<Vec<u8>> {
        let mailpath = self.get_path_of(metadata);
        log::trace!("Getting content of {}", mailpath.display());
        compio::fs::read(mailpath).await
    }

    fn get_path_of(&self, mail: &impl MaildirFile) -> PathBuf {
        self.cur.join(mail.filename())
    }

    pub async fn sync_cur(&self) -> Result<(), Error> {
        Self::sync_dir(&self.cur).await
    }

    pub async fn sync_tmp(&self) -> Result<(), Error> {
        Self::sync_dir(&self.tmp).await
    }

    async fn sync_dir(dir: &Path) -> Result<(), Error> {
        compio::fs::File::open(dir).await?.sync_all().await?;

        Ok(())
    }

    pub async fn handle_uid_conflict(
        &self,
        current: &LocalMailMetadata,
        new: &LocalMailMetadata,
    ) -> Result<(), Error> {
        let current_path = self.get_path_of(current);
        let new_path = self.tmp_path_of(new);
        let identical = match Self::is_content_identical(&current_path, &new_path).await {
            Ok(identical) => identical,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        if identical {
            compio::fs::remove_file(&new_path).await?;
        } else {
            log::warn!(
                "Found already existing mail with uid {} and different content. Keeping new mail at {}",
                new.uid(),
                new_path.display()
            );
        }

        Ok(())
    }

    async fn ignoring<T>(
        watch: Option<&Watch>,
        file: &Path,
        operation: impl Future<Output = io::Result<T>>,
    ) -> io::Result<T> {
        if let Some(watch) = watch {
            watch.ignore_next_update_for_file(file).await;
        }
        let result = operation.await;
        if result.is_err()
            && let Some(watch) = watch
        {
            watch.unignore_file(file).await;
        }

        result
    }

    async fn rename(watch: Option<&Watch>, current: PathBuf, new: PathBuf) -> Result<(), Error> {
        // todo: write and match error instead of checking if exists
        match futures::try_join!(try_exists(&current), try_exists(&new))? {
            (true, true) => {
                if Self::is_content_identical(current.as_path(), new.as_path()).await? {
                    log::warn!(
                        "Removing {} during rename to {}: target name is already present with identical content",
                        current.display(),
                        new.display()
                    );
                    Self::ignoring(watch, &current, compio::fs::remove_file(&current))
                        .await
                        .map_err(Error::from)
                } else {
                    Err(Error::Existing {
                        from: current,
                        to: new,
                    })
                }
            }
            (true, false) => {
                log::trace!("renaming {:} to {:}", current.display(), new.display());
                Self::ignoring(watch, &new, compio::fs::rename(&current, &new)).await?;

                Ok(())
            }
            (false, true) => {
                log::warn!(
                    "ignoring rename of {} to {}, because old file does not exist while new one does. May be due to prior crash",
                    current.to_string_lossy(),
                    new.to_string_lossy()
                );

                Ok(())
            }
            (false, false) => Err(Error::Missing(current)),
        }
    }

    async fn is_content_identical(current: &Path, new: &Path) -> io::Result<bool> {
        log::trace!(
            "checking if content of {} and {} is identical",
            current.display(),
            new.display()
        );
        let (current_content, new_content) =
            futures::try_join!(compio::fs::read(current), compio::fs::read(new))?;

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
        let watch = self.watch.lock().await.clone();
        Self::rename(watch.as_ref(), current_mail, new_mail).await?;

        Ok(entry)
    }

    pub async fn remove_uid(
        &self,
        entry: LocalMailMetadata,
    ) -> Result<NewLocalMailMetadata, Error> {
        let current = self.get_path_of(&entry);
        let new_metadata = NewLocalMailMetadata::from(entry);
        let watch = self.watch.lock().await.clone();
        Self::rename(watch.as_ref(), current, self.get_path_of(&new_metadata)).await?;

        Ok(new_metadata)
    }

    pub async fn update_flags(
        &self,
        entry: &mut impl MaildirFile,
        new_flags: BitFlags<Flag>,
    ) -> Result<(), Error> {
        log::debug!("updating mail {} flags: {}", entry.filename(), new_flags);
        let current_mail = self.get_path_of(entry);
        entry.set_flags(new_flags);
        let new_mail = self.get_path_of(entry);

        let watch = self.watch.lock().await.clone();
        Self::rename(watch.as_ref(), current_mail, new_mail).await
    }

    pub async fn delete(&self, entry: &impl MaildirFile) -> io::Result<()> {
        let file_path = self.get_path_of(entry);
        log::trace!("deleting {}", file_path.display());
        let watch = self.watch.lock().await.clone();
        Self::ignoring(
            watch.as_ref(),
            &file_path,
            compio::fs::remove_file(&file_path),
        )
        .await
        .or_else(|e| {
            if let std::io::ErrorKind::NotFound = e.kind() {
                log::trace!("{} already gone", file_path.display());
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
    ) -> Result<NewLocalMailMetadata, Error> {
        let current = cur.join(&filename);
        let metadata = NewLocalMailMetadata::new(Flag::Seen.into(), filename);
        let new = cur.join(metadata.filename());
        Self::rename_new_mail(watch.lock().await.as_ref(), current, new).await?;

        Ok(metadata)
    }
}

/// Async version of [`Path::try_exists`]
async fn try_exists(path: &Path) -> io::Result<bool> {
    match compio::fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[derive(Debug)]
pub enum Change {
    Deletion(LocalMailMetadata),
    New(NewLocalMailMetadata),
    Rename {
        from: LocalMailMetadata,
        to: LocalMailMetadata,
    },
    Overflow,
    Error(Error),
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
    #[error("Listed mail {0} vanished")]
    Missing(PathBuf),
}

impl From<Error> for MaildirListError {
    fn from(value: Error) -> Self {
        match value {
            Error::Io(e) => Self::Io(e.kind()),
            Error::Missing(path) => Self::Missing(path),
            Error::Existing { .. } | Error::Watch(_) => {
                unreachable!("renaming new mail resolves conflicts and does not watch")
            }
        }
    }
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
    #[error("{0}")]
    Watch(watcher::Error),
}

impl From<watcher::Error> for Error {
    fn from(value: watcher::Error) -> Self {
        Self::Watch(value)
    }
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
        time::Duration,
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
    async fn maildir(temp_dir: TempDir) -> TestMaildir {
        TestMaildir {
            maildir: assert_ok!(Maildir::try_init(temp_dir.path()).await),
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
    #[compio::test]
    async fn test_init_creates_maildir_dirs(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        assert_ok!(Maildir::try_init(maildir_path).await);

        assert!(maildir_path.join("cur").exists());
        assert!(maildir_path.join("new").exists());
        assert!(maildir_path.join("tmp").exists());
    }

    #[rstest]
    #[compio::test]
    async fn test_init_errors_on_existing_dir(
        temp_dir: TempDir,
        #[values("cur", "tmp", "new")] dir: &str,
    ) {
        let maildir_path = temp_dir.path();
        let cur = maildir_path.join(dir);
        assert_ok!(fs::create_dir(cur));

        let maybe_maildir = Maildir::try_init(maildir_path).await;

        assert_matches!(maybe_maildir, Err(InitError::Exists(_)));
    }

    #[rstest]
    #[compio::test]
    async fn test_new_errors_on_unreadable_dir(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        let mut permissions = assert_ok!(fs::metadata(maildir_path)).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(maildir_path, permissions));

        let result = Maildir::try_init(maildir_path).await;
        let result = assert_err!(result);
        assert_matches!(result, InitError::Io(_, _));
    }

    #[rstest]
    #[compio::test]
    async fn test_load_loads_exisiting_dir(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        assert_ok!(fs::create_dir(maildir_path.join("cur")));
        assert_ok!(fs::create_dir(maildir_path.join("new")));
        assert_ok!(fs::create_dir(maildir_path.join("tmp")));

        assert!(Maildir::load(maildir_path).await.is_ok());
    }

    #[rstest]
    #[compio::test]
    async fn test_load_errors_on_missing_dir(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        assert_matches!(
            Maildir::load(maildir_path).await,
            Err(LoadError::Missing(_))
        );
    }

    #[rstest]
    #[compio::test]
    async fn test_load_errors_on_partial_existing_dir(
        temp_dir: TempDir,
        #[values("tmp", "new")] dir: &str,
    ) {
        let maildir_path = temp_dir.path();
        assert_ok!(fs::create_dir(maildir_path.join(dir)));

        assert_matches!(
            Maildir::load(maildir_path).await,
            Err(LoadError::Partial(_))
        );
    }

    #[rstest]
    #[compio::test]
    async fn test_load_recreates_tmp_and_cur_if_missing(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        assert_ok!(fs::create_dir(maildir_path.join("cur")));

        assert_ok!(Maildir::load(maildir_path).await);
    }

    #[rstest]
    #[compio::test]
    async fn test_load_errors_on_unreadable_dir(temp_dir: TempDir) {
        let maildir_path = temp_dir.path();
        let mut permissions = assert_ok!(fs::metadata(maildir_path)).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(maildir_path, permissions));

        let result = Maildir::load(maildir_path).await;
        let result = assert_err!(result);
        assert_matches!(result, LoadError::Io(_, _));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_store_stores_mail(#[future] maildir: TestMaildir, new_mail: RemoteMail) {
        let maildir = maildir.maildir;

        let result = assert_ok!(maildir.write_tmp(&new_mail).await);
        assert_ok!(maildir.move_from_tmp(&result).await);
        let expected = LocalMailMetadata::new(
            new_mail.metadata().uid(),
            new_mail.metadata().flags(),
            Some(result.fileprefix().to_string()),
        );

        assert_eq!(result, expected);
        assert!(assert_ok!(fs::exists(maildir.get_path_of(&expected))));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_store_errors_on_missing_dir(
        #[future] maildir: TestMaildir,
        new_mail: RemoteMail,
        #[values("tmp", "cur")] dir: &str,
    ) {
        assert_ok!(fs::remove_dir(maildir.dir.path().join(dir)));

        let result = match maildir.maildir.write_tmp(&new_mail).await {
            Ok(metadata) => assert_err!(maildir.maildir.move_from_tmp(&metadata).await),
            Err(e) => e,
        };
        if let Error::Io(error) = result {
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        } else {
            panic!("result should be io error")
        }
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_list_cur_lists_all_mails(#[future] maildir: TestMaildir) {
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
        let list_rx = assert_ok!(maildir.list_cur());
        let mut result: HashSet<_> = HashSet::new();
        while let Ok(entry) = list_rx.recv().await {
            let entry = assert_ok!(entry);
            result.insert(entry);
        }

        assert_eq!(result, expected);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_list_cur_errors_on_unreadable_cur_dir(#[future] maildir: TestMaildir) {
        let maildir = maildir.maildir;
        assert_ok!(fs::remove_dir(&maildir.cur));

        let result = maildir.list_cur();

        let result = assert_err!(result);
        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_list_cur_errors_on_non_utf8_filename(#[future] maildir: TestMaildir) {
        let maildir = maildir.maildir;
        let filename = OsString::from_vec(vec![255]);
        assert_ok!(fs::write(maildir.cur.join(filename), ""));

        let result = assert_ok!(maildir.list_cur());
        let file_read = assert_ok!(result.recv().await);
        let read_error = assert_err!(file_read);
        assert_matches!(read_error, MaildirListError::InvalidFilename(_));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_list_cur_renames_file_according_to_new_local_metadata(
        #[future] maildir: TestMaildir,
    ) {
        let maildir = maildir.maildir;
        let fileprefix = "f";
        let mail = maildir.cur.join(fileprefix);
        let metadata = NewLocalMailMetadata::new(Flag::Seen.into(), fileprefix.to_string());
        assert_ok!(fs::File::create_new(&mail));

        let list_rx = assert_ok!(maildir.list_cur());
        while list_rx.recv().await.is_ok() {}

        assert!(!mail.exists());
        assert!(maildir.get_path_of(&metadata).exists());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_list_cur_renames_file_with_additional_prefix_if_new_name_exists_with_different_content(
        #[future] maildir: TestMaildir,
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

        let list_rx = assert_ok!(maildir.list_cur());
        while list_rx.recv().await.is_ok() {}

        assert!(!mail.exists());
        assert!(existing_mail.exists());
        assert!(expected_mail.exists());
    }

    #[rstest]
    #[compio::test]
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
    #[compio::test]
    #[awt]
    async fn test_read_reads_mail(#[future] maildir: TestMaildir, local_mail: LocalMail) {
        let maildir = maildir.maildir;
        let (metadata, expected_content) = local_mail.unpack();
        assert_ok!(fs::write(
            maildir.cur.join(metadata.filename()),
            &expected_content
        ));

        let result = assert_ok!(maildir.read_content(&metadata).await);
        assert_eq!(result, expected_content);
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_read_errors_on_io_error(#[future] maildir: TestMaildir, local_mail: LocalMail) {
        let maildir = maildir.maildir;
        let (metadata, _) = local_mail.unpack();

        let result = assert_err!(maildir.read_content(&metadata).await);
        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    #[compio::test]
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
    #[compio::test]
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
    #[compio::test]
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
    #[compio::test]
    #[awt]
    async fn test_update_flags_onto_identical_file_is_not_reported_as_change(
        #[future] maildir: TestMaildir,
        metadata: LocalMailMetadata,
    ) {
        let maildir = maildir.maildir;
        let mut entry = metadata;
        let mut renamed = entry.clone();
        renamed.set_flags(Flag::Seen | Flag::Flagged);
        assert_ok!(fs::write(maildir.get_path_of(&entry), "content"));
        assert_ok!(fs::write(maildir.get_path_of(&renamed), "content"));
        let changes = assert_ok!(maildir.clone().watch(8).await);

        assert_ok!(maildir.update_flags(&mut entry, renamed.flags()).await);

        assert_err!(compio::time::timeout(Duration::from_millis(300), changes.recv()).await);
        assert!(maildir.get_path_of(&renamed).exists());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_of_missing_mail_does_not_ignore_later_changes(
        #[future] maildir: TestMaildir,
        metadata: LocalMailMetadata,
    ) {
        let maildir = maildir.maildir;
        let changes = assert_ok!(maildir.clone().watch(8).await);
        assert_ok!(maildir.delete(&metadata).await);

        assert_ok!(fs::write(maildir.tmp.join("x"), "content"));
        assert_ok!(fs::rename(
            maildir.tmp.join("x"),
            maildir.get_path_of(&metadata)
        ));

        let change = assert_ok!(assert_ok!(
            compio::time::timeout(Duration::from_secs(1), changes.recv()).await
        ));
        assert_matches!(change, Change::New(_));
    }

    #[rstest]
    #[compio::test]
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
    #[compio::test]
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
    #[compio::test]
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
    #[compio::test]
    #[awt]
    async fn test_update_uid_updates_uid(#[future] maildir: TestMaildir, local_mail: LocalMail) {
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
    #[compio::test]
    #[awt]
    async fn test_update_flags_errors_on_missing_mail(
        #[future] maildir: TestMaildir,
        local_mail: LocalMail,
    ) {
        let maildir = maildir.maildir;
        let (mut entry, content) = local_mail.unpack();
        assert_ok!(fs::write(maildir.cur.join(entry.filename()), &content));

        let expected_flags = Flag::empty();
        assert_ok!(maildir.update_flags(&mut entry, expected_flags).await);

        assert_eq!(entry.flags(), expected_flags);
        assert!(maildir.get_path_of(&entry).exists());
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_deletes_existing_mail(
        #[future] maildir: TestMaildir,
        local_mail: LocalMail,
    ) {
        let maildir = maildir.maildir;
        let (entry, content) = local_mail.unpack();
        assert_ok!(fs::write(maildir.get_path_of(&entry), &content));

        assert_ok!(maildir.delete(&entry).await);
        assert!(!assert_ok!(fs::exists(maildir.get_path_of(&entry))));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_succeeds_on_already_gone_mail(
        #[future] maildir: TestMaildir,
        local_mail: LocalMail,
    ) {
        let maildir = maildir.maildir;
        let (entry, ..) = local_mail.unpack();

        assert_ok!(maildir.delete(&entry).await);
        assert!(!assert_ok!(fs::exists(maildir.get_path_of(&entry))));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_delete_propagates_deletion_error(
        #[future] maildir: TestMaildir,
        local_mail: LocalMail,
    ) {
        let (entry, ..) = local_mail.unpack();
        let mut permissions = assert_ok!(maildir.dir.path().metadata()).permissions();
        permissions.set_mode(0o000);
        assert_ok!(fs::set_permissions(maildir.dir.path(), permissions));

        let result = assert_err!(maildir.maildir.delete(&entry).await);
        assert_matches!(result, io::Error { .. });
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_remove_uid_removes_uid(
        #[future] maildir: TestMaildir,
        metadata: LocalMailMetadata,
    ) {
        let maildir = maildir.maildir;
        assert_ok!(fs::File::create_new(maildir.get_path_of(&metadata)));
        let old_metadata = metadata.clone();

        let new_metadata = assert_ok!(maildir.remove_uid(metadata).await);
        assert!(assert_ok!(fs::exists(maildir.get_path_of(&new_metadata))));
        assert!(!assert_ok!(fs::exists(maildir.get_path_of(&old_metadata))));
    }

    #[rstest]
    #[compio::test]
    #[awt]
    async fn test_remove_uid_errors_if_new_file_with_different_content_exists(
        #[future] maildir: TestMaildir,
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
