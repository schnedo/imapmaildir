use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::Path,
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;
use inotify::{EventMask, Inotify, WatchMask};
use log::trace;
use tokio::sync::{Mutex, mpsc};

#[derive(Debug, Clone)]
pub struct Watch {
    ignore_files: Arc<Mutex<HashSet<OsString>>>,
}
impl Watch {
    pub fn new(path: &Path) -> (Self, mpsc::Receiver<Change>) {
        let (change_tx, change_rx) = mpsc::channel(32);
        let ignore_files = Arc::new(Mutex::new(HashSet::new()));
        let inotfy = Inotify::init().expect("initializing inotify should succeed");
        inotfy
            .watches()
            .add(
                path,
                WatchMask::CREATE | WatchMask::MOVE | WatchMask::DELETE,
            )
            .expect("adding cur watch should succeed");

        let ignored = ignore_files.clone();
        tokio::spawn(async move {
            let mut buf = [0; 1024];
            let mut stream = inotfy
                .into_event_stream(&mut buf)
                .expect("creating inotfy event stream should succeed");

            let mut move_matches: HashMap<u32, OsString> = HashMap::new();
            let (timedout_tx, mut timedout_rx) = mpsc::channel(32);
            loop {
                tokio::select! {
                    Some((cookie, mask)) = timedout_rx.recv() => {
                        trace!("move_matches {move_matches:?}");
                        if let Some(filename) = move_matches.remove(&cookie) {
                            trace!("file {cookie:?} timed out");
                            match mask {
                                EventMask::MOVED_TO => change_tx
                                    .send(Change::New(filename))
                                    .await
                                    .expect("change channel should still be open"),
                                EventMask::MOVED_FROM => change_tx
                                    .send(Change::Deletion(filename))
                                    .await
                                    .expect("change channel should still be open"),
                                _ => unreachable!("mask should only be defined values")
                            }

                        }
                    },
                    Some(event) = stream.next() => {
                        let event = event.expect("inotify event should be ok");
                        trace!("{event:?}");
                        match event.mask {
                            EventMask::MOVED_FROM | EventMask::MOVED_TO => Self::handle_move_event(event, &mut move_matches,&change_tx,&timedout_tx, &ignored).await,
                            EventMask::DELETE => {
                                let filename = event
                                    .name
                                    .expect("name should always be present in delete event");
                                let mut ignored = ignored.lock().await;
                                if !ignored.remove(&filename) {
                                    change_tx
                                        .send(Change::Deletion(filename))
                                        .await
                                        .expect("change channel should still be open");
                                }
                            }
                            EventMask::CREATE => {
                                let filename = event
                                    .name
                                    .expect("name should always be present in create event");
                                let mut ignored = ignored.lock().await;
                                if !ignored.remove(&filename) {
                                    change_tx
                                        .send(Change::New(filename))
                                        .await
                                        .expect("change channel should still be open");
                                }
                            }
                            _ => unreachable!("should never recieve unregistered inotify event {event:?}"),
                        }
                    },
                }
            }
        });

        (Self { ignore_files }, change_rx)
    }

    pub async fn ignore_next_update_for_file(&self, file: &Path) {
        let mut ignored = self.ignore_files.lock().await;
        trace!("ignoring next update of {}", file.display());
        ignored.insert(
            file.file_name()
                .expect("ignored file should have a name")
                .to_os_string(),
        );
    }

    async fn handle_move_event(
        event: inotify::Event<OsString>,
        move_matches: &mut HashMap<u32, OsString>,
        change_tx: &mpsc::Sender<Change>,
        timedout_tx: &mpsc::Sender<(u32, EventMask)>,
        ignored: &Arc<Mutex<HashSet<OsString>>>,
    ) {
        let filename = event
            .name
            .expect("name should always be present in move event");
        trace!("move_matches {move_matches:?}");
        trace!("ignore_files {ignored:?}");
        if let Some(stored_name) = move_matches.remove(&event.cookie) {
            let mut ignored = ignored.lock().await;
            if !(ignored.remove(&filename) || ignored.remove(&stored_name)) {
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
            };
        } else {
            move_matches.insert(event.cookie, filename);
            let timedout_tx = timedout_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                timedout_tx
                    .send((event.cookie, event.mask))
                    .await
                    .expect("timedout channel should still be open");
            });
        }
    }
}

#[derive(Debug)]
pub enum Change {
    Deletion(OsString),
    New(OsString),
    Rename { from: OsString, to: OsString },
}
