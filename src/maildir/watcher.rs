use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    io,
    os::fd::AsFd as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use compio::runtime::fd::PollFd;
use crossfire::{AsyncRx, MAsyncTx, SendError, mpsc};
use futures::lock::Mutex;
use inotify::{EventMask, Inotify, WatchMask};
use rustix::{
    fs::{CWD, RenameFlags},
    io::Errno,
};
use thiserror::Error;

type ChangeTx = MAsyncTx<mpsc::Array<Change>>;

/// Time to wait for the second event of a rename, before treating the first one as a move into or
/// out of the watched directory
const MOVE_MATCH_TIMEOUT: Duration = Duration::from_millis(50);

enum WatchEvent {
    Inotify(inotify::Event<OsString>),
    Error(Error),
}

/// Unmatched half of a rename
struct PendingMove {
    filename: OsString,
    mask: EventMask,
    deadline: Instant,
}

pub async fn move_new_to_cur(new: &Path, cur: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(new)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            move_to_cur(new, cur, &entry.file_name()).await?;
        }
    }

    Ok(())
}

async fn move_to_cur(new: &Path, cur: &Path, filename: &OsStr) -> io::Result<()> {
    let mut target = filename.to_os_string();
    if !filename.to_string_lossy().contains(":2,") {
        target.push(":2,");
    }
    log::debug!("moving {} from new to cur", filename.display());
    let source = new.join(filename);
    let cur = cur.to_path_buf();
    compio::runtime::spawn_blocking(move || {
        loop {
            match rustix::fs::renameat_with(
                CWD,
                &source,
                CWD,
                cur.join(&target),
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) | Err(Errno::NOENT) => return Ok(()),
                Err(Errno::EXIST) => {
                    // keeps mails with clashing names instead of overwriting them
                    let mut prefixed = OsString::from("1");
                    prefixed.push(&target);
                    target = prefixed;
                }
                Err(e) => return Err(e.into()),
            }
        }
    })
    .await
    .map_err(|e| io::Error::other(e.to_string()))?
}

fn init_inotify(path: &Path, mask: WatchMask) -> Result<Inotify, Error> {
    let setup_error = |e| Error::Setup(path.to_path_buf(), e);
    let inotify = Inotify::init().map_err(setup_error)?;
    inotify.watches().add(path, mask).map_err(setup_error)?;

    Ok(inotify)
}

fn watch_new(new: PathBuf, cur: PathBuf, change_tx: ChangeTx) -> Result<(), Error> {
    let inotify = init_inotify(&new, WatchMask::MOVED_TO | WatchMask::CLOSE_WRITE)?;
    let (event_tx, event_rx) = mpsc::bounded_async(32);
    Watch::spawn_inotify_reader(inotify, event_tx)?;

    compio::runtime::spawn(async move {
        let error = async {
            move_new_to_cur(&new, &cur)
                .await
                .map_err(Error::MoveToCur)?;
            while let Ok(watch_event) = event_rx.recv().await {
                let event = match watch_event {
                    WatchEvent::Inotify(event) => event,
                    WatchEvent::Error(e) => return Err(e),
                };
                log::trace!("{event:?}");
                if event.mask.contains(EventMask::ISDIR) {
                    continue;
                }
                match (event.mask, event.name) {
                    (EventMask::IGNORED, _) => return Err(Error::WatchedDirRemoved(new)),
                    (EventMask::MOVED_TO | EventMask::CLOSE_WRITE, Some(name)) => {
                        move_to_cur(&new, &cur, &name).await
                    }
                    _ => move_new_to_cur(&new, &cur).await,
                }
                .map_err(Error::MoveToCur)?;
            }

            Ok(())
        }
        .await;
        if let Err(e) = error {
            // receiver being gone means nobody is interested in the error anymore
            let _ = change_tx.send(Change::Error(e)).await;
        }
    })
    .detach();

    Ok(())
}

#[derive(Debug, Clone)]
pub struct Watch {
    ignore_files: Arc<Mutex<HashSet<OsString>>>,
}
impl Watch {
    /// Watches `cur` for changes and moves mail delivered to `new` into `cur`
    pub fn new(
        new: PathBuf,
        cur: &Path,
        buffer_size: usize,
    ) -> Result<(Self, AsyncRx<mpsc::Array<Change>>), Error> {
        let (change_tx, change_rx) = mpsc::bounded_async(buffer_size);
        let ignore_files = Arc::new(Mutex::new(HashSet::new()));
        let inotify = init_inotify(
            cur,
            WatchMask::CREATE | WatchMask::CLOSE_WRITE | WatchMask::MOVE | WatchMask::DELETE,
        )?;

        let (event_tx, event_rx) = mpsc::bounded_async(32);
        Self::spawn_inotify_reader(inotify, event_tx)?;
        watch_new(new, cur.to_path_buf(), change_tx.clone())?;

        let ignored = ignore_files.clone();
        let cur = cur.to_path_buf();
        compio::runtime::spawn(async move {
            let mut move_matches: HashMap<u32, PendingMove> = HashMap::new();
            let mut created: HashSet<OsString> = HashSet::new();
            loop {
                // waiting on the event queue instead of a timer makes queued second halves of
                // renames always match, even if the queue is backed up
                let next_deadline = move_matches.values().map(|pending| pending.deadline).min();
                let watch_event = if let Some(deadline) = next_deadline {
                    compio::time::timeout_at(deadline, event_rx.recv())
                        .await
                        .ok()
                } else {
                    Some(event_rx.recv().await)
                };
                let result = match watch_event {
                    Some(Ok(watch_event)) => {
                        Self::handle_watch_event(
                            watch_event,
                            &cur,
                            &mut move_matches,
                            &mut created,
                            &change_tx,
                            &ignored,
                        )
                        .await
                    }
                    Some(Err(_)) => break,
                    None => {
                        Self::handle_move_timeouts(&mut move_matches, &change_tx, &ignored).await
                    }
                };
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(SendError(_))) => break,
                    Err(e) => {
                        let _ = change_tx.send(Change::Error(e)).await;
                        break;
                    }
                }
            }
        })
        .detach();

        Ok((Self { ignore_files }, change_rx))
    }

    /// Treats renames without second half until their deadline as moves into or out of the
    /// watched directory
    async fn handle_move_timeouts(
        move_matches: &mut HashMap<u32, PendingMove>,
        change_tx: &ChangeTx,
        ignored: &Arc<Mutex<HashSet<OsString>>>,
    ) -> Result<Result<(), SendError<Change>>, Error> {
        let now = Instant::now();
        let timed_out: Vec<u32> = move_matches
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(cookie, _)| *cookie)
            .collect();
        for cookie in timed_out {
            log::trace!("file {cookie:?} timed out");
            let Some(PendingMove { filename, mask, .. }) = move_matches.remove(&cookie) else {
                continue;
            };
            let change = match mask {
                EventMask::MOVED_TO => Change::New,
                EventMask::MOVED_FROM => Change::Deletion,
                _ => unreachable!("mask should only be defined values"),
            };
            if let Err(e) = Self::send_unless_ignored(filename, ignored, change_tx, change).await? {
                return Ok(Err(e));
            }
        }

        Ok(Ok(()))
    }

    /// Outer error: watching failed, inner error: change receiver is gone
    async fn handle_watch_event(
        watch_event: WatchEvent,
        cur: &Path,
        move_matches: &mut HashMap<u32, PendingMove>,
        created: &mut HashSet<OsString>,
        change_tx: &ChangeTx,
        ignored: &Arc<Mutex<HashSet<OsString>>>,
    ) -> Result<Result<(), SendError<Change>>, Error> {
        match watch_event {
            WatchEvent::Error(e) => Err(e),
            WatchEvent::Inotify(event) => {
                log::trace!("{event:?}");
                if event.mask.contains(EventMask::ISDIR) {
                    return Ok(Ok(()));
                }
                match event.mask {
                    EventMask::MOVED_FROM | EventMask::MOVED_TO => {
                        Self::handle_move_event(event, move_matches, change_tx, ignored).await
                    }
                    EventMask::DELETE => {
                        let filename = event.name.ok_or(Error::MissingName(event.mask))?;
                        Self::send_unless_ignored(filename, ignored, change_tx, Change::Deletion)
                            .await
                    }
                    EventMask::CREATE => {
                        created.insert(event.name.ok_or(Error::MissingName(event.mask))?);
                        Ok(Ok(()))
                    }
                    EventMask::CLOSE_WRITE => {
                        let filename = event.name.ok_or(Error::MissingName(event.mask))?;
                        if created.remove(&filename) {
                            Self::send_unless_ignored(filename, ignored, change_tx, Change::New)
                                .await
                        } else {
                            Ok(Ok(()))
                        }
                    }
                    EventMask::Q_OVERFLOW => {
                        log::warn!("inotify event queue overflowed");
                        move_matches.clear();
                        created.clear();
                        // events of ignored updates may be lost, so they would hide later ones
                        ignored.lock().await.clear();
                        Ok(change_tx.send(Change::Overflow).await)
                    }
                    EventMask::IGNORED => Err(Error::WatchedDirRemoved(cur.to_path_buf())),
                    mask => Err(Error::UnexpectedEvent(mask)),
                }
            }
        }
    }

    async fn send_unless_ignored(
        filename: OsString,
        ignored: &Arc<Mutex<HashSet<OsString>>>,
        change_tx: &ChangeTx,
        change: fn(String) -> Change,
    ) -> Result<Result<(), SendError<Change>>, Error> {
        if ignored.lock().await.remove(&filename) {
            Ok(Ok(()))
        } else {
            let filename = into_string(filename)?;
            Ok(change_tx.send(change(filename)).await)
        }
    }

    fn spawn_inotify_reader(
        mut inotify: Inotify,
        event_tx: MAsyncTx<mpsc::Array<WatchEvent>>,
    ) -> Result<(), Error> {
        let inotify_fd = inotify
            .as_fd()
            .try_clone_to_owned()
            .and_then(PollFd::new)
            .map_err(Error::Read)?;
        compio::runtime::spawn(async move {
            let mut buf = [0; 1024];
            loop {
                if let Err(e) = inotify_fd.read_ready().await {
                    let _ = event_tx.send(WatchEvent::Error(Error::Read(e))).await;
                    return;
                }
                let events = match inotify.read_events(&mut buf) {
                    Ok(events) => events,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(e) => {
                        let _ = event_tx.send(WatchEvent::Error(Error::Read(e))).await;
                        return;
                    }
                };
                for event in events {
                    if event_tx
                        .send(WatchEvent::Inotify(event.to_owned()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        })
        .detach();

        Ok(())
    }

    pub async fn ignore_next_update_for_file(&self, file: &Path) {
        if let Some(name) = file.file_name() {
            log::trace!("ignoring next update of {}", file.display());
            self.ignore_files.lock().await.insert(name.to_os_string());
        }
    }

    pub async fn unignore_file(&self, file: &Path) {
        if let Some(name) = file.file_name() {
            self.ignore_files.lock().await.remove(name);
        }
    }

    async fn handle_move_event(
        event: inotify::Event<OsString>,
        move_matches: &mut HashMap<u32, PendingMove>,
        change_tx: &ChangeTx,
        ignored: &Arc<Mutex<HashSet<OsString>>>,
    ) -> Result<Result<(), SendError<Change>>, Error> {
        let filename = event.name.ok_or(Error::MissingName(event.mask))?;
        if let Some(PendingMove {
            filename: stored_name,
            ..
        }) = move_matches.remove(&event.cookie)
        {
            let mut ignored = ignored.lock().await;
            let filename_ignored = ignored.remove(&filename);
            let stored_name_ignored = ignored.remove(&stored_name);
            drop(ignored);
            if filename_ignored || stored_name_ignored {
                return Ok(Ok(()));
            }
            let filename = into_string(filename)?;
            let stored_name = into_string(stored_name)?;
            let change = match event.mask {
                EventMask::MOVED_FROM => Change::Rename {
                    from: filename,
                    to: stored_name,
                },
                EventMask::MOVED_TO => Change::Rename {
                    from: stored_name,
                    to: filename,
                },
                _ => unreachable!("this function should only be called with moved masks"),
            };
            Ok(change_tx.send(change).await)
        } else {
            move_matches.insert(
                event.cookie,
                PendingMove {
                    filename,
                    mask: event.mask,
                    deadline: Instant::now() + MOVE_MATCH_TIMEOUT,
                },
            );

            Ok(Ok(()))
        }
    }
}

fn into_string(filename: OsString) -> Result<String, Error> {
    filename
        .into_string()
        .map_err(|filename| Error::InvalidFilename(filename.to_string_lossy().into_owned()))
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Cannot watch {0}: {1}")]
    Setup(PathBuf, io::Error),
    #[error("Cannot read inotify events: {0}")]
    Read(io::Error),
    #[error("Watched maildir directory {0} was removed")]
    WatchedDirRemoved(PathBuf),
    #[error("Received unexpected inotify event {0:?}")]
    UnexpectedEvent(EventMask),
    #[error("Received inotify event {0:?} without file name")]
    MissingName(EventMask),
    #[error("Non utf-8 filename {0}")]
    InvalidFilename(String),
    #[error("Cannot move mail from new to cur: {0}")]
    MoveToCur(io::Error),
}

#[derive(Debug)]
pub enum Change {
    Deletion(String),
    New(String),
    Rename { from: String, to: String },
    Overflow,
    Error(Error),
}

#[cfg(test)]
mod tests {
    use std::fs;

    use assertables::*;
    use tempfile::tempdir;

    use super::*;

    #[compio::test]
    async fn test_move_new_to_cur_keeps_existing_mail_with_same_name() {
        let dir = assert_ok!(tempdir());
        let (new, cur) = (dir.path().join("new"), dir.path().join("cur"));
        assert_ok!(fs::create_dir(&new));
        assert_ok!(fs::create_dir(&cur));
        assert_ok!(fs::write(cur.join("mail:2,"), "existing"));
        assert_ok!(fs::write(new.join("mail"), "delivered"));

        assert_ok!(move_new_to_cur(&new, &cur).await);

        assert_eq!(
            assert_ok!(fs::read_to_string(cur.join("mail:2,"))),
            "existing"
        );
        assert_eq!(
            assert_ok!(fs::read_to_string(cur.join("1mail:2,"))),
            "delivered"
        );
    }
}
