//! Notices changes of the notmuch database by watching its Xapian files, as notmuch cannot tell
//! about them

use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError, Weak,
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
};

use crossfire::{
    AsyncRx, MAsyncTx, MTx, TrySendError,
    mpsc::{self, Array},
};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};

use super::{Error, Job, OwnChanges, Revision, call_blocking, only_own_changes, tags};

/// Name of the Xapian file, that gets replaced by every commit
const VERSION_FILE: &str = "iamglass";
const XAPIAN_DIR: &str = "xapian";

/// Watches the Xapian files of a database, as long as anybody subscribed to its changes
pub struct DatabaseWatch {
    xapian: PathBuf,
    /// Jobs of the thread calling notmuch, which is the only one using libnotmuch
    jobs: MTx<Array<Job>>,
    running: Mutex<Weak<Running>>,
}

impl std::fmt::Debug for DatabaseWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseWatch")
            .field("xapian", &self.xapian)
            .finish_non_exhaustive()
    }
}

/// Revision to compare changes to, which the subscriber starting to watch reads
pub type Baseline = SyncSender<Option<Revision>>;

/// Mailbox interested in changes of its mails
#[derive(Debug)]
struct Subscriber {
    tx: MAsyncTx<Array<()>>,
    folder: PathBuf,
    /// Changes of the mailbox itself, which it needs not get notified about
    own_changes: Arc<Mutex<Option<OwnChanges>>>,
}

/// Watching on its own thread, which stops once the last subscription got dropped
#[derive(Debug)]
struct Running {
    subscribers: Mutex<Vec<Arc<Subscriber>>>,
    /// Why watching stopped, e.g. as the database directory got removed
    failure: Mutex<Option<String>>,
    watches: Mutex<Watches>,
    descriptors: Mutex<Vec<WatchDescriptor>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut watches = self.watches.lock().unwrap_or_else(PoisonError::into_inner);
        let descriptors = self
            .descriptors
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        // the thread waits for events, which removing the watches emits
        for descriptor in descriptors.drain(..) {
            let _ = watches.remove(descriptor);
        }
    }
}

/// Changes of the database, e.g. its tags
#[derive(Debug)]
pub struct Changes {
    rx: AsyncRx<Array<()>>,
    running: Arc<Running>,
}

impl Changes {
    /// Waits for the next change. Changes since the last call get reported once.
    ///
    /// # Errors
    /// If watching the database stopped, e.g. as its directory got removed
    pub async fn next(&mut self) -> Result<(), Error> {
        self.rx.recv().await.map_err(|_| {
            Error::WatchStopped(
                self.running
                    .failure
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                    .unwrap_or_default(),
            )
        })
    }
}

impl DatabaseWatch {
    pub fn new(xapian: PathBuf, jobs: MTx<Array<Job>>) -> Self {
        Self {
            xapian,
            jobs,
            running: Mutex::new(Weak::new()),
        }
    }

    /// Changes of mails in `folder` from now on, except `own_changes`. Starts watching, unless
    /// already watching for other subscribers. Then watching waits for the current revision of
    /// the database, which the subscriber needs to send with the returned [`Baseline`].
    ///
    /// # Errors
    /// If the database files cannot be watched
    pub fn subscribe(
        &self,
        folder: PathBuf,
        own_changes: Arc<Mutex<Option<OwnChanges>>>,
    ) -> Result<(Changes, Option<Baseline>), Error> {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        let (tx, rx) = mpsc::bounded_async(1);
        let subscriber = Subscriber {
            tx,
            folder,
            own_changes,
        };
        if let Some(watching) = running.upgrade() {
            let mut subscribers = watching
                .subscribers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // checked with the subscribers locked, which a failing watch clears afterwards
            if watching
                .failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none()
            {
                subscribers.push(Arc::new(subscriber));
                drop(subscribers);
                let changes = Changes {
                    rx,
                    running: watching,
                };
                return Ok((changes, None));
            }
        }
        let (baseline, baseline_rx) = sync_channel(1);
        let started = self
            .start(baseline_rx)
            .map_err(|e| Error::Watch(self.xapian.clone(), e))?;
        started
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::new(subscriber));
        *running = Arc::downgrade(&started);
        let changes = Changes {
            rx,
            running: started,
        };

        Ok((changes, Some(baseline)))
    }

    fn start(&self, baseline: Receiver<Option<Revision>>) -> io::Result<Arc<Running>> {
        let inotify = Inotify::init()?;
        let mut watches = inotify.watches();
        let parent = self.xapian.parent().unwrap_or(&self.xapian).to_path_buf();
        // notices the database getting replaced, e.g. by `notmuch compact`
        let parent_descriptor = watches.add(
            &parent,
            WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::DELETE_SELF | WatchMask::MOVE_SELF,
        )?;
        let xapian_descriptor = watches.add(&self.xapian, XAPIAN_MASK)?;
        let running = Arc::new(Running {
            subscribers: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
            watches: Mutex::new(watches),
            descriptors: Mutex::new(vec![parent_descriptor.clone(), xapian_descriptor.clone()]),
        });
        let mut thread_state = Watcher {
            inotify,
            xapian: self.xapian.clone(),
            jobs: self.jobs.clone(),
            parent: parent_descriptor,
            xapian_descriptor: Some(xapian_descriptor),
            seen: None,
            running: Arc::downgrade(&running),
        };
        thread::Builder::new()
            .name("notmuch-watch".to_string())
            .spawn(move || {
                // changes before reading it are no news to the subscriber reading it
                thread_state.seen = baseline.recv().ok().flatten();
                if let Err(failure) = thread_state.run() {
                    log::warn!("stopped watching notmuch database: {failure}");
                    if let Some(running) = thread_state.running.upgrade() {
                        let mut subscribers = running
                            .subscribers
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        *running
                            .failure
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(failure);
                        // subscribers learn about the failure once their channels closed
                        subscribers.clear();
                    }
                }
            })?;

        Ok(running)
    }
}

const XAPIAN_MASK: WatchMask = WatchMask::MOVED_TO
    .union(WatchMask::DELETE_SELF)
    .union(WatchMask::MOVE_SELF);

struct Watcher {
    inotify: Inotify,
    xapian: PathBuf,
    parent: WatchDescriptor,
    jobs: MTx<Array<Job>>,
    /// `None` while the database got replaced
    xapian_descriptor: Option<WatchDescriptor>,
    /// Revision subscribers got notified about. `None` if unknown, e.g. as reading it failed.
    seen: Option<Revision>,
    running: Weak<Running>,
}

impl Watcher {
    /// Notifies subscribers about changes until all subscriptions got dropped. Fails with the
    /// reason, why watching cannot go on.
    fn run(&mut self) -> Result<(), String> {
        let mut buffer = [0; 4096];
        loop {
            let events = match self.inotify.read_events_blocking(&mut buffer) {
                Ok(events) => events,
                // e.g. a signal got handled on this thread
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("cannot read inotify events: {e}")),
            };
            let mut changed = false;
            let mut replaced = false;
            let mut removed = false;
            for event in events {
                log::trace!("{event:?}");
                if event.mask.contains(EventMask::Q_OVERFLOW) {
                    changed = true;
                } else if event.wd == self.parent {
                    if event
                        .mask
                        .intersects(EventMask::DELETE_SELF | EventMask::MOVE_SELF)
                        || event.mask.contains(EventMask::IGNORED)
                    {
                        removed = true;
                    } else if event.mask.contains(EventMask::ISDIR)
                        && event.name == Some(OsStr::new(XAPIAN_DIR))
                    {
                        replaced = true;
                    }
                } else if self.xapian_descriptor.as_ref() == Some(&event.wd) {
                    if event.mask.contains(EventMask::MOVED_TO)
                        && event.name == Some(OsStr::new(VERSION_FILE))
                    {
                        changed = true;
                    } else if event.mask.intersects(
                        EventMask::DELETE_SELF | EventMask::MOVE_SELF | EventMask::IGNORED,
                    ) {
                        self.forget_xapian();
                    }
                }
            }
            let Some(running) = self.running.upgrade() else {
                return Ok(());
            };
            if removed {
                return Err(format!(
                    "directory of the database {} got removed",
                    self.xapian.display()
                ));
            }
            if replaced {
                self.forget_xapian();
                self.watch_xapian(&running)?;
                // changes before watching the new files
                changed = true;
            }
            if changed {
                self.dispatch(&running);
            }
        }
    }

    /// Stops watching files of a replaced database
    fn forget_xapian(&mut self) {
        let Some(descriptor) = self.xapian_descriptor.take() else {
            return;
        };
        if let Some(running) = self.running.upgrade() {
            let _ = running
                .watches
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(descriptor.clone());
            running
                .descriptors
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|watched| *watched != descriptor);
        }
    }

    fn watch_xapian(&mut self, running: &Running) -> Result<(), String> {
        let descriptor = running
            .watches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .add(&self.xapian, XAPIAN_MASK)
            .map_err(|e| format!("cannot watch {}: {e}", self.xapian.display()))?;
        running
            .descriptors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(descriptor.clone());
        self.xapian_descriptor = Some(descriptor);

        Ok(())
    }

    /// Notifies subscribers with mails changed by others since the last notification. Checks the
    /// database once for all of them, as they would interrupt syncing to check it themselves.
    fn dispatch(&mut self, running: &Running) {
        // not locked while checking, as checking own changes waits for writes in progress
        let subscribers = running
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let current = match call_blocking(&self.jobs, super::Worker::read_revision) {
            Ok(current) => current,
            Err(e) => {
                log::warn!("cannot read revision of notmuch database: {e}");
                self.seen = None;
                Self::notify(running, subscribers.iter());
                return;
            }
        };
        let Some(seen) = self.seen.replace(current.clone()) else {
            Self::notify(running, subscribers.iter());
            return;
        };
        if seen == current {
            return;
        }
        // e.g. a recreated database
        if seen.uuid != current.uuid {
            Self::notify(running, subscribers.iter());
            return;
        }
        let foreign: Vec<&Arc<Subscriber>> = subscribers
            .iter()
            .filter(|subscriber| !only_own_changes(&subscriber.own_changes, &seen, &current))
            .collect();
        // e.g. a single mailbox writing remote changes
        if foreign.is_empty() {
            return;
        }
        let (from, to) = (seen.number + 1, current.number);
        let folders = call_blocking(&self.jobs, move |worker| {
            worker.read(|database| tags::changed_folders(database, from, to))
        });
        match folders {
            Ok(folders) => Self::notify(
                running,
                foreign
                    .into_iter()
                    .filter(|subscriber| folders.contains(&subscriber.folder)),
            ),
            Err(e) => {
                log::warn!("cannot check notmuch for changed mails: {e}");
                Self::notify(running, foreign.into_iter());
            }
        }
    }

    /// Notifies `subscribers` and forgets the ones gone meanwhile
    fn notify<'a>(running: &Running, subscribers: impl Iterator<Item = &'a Arc<Subscriber>>) {
        // a full channel already holds a change not received yet
        let gone: Vec<&Arc<Subscriber>> = subscribers
            .filter(|subscriber| {
                matches!(
                    subscriber.tx.try_send(()),
                    Err(TrySendError::Disconnected(()))
                )
            })
            .collect();
        if !gone.is_empty() {
            running
                .subscribers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|subscriber| !gone.iter().any(|gone| Arc::ptr_eq(gone, subscriber)));
        }
    }
}

/// Directory of the Xapian files of the database at `path`, preferring the layout of separate
/// database directories like notmuch
pub fn xapian_dir(path: &Path) -> Option<PathBuf> {
    [
        path.join(XAPIAN_DIR),
        path.join(".notmuch").join(XAPIAN_DIR),
    ]
    .into_iter()
    .find(|dir| dir.is_dir())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use assertables::*;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_xapian_dir_prefers_separate_database_directory() {
        let root = assert_ok!(TempDir::new());
        assert_none!(xapian_dir(root.path()));
        let legacy = root.path().join(".notmuch/xapian");
        assert_ok!(fs::create_dir_all(&legacy));
        assert_eq!(xapian_dir(root.path()), Some(legacy));

        let separate = root.path().join("xapian");
        assert_ok!(fs::create_dir(&separate));

        assert_eq!(xapian_dir(root.path()), Some(separate));
    }
}
