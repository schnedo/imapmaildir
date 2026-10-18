use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    io,
    os::fd::AsFd as _,
    path::Path,
    sync::Arc,
    time::Duration,
};

use compio::runtime::fd::PollFd;
use crossfire::{AsyncRx, AsyncTx, MAsyncTx, mpsc, spsc};
use futures::lock::Mutex;
use inotify::{EventMask, Inotify, WatchMask};

enum WatchEvent {
    Inotify(inotify::Event<OsString>),
    MoveTimeout(u32, EventMask),
}

#[derive(Debug, Clone)]
pub struct Watch {
    ignore_files: Arc<Mutex<HashSet<OsString>>>,
}
impl Watch {
    pub fn new(path: &Path, buffer_size: usize) -> (Self, AsyncRx<spsc::Array<Change>>) {
        let (change_tx, change_rx) = spsc::bounded_async(buffer_size);
        let ignore_files = Arc::new(Mutex::new(HashSet::new()));
        let inotify = Inotify::init().expect("initializing inotify should succeed");
        inotify
            .watches()
            .add(
                path,
                WatchMask::CREATE | WatchMask::CLOSE_WRITE | WatchMask::MOVE | WatchMask::DELETE,
            )
            .expect("adding cur watch should succeed");

        let (event_tx, event_rx) = mpsc::bounded_async(32);
        Self::spawn_inotify_reader(inotify, event_tx.clone());

        let ignored = ignore_files.clone();
        compio::runtime::spawn(async move {
            let mut move_matches: HashMap<u32, OsString> = HashMap::new();
            let mut created: HashSet<OsString> = HashSet::new();
            while let Ok(watch_event) = event_rx.recv().await {
                match watch_event {
                    WatchEvent::MoveTimeout(cookie, mask) => {
                        log::trace!("file {cookie:?} timed out");
                        if let Some(filename) = move_matches.remove(&cookie) {
                            let change = match mask {
                                EventMask::MOVED_TO => Change::New,
                                EventMask::MOVED_FROM => Change::Deletion,
                                _ => unreachable!("mask should only be defined values"),
                            };
                            Self::send_unless_ignored(filename, &ignored, &change_tx, change).await;
                        }
                    }
                    WatchEvent::Inotify(event) => {
                        log::trace!("{event:?}");
                        if event.mask.contains(EventMask::ISDIR) {
                            continue;
                        }
                        match event.mask {
                            EventMask::MOVED_FROM | EventMask::MOVED_TO => {
                                Self::handle_move_event(
                                    event,
                                    &mut move_matches,
                                    &change_tx,
                                    &event_tx,
                                    &ignored,
                                )
                                .await;
                            }
                            EventMask::DELETE => {
                                let filename = event
                                    .name
                                    .expect("name should always be present in delete event");
                                Self::send_unless_ignored(
                                    filename,
                                    &ignored,
                                    &change_tx,
                                    Change::Deletion,
                                )
                                .await;
                            }
                            EventMask::CREATE => {
                                created.insert(
                                    event
                                        .name
                                        .expect("name should always be present in create event"),
                                );
                            }
                            EventMask::CLOSE_WRITE => {
                                let filename = event
                                    .name
                                    .expect("name should always be present in close event");
                                if created.remove(&filename) {
                                    Self::send_unless_ignored(
                                        filename,
                                        &ignored,
                                        &change_tx,
                                        Change::New,
                                    )
                                    .await;
                                }
                            }
                            EventMask::Q_OVERFLOW => {
                                log::warn!("inotify event queue overflowed");
                                move_matches.clear();
                                created.clear();
                                change_tx
                                    .send(Change::Overflow)
                                    .await
                                    .expect("change channel should still be open");
                            }
                            EventMask::IGNORED => panic!("watched maildir should not be removed"),
                            _ => unreachable!(
                                "should never recieve unregistered inotify event {event:?}"
                            ),
                        }
                    }
                }
            }
        })
        .detach();

        (Self { ignore_files }, change_rx)
    }

    async fn send_unless_ignored(
        filename: OsString,
        ignored: &Arc<Mutex<HashSet<OsString>>>,
        change_tx: &AsyncTx<spsc::Array<Change>>,
        change: fn(String) -> Change,
    ) {
        if !ignored.lock().await.remove(&filename) {
            let filename = filename
                .into_string()
                .expect("filename should be valid utf8");
            change_tx
                .send(change(filename))
                .await
                .expect("change channel should still be open");
        }
    }

    fn spawn_inotify_reader(mut inotify: Inotify, event_tx: MAsyncTx<mpsc::Array<WatchEvent>>) {
        let inotify_fd = PollFd::new(
            inotify
                .as_fd()
                .try_clone_to_owned()
                .expect("duplicating inotify fd should succeed"),
        )
        .expect("polling inotify fd should be possible");
        compio::runtime::spawn(async move {
            let mut buf = [0; 1024];
            loop {
                inotify_fd
                    .read_ready()
                    .await
                    .expect("waiting for inotify events should succeed");
                let events = match inotify.read_events(&mut buf) {
                    Ok(events) => events,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(e) => panic!("reading inotify events should succeed: {e}"),
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
    }

    pub async fn ignore_next_update_for_file(&self, file: &Path) {
        let mut ignored = self.ignore_files.lock().await;
        log::trace!("ignoring next update of {}", file.display());
        ignored.insert(
            file.file_name()
                .expect("ignored file should have a name")
                .to_os_string(),
        );
    }

    async fn handle_move_event(
        event: inotify::Event<OsString>,
        move_matches: &mut HashMap<u32, OsString>,
        change_tx: &AsyncTx<spsc::Array<Change>>,
        event_tx: &MAsyncTx<mpsc::Array<WatchEvent>>,
        ignored: &Arc<Mutex<HashSet<OsString>>>,
    ) {
        let filename = event
            .name
            .expect("name should always be present in move event");
        if let Some(stored_name) = move_matches.remove(&event.cookie) {
            let mut ignored = ignored.lock().await;
            if !(ignored.remove(&filename) || ignored.remove(&stored_name)) {
                let filename = filename
                    .into_string()
                    .expect("filename should be valid utf8");
                let stored_name = stored_name
                    .into_string()
                    .expect("filename should be valid utf8");
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
                change_tx
                    .send(change)
                    .await
                    .expect("change channel should still be open");
            }
        } else {
            move_matches.insert(event.cookie, filename);
            let event_tx = event_tx.clone();
            compio::runtime::spawn(async move {
                compio::time::sleep(Duration::from_millis(50)).await;
                event_tx
                    .send(WatchEvent::MoveTimeout(event.cookie, event.mask))
                    .await
                    .expect("watch event channel should still be open");
            })
            .detach();
        }
    }
}

#[derive(Debug)]
pub enum Change {
    Deletion(String),
    New(String),
    Rename { from: String, to: String },
    Overflow,
}
