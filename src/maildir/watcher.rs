use std::{collections::HashMap, ffi::OsString, path::Path};

use futures::StreamExt;
use inotify::{EventMask, Inotify, WatchMask};
use log::trace;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct Watch {}
impl Watch {
    pub fn new(path: &Path) -> (Self, mpsc::Receiver<Change>) {
        let (change_tx, change_rx) = mpsc::channel(32);
        let inotfy = Inotify::init().expect("initializing inotify should succeed");
        inotfy
            .watches()
            .add(
                path,
                WatchMask::CREATE | WatchMask::MOVE | WatchMask::DELETE,
            )
            .expect("adding cur watch should succeed");

        tokio::spawn(async move {
            let mut buf = [0; 1024];
            let mut stream = inotfy
                .into_event_stream(&mut buf)
                .expect("creating inotfy event stream should succeed");

            let mut move_matches: HashMap<u32, OsString> = HashMap::new();
            while let Some(event) = stream.next().await {
                let event = event.expect("inotify event should be ok");
                trace!("{event:?}");
                match event.mask {
                    EventMask::MOVED_FROM => {
                        let filename = event
                            .name
                            .expect("name should always be present in move event");
                        if let Some(stored_name) = move_matches.remove(&event.cookie) {
                            change_tx
                                .send(Change::Rename {
                                    from: filename,
                                    to: stored_name,
                                })
                                .await
                                .expect("change channel should still be open");
                        } else {
                            move_matches.insert(event.cookie, filename);
                        }
                    }
                    EventMask::MOVED_TO => {
                        let filename = event
                            .name
                            .expect("name should always be present in move event");
                        if let Some(stored_name) = move_matches.remove(&event.cookie) {
                            change_tx
                                .send(Change::Rename {
                                    from: stored_name,
                                    to: filename,
                                })
                                .await
                                .expect("change channel should still be open");
                        } else {
                            move_matches.insert(event.cookie, filename);
                        }
                    }
                    EventMask::DELETE => {
                        let filename = event
                            .name
                            .expect("name should always be present in delete event");
                        change_tx
                            .send(Change::Deletion(filename))
                            .await
                            .expect("change channel should still be open");
                    }
                    EventMask::CREATE => {
                        let filename = event
                            .name
                            .expect("name should always be present in create event");
                        change_tx
                            .send(Change::New(filename))
                            .await
                            .expect("change channel should still be open");
                    }
                    _ => unreachable!("should never recieve unregistered inotify event {event:?}"),
                }
            }
        });

        (Self {}, change_rx)
    }
}

#[derive(Debug)]
pub enum Change {
    Deletion(OsString),
    New(OsString),
    Rename { from: OsString, to: OsString },
}
