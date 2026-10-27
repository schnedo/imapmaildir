use std::{fmt::Debug, path::PathBuf, sync::Arc};

use crate::repository::{Keywords, Uid};

/// Change of the remote state, that got applied to the local maildir
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailChange {
    Downloaded {
        uid: Uid,
        path: PathBuf,
        flags: String,
        keywords: Keywords,
    },
    Uploaded {
        uid: Uid,
        from: PathBuf,
        to: PathBuf,
        flags: String,
    },
    Deleted {
        uid: Uid,
        path: PathBuf,
    },
    FlagsChanged {
        uid: Uid,
        from: PathBuf,
        to: PathBuf,
        old: String,
        new: String,
    },
    /// Keywords changed, which are not part of the file name
    KeywordsChanged {
        uid: Uid,
        path: PathBuf,
        added: Keywords,
        removed: Keywords,
    },
    /// Changes got dropped, so the mailbox needs to be rescanned
    Overflow,
}

pub trait ChangeSink: Send + Sync {
    /// Must not block, as it is called while syncing
    fn record(&self, change: MailChange);
}

/// Passes changes on to all configured sinks
#[derive(Clone, Default)]
pub struct ChangeRecorder {
    sinks: Arc<[Arc<dyn ChangeSink>]>,
}

impl ChangeRecorder {
    #[must_use]
    pub fn new(sinks: Vec<Arc<dyn ChangeSink>>) -> Self {
        Self {
            sinks: sinks.into(),
        }
    }

    pub fn record(&self, change: MailChange) {
        if let Some((last, others)) = self.sinks.split_last() {
            for sink in others {
                sink.record(change.clone());
            }
            last.record(change);
        }
    }
}

impl Debug for ChangeRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeRecorder")
            .field("sinks", &self.sinks.len())
            .finish()
    }
}
