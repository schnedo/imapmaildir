use std::{fmt::Debug, path::PathBuf, rc::Rc};

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

/// Receiver of recorded mail changes, e.g. to forward them to hooks or notmuch
pub trait ChangeSink {
    /// Must not block, as it is called while syncing
    fn record(&self, change: MailChange);

    /// Passes on changes the sink holds back, e.g. at the end of a sync round. Must not block.
    fn flush(&self) {}
}

/// Passes changes on to all configured sinks
#[derive(Clone, Default)]
pub struct ChangeRecorder {
    sinks: Rc<[Rc<dyn ChangeSink>]>,
}

impl ChangeRecorder {
    /// Creates a recorder forwarding to `sinks`
    #[must_use]
    pub fn new(sinks: Vec<Rc<dyn ChangeSink>>) -> Self {
        Self {
            sinks: sinks.into(),
        }
    }

    /// Tells every sink to pass on the changes it holds back
    pub fn flush(&self) {
        for sink in self.sinks.iter() {
            sink.flush();
        }
    }

    /// Hands the change to every sink
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[derive(Default)]
    struct CountingSink {
        recorded: Cell<usize>,
        flushed: Cell<usize>,
    }

    impl ChangeSink for CountingSink {
        fn record(&self, _change: MailChange) {
            self.recorded.set(self.recorded.get() + 1);
        }

        fn flush(&self) {
            self.flushed.set(self.flushed.get() + 1);
        }
    }

    #[test]
    fn test_recorder_flushes_all_sinks() {
        let first = Rc::new(CountingSink::default());
        let second = Rc::new(CountingSink::default());
        let recorder = ChangeRecorder::new(vec![first.clone(), second.clone()]);

        recorder.record(MailChange::Overflow);
        recorder.flush();

        assert_eq!((first.recorded.get(), first.flushed.get()), (1, 1));
        assert_eq!((second.recorded.get(), second.flushed.get()), (1, 1));
    }
}
