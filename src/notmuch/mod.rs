//! Syncs keywords of mails, e.g. IMAP keywords or Gmail labels, with tags of a notmuch database

mod config;
mod database;
mod ffi;
mod mapping;
mod tags;
mod watch;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, mpsc as std_mpsc},
    thread,
    time::{Duration, Instant},
};

use crossfire::{
    MAsyncTx, MTx,
    mpsc::{self, Array},
    oneshot::oneshot,
};
use thiserror::Error;

use crate::repository::KeywordSource;

pub use config::{Config, ConfigFile};
pub use database::{Database, Error as DatabaseError, Location, Revision};
pub use mapping::{TagConfig, TagMapping};
#[cfg(test)]
pub use tags::tests as test_support;
pub use tags::{IndexOptions, TagUpdate, changed, rename, update};
pub use watch::Changes;

use watch::DatabaseWatch;

const JOB_QUEUE_SIZE: usize = 64;
/// Time to wait for other processes, e.g. `notmuch new`, to release the write lock
const WRITE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_LOCK_MAX_BACKOFF: Duration = Duration::from_secs(2);

type Job = Box<dyn FnOnce(&mut Worker) + Send>;

/// Handle to the notmuch database of an account. Calls run on an own thread, as libnotmuch
/// blocks. The thread stops after the last handle got dropped.
#[derive(Clone)]
pub struct Notmuch {
    jobs: MAsyncTx<Array<Job>>,
    mail_root: Arc<PathBuf>,
    watch: Arc<DatabaseWatch>,
    own_changes: Arc<Mutex<Option<OwnChanges>>>,
}

/// Revisions before and after consecutive own changes of the database
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnChanges {
    before: Revision,
    after: Revision,
}

/// Whether only `own` changes happened between the revisions `from` and `to`
fn only_own_changes(own: &Mutex<Option<OwnChanges>>, from: &Revision, to: &Revision) -> bool {
    own.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|own| {
            own.after == *to
                && own.before.uuid == from.uuid
                && (own.before.number..=own.after.number).contains(&from.number)
        })
}

impl std::fmt::Debug for Notmuch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notmuch")
            .field("mail_root", &self.mail_root)
            .finish_non_exhaustive()
    }
}

struct Worker {
    location: Location,
}

impl Notmuch {
    /// Opens the database at `location` to check it and starts the thread calling notmuch
    ///
    /// # Errors
    /// If the database cannot be opened, e.g. as libnotmuch is missing, its files cannot be
    /// found or the thread cannot be spawned
    pub fn start(location: Location) -> Result<Self, Error> {
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let (jobs, job_rx) = mpsc::bounded_async_blocking::<Job>(JOB_QUEUE_SIZE);
        thread::Builder::new()
            .name("notmuch".to_string())
            .spawn(move || {
                let paths = Database::open(&location, false).and_then(|database| {
                    let paths = (database.mail_root(), database.path());
                    database.close()?;
                    Ok(paths)
                });
                let _ = ready_tx.send(paths);
                let mut worker = Worker { location };
                while let Ok(job) = job_rx.recv() {
                    job(&mut worker);
                }
            })
            .map_err(Error::Spawn)?;
        let (mail_root, path) = ready_rx.recv().map_err(|_| Error::Stopped)??;
        let mail_root = mail_root.ok_or(Error::NoMailRoot)?;
        let xapian = path
            .as_deref()
            .and_then(watch::xapian_dir)
            .ok_or(Error::NoDatabaseFiles(path))?;

        let watch = DatabaseWatch::new(xapian, MTx::from(jobs.clone()));

        Ok(Self {
            jobs,
            mail_root: Arc::new(mail_root),
            watch: Arc::new(watch),
            own_changes: Arc::new(Mutex::new(None)),
        })
    }

    /// Changes of mails in `folder` from now on, except own ones
    ///
    /// # Errors
    /// If the database files cannot be watched
    pub async fn changes(&self, folder: PathBuf) -> Result<Changes, Error> {
        let (changes, baseline) = self.watch.subscribe(folder, self.own_changes.clone())?;
        if let Some(baseline) = baseline {
            // read after starting to watch, so that later changes get noticed
            let revision = self
                .revision()
                .await
                .inspect_err(|e| log::warn!("cannot read revision of notmuch database: {e}"))
                .ok();
            let _ = baseline.send(revision);
        }

        Ok(changes)
    }

    /// Directory containing all mails of the database
    #[must_use]
    pub fn mail_root(&self) -> &Path {
        &self.mail_root
    }

    async fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Worker) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let (result_tx, result_rx) = oneshot();
        self.jobs
            .send(Box::new(move |worker: &mut Worker| {
                result_tx.send(job(worker));
            }))
            .await
            .map_err(|_| Error::Stopped)?;
        result_rx.await.map_err(|_| Error::Stopped)?
    }

    /// Current revision of the database
    ///
    /// # Errors
    /// If the database cannot be opened
    pub async fn revision(&self) -> Result<Revision, Error> {
        self.call(Worker::read_revision).await
    }

    /// Runs `f` on the database opened read-only
    ///
    /// # Errors
    /// If the database cannot be opened or `f` fails
    pub async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Database) -> Result<T, DatabaseError> + Send + 'static,
    ) -> Result<T, Error> {
        self.call(|worker| worker.read(f)).await
    }

    /// Runs `f` as single change of the database. Waits for other processes to release the
    /// write lock. Unless `own`, treats the change and own ones before like foreign ones, e.g.
    /// as they need syncing to the server.
    ///
    /// # Errors
    /// If the database cannot be opened, e.g. as the lock does not get released in time, or `f`
    /// fails
    pub async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Database) -> Result<T, DatabaseError> + Send + 'static,
        own: bool,
    ) -> Result<T, Error> {
        let own_changes = self.own_changes.clone();
        self.call(move |worker| {
            let database = worker.open_writable()?;
            // nobody else changes the database while it is open for writing
            let before = database.revision();
            let result = database.atomic(f)?;
            let after = database.revision();
            database.close()?;
            // before watching reads the new revision on this thread
            let mut own_changes = own_changes.lock().unwrap_or_else(PoisonError::into_inner);
            if !own {
                *own_changes = None;
            } else if before != after {
                let before = match own_changes.take() {
                    Some(previous) if previous.after == before => previous.before,
                    _ => before,
                };
                *own_changes = Some(OwnChanges { before, after });
            }

            Ok(result)
        })
        .await
    }

    /// Handle with own changes tracked separately, e.g. per mailbox, whose changes are foreign
    /// to other mailboxes with copies of the same messages
    #[must_use]
    pub fn tracking_separately(&self) -> Self {
        Self {
            jobs: self.jobs.clone(),
            mail_root: self.mail_root.clone(),
            watch: self.watch.clone(),
            own_changes: Arc::new(Mutex::new(None)),
        }
    }

    /// Treats own changes so far like foreign ones, e.g. as they need syncing to the server
    pub fn forget_own_changes(&self) {
        *self
            .own_changes
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Whether only own changes happened between the revisions `from` and `to`
    #[must_use]
    pub fn only_own_changes(&self, from: &Revision, to: &Revision) -> bool {
        only_own_changes(&self.own_changes, from, to)
    }

    /// Removes deleted mail files from the database. With `keep_last`, last files of their
    /// message stay and get returned, so that they can get removed later, once copies of the
    /// message got indexed.
    ///
    /// # Errors
    /// If the database cannot be opened or changed
    pub async fn remove(
        &self,
        paths: Vec<PathBuf>,
        keep_last: bool,
    ) -> Result<Vec<PathBuf>, Error> {
        self.write(
            move |database| {
                let mut kept = Vec::new();
                for path in paths {
                    // files vanished without notmuch knowing do not keep the message
                    let last = keep_last
                        && match database.find_by_filename(&path)? {
                            Some(message) => message
                                .filenames()?
                                .iter()
                                .all(|filename| *filename == path || !filename.exists()),
                            None => false,
                        };
                    if last {
                        kept.push(path);
                    } else {
                        database.remove_filename(&path)?;
                    }
                }

                Ok(kept)
            },
            true,
        )
        .await
    }
}

/// Runs `job` on the thread calling notmuch and waits for its result, e.g. from other threads
/// than async runtimes
fn call_blocking<T: Send + 'static>(
    jobs: &MTx<Array<Job>>,
    job: impl FnOnce(&mut Worker) -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    let (result_tx, result_rx) = std_mpsc::sync_channel(1);
    jobs.send(Box::new(move |worker: &mut Worker| {
        let _ = result_tx.send(job(worker));
    }))
    .map_err(|_| Error::Stopped)?;
    result_rx.recv().map_err(|_| Error::Stopped)?
}

impl Worker {
    /// Runs `f` on the database opened read-only
    fn read<T>(&self, f: impl FnOnce(&Database) -> Result<T, DatabaseError>) -> Result<T, Error> {
        let database = Database::open(&self.location, false)?;
        let result = f(&database)?;
        database.close()?;

        Ok(result)
    }

    fn read_revision(&mut self) -> Result<Revision, Error> {
        let database = Database::open(&self.location, false)?;
        let revision = database.revision();
        database.close()?;

        Ok(revision)
    }

    fn open_writable(&self) -> Result<Database, Error> {
        let started = Instant::now();
        let mut backoff = Duration::from_millis(50);
        loop {
            match Database::open(&self.location, true) {
                Ok(database) => return Ok(database),
                Err(e) if e.is_retryable() && started.elapsed() < WRITE_LOCK_TIMEOUT => {
                    if backoff >= WRITE_LOCK_MAX_BACKOFF {
                        log::warn!("{e}. Retrying...");
                    } else {
                        log::debug!("{e}. Retrying...");
                    }
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(WRITE_LOCK_MAX_BACKOFF);
                }
                Err(e) => return Err(Error::Database(e)),
            }
        }
    }
}

/// Tags of an account's mailboxes synced with keywords of the server
#[derive(Debug, Clone)]
pub struct SyncedTags {
    pub notmuch: Notmuch,
    pub mapping: Arc<TagMapping>,
}

/// Syncing tags of an account, before knowing the keywords of the server
#[derive(Debug, Clone)]
pub struct TagSync {
    pub notmuch: Notmuch,
    pub config: TagConfig,
}

impl TagSync {
    /// Tags of a single mailbox, whose own changes of notmuch are foreign to other mailboxes
    #[must_use]
    pub fn for_mailbox(&self) -> Self {
        Self {
            notmuch: self.notmuch.tracking_separately(),
            config: self.config.clone(),
        }
    }

    /// Tags synced with keywords from `source`
    #[must_use]
    pub fn synced(&self, source: KeywordSource) -> SyncedTags {
        SyncedTags {
            notmuch: self.notmuch.clone(),
            mapping: Arc::new(TagMapping::new(&self.config, source)),
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Database(#[from] DatabaseError),
    #[error("notmuch configuration lacks the mail root")]
    NoMailRoot,
    #[error("cannot find the files of the notmuch database in {0:?}")]
    NoDatabaseFiles(Option<PathBuf>),
    #[error("cannot watch notmuch database {0}: {1}")]
    Watch(PathBuf, std::io::Error),
    #[error("stopped watching notmuch database: {0}")]
    WatchStopped(String),
    #[error("cannot spawn notmuch thread: {0}")]
    Spawn(std::io::Error),
    #[error("notmuch thread stopped")]
    Stopped,
}

impl Error {
    /// Whether retrying later may succeed, e.g. once other processes released the write lock
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Database(e) => e.is_retryable(),
            // e.g. the database got recreated meanwhile
            Self::WatchStopped(_) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;

    use super::{test_support::*, *};

    #[compio::test]
    async fn test_own_changes_get_told_apart_from_others() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let path = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        let notmuch = assert_ok!(Notmuch::start(database.location()));
        let seen = assert_ok!(notmuch.revision().await);
        for tag in ["a", "b"] {
            tag_own(&notmuch, &path, tag).await;
        }
        let own = assert_ok!(notmuch.revision().await);

        assert!(notmuch.only_own_changes(&seen, &own));

        database.tag(&path, "other");
        let other = assert_ok!(notmuch.revision().await);
        assert!(!notmuch.only_own_changes(&own, &other));
        assert!(!notmuch.only_own_changes(&seen, &other));
    }

    /// Waits shortly for the next change
    async fn next_change(changes: &mut Changes) -> Option<Result<(), Error>> {
        compio::time::timeout(Duration::from_secs(1), changes.next())
            .await
            .ok()
    }

    /// Writes tag `tag` to the mail at `path` with `notmuch` as own change
    async fn tag_own(notmuch: &Notmuch, path: &Path, tag: &'static str) {
        let path = path.to_path_buf();
        assert_ok!(
            notmuch
                .write(
                    move |database| {
                        let (message, _) = database.index_file(&path)?.expect("mail");
                        message.add_tag(tag)
                    },
                    true
                )
                .await
        );
    }

    #[compio::test]
    async fn test_changes_get_noticed_only_for_mails_of_folder() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let inbox = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        let other = database.mail("Other/cur", "1,U=1:2,S", "two@example.com");
        for path in [&inbox, &other] {
            database.index_file(path);
        }
        let notmuch = assert_ok!(Notmuch::start(database.location()));
        let folder = database.root.path().join("INBOX/cur");
        let mut changes = assert_ok!(notmuch.changes(folder).await);

        database.tag(&other, "work");
        assert!(next_change(&mut changes).await.is_none());
        database.tag(&inbox, "work");

        assert_ok!(assert_some!(next_change(&mut changes).await));
    }

    #[compio::test]
    async fn test_own_changes_get_noticed_only_by_other_mailboxes() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        // copies of the same message
        let inbox = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        let other = database.mail("Other/cur", "1,U=1:2,S", "one@example.com");
        for path in [&inbox, &other] {
            database.index_file(path);
        }
        let notmuch = assert_ok!(Notmuch::start(database.location()));
        let (inbox_notmuch, other_notmuch) =
            (notmuch.tracking_separately(), notmuch.tracking_separately());
        let mut inbox_changes = assert_ok!(
            inbox_notmuch
                .changes(database.root.path().join("INBOX/cur"))
                .await
        );
        let mut other_changes = assert_ok!(
            other_notmuch
                .changes(database.root.path().join("Other/cur"))
                .await
        );

        tag_own(&inbox_notmuch, &inbox, "work").await;

        assert_ok!(assert_some!(next_change(&mut other_changes).await));
        assert!(next_change(&mut inbox_changes).await.is_none());
    }

    #[compio::test]
    async fn test_changes_get_noticed_by_all_subscribers_and_later_ones() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let path = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        database.index_file(&path);
        let notmuch = assert_ok!(Notmuch::start(database.location()));
        let folder = database.root.path().join("INBOX/cur");
        let mut first = assert_ok!(notmuch.changes(folder.clone()).await);
        let mut second = assert_ok!(notmuch.tracking_separately().changes(folder.clone()).await);

        database.tag(&path, "a");

        assert_ok!(assert_some!(next_change(&mut first).await));
        assert_ok!(assert_some!(next_change(&mut second).await));
        drop((first, second));
        // watching again, after it stopped without subscribers
        let mut later = assert_ok!(notmuch.changes(folder).await);
        database.tag(&path, "b");
        assert_ok!(assert_some!(next_change(&mut later).await));
    }

    #[compio::test]
    async fn test_changes_get_noticed_after_database_got_replaced() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let path = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        database.index_file(&path);
        let notmuch = assert_ok!(Notmuch::start(database.location()));
        let mut changes = assert_ok!(
            notmuch
                .changes(database.root.path().join("INBOX/cur"))
                .await
        );
        let dir = database.root.path().join(".notmuch");

        // like `notmuch compact`
        let copied = assert_ok!(
            std::process::Command::new("cp")
                .arg("-r")
                .arg(dir.join("xapian"))
                .arg(dir.join("xapian.compact"))
                .status()
        );
        assert!(copied.success());
        assert_ok!(std::fs::rename(dir.join("xapian"), dir.join("xapian.old")));
        assert_ok!(std::fs::rename(
            dir.join("xapian.compact"),
            dir.join("xapian")
        ));

        database.tag(&path, "work");

        assert_ok!(assert_some!(next_change(&mut changes).await));
    }

    #[compio::test]
    async fn test_removed_database_directory_stops_watching() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let notmuch = assert_ok!(Notmuch::start(database.location()));
        let mut changes = assert_ok!(
            notmuch
                .changes(database.root.path().join("INBOX/cur"))
                .await
        );

        assert_ok!(std::fs::remove_dir_all(
            database.root.path().join(".notmuch")
        ));

        let stopped = assert_err!(assert_some!(next_change(&mut changes).await));
        assert!(stopped.is_transient());
    }

    #[compio::test]
    async fn test_removal_keeps_last_existing_file_despite_vanished_copies() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let other = database.mail("Other/cur", "1,U=1:2,S", "one@example.com");
        let inbox = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        for path in [&other, &inbox] {
            database.index_file(path);
        }
        // e.g. renamed by another program without notmuch knowing
        assert_ok!(std::fs::remove_file(&other));
        assert_ok!(std::fs::remove_file(&inbox));
        let notmuch = assert_ok!(Notmuch::start(database.location()));

        let kept = assert_ok!(notmuch.remove(vec![inbox.clone()], true).await);

        assert_eq!(kept, vec![inbox.clone()]);
        assert!(database.is_indexed(&inbox));
    }

    #[compio::test]
    async fn test_own_changes_are_foreign_to_other_mailboxes_and_after_forgetting() {
        let Some(database) = TestDatabase::if_available() else {
            return;
        };
        let path = database.mail("INBOX/cur", "1,U=1:2,S", "one@example.com");
        let notmuch = assert_ok!(Notmuch::start(database.location()));
        let (inbox, other) = (notmuch.tracking_separately(), notmuch.tracking_separately());
        let seen = assert_ok!(inbox.revision().await);
        assert_ok!(
            inbox
                .write(
                    move |database| {
                        let (message, _) = database.index_file(&path)?.expect("mail");
                        message.add_tag("a")
                    },
                    true
                )
                .await
        );
        let now = assert_ok!(inbox.revision().await);

        assert!(inbox.only_own_changes(&seen, &now));
        assert!(!other.only_own_changes(&seen, &now));
        inbox.forget_own_changes();
        assert!(!inbox.only_own_changes(&seen, &now));
    }
}
