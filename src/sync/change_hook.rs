use std::{
    collections::HashMap,
    ffi::OsString,
    fs::File,
    io::{self, BufRead as _, BufReader, Read as _},
    mem,
    os::fd::AsFd,
    pin::pin,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex, PoisonError},
    thread,
    time::{Duration, Instant},
};

use compio::{io::AsyncWriteExt as _, runtime::JoinHandle};
use crossfire::{MTx, mpsc};
use derive_getters::Getters;
use futures::{
    channel::oneshot,
    future::{self, Either},
};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    maildir::{ChangeSink, MailChange},
    repository::Uid,
    sync::Shutdown,
};

/// When batched changes get passed to a hook
#[derive(Getters, Debug, Clone, Copy)]
pub struct Batching {
    /// Time after the first change of a batch got recorded, until the hook runs
    #[getter(copy)]
    throttle: Duration,
    /// Changes queued, beyond which they get dropped for a single overflow
    #[getter(copy)]
    max_pending: usize,
}

impl Batching {
    #[must_use]
    pub fn new(throttle: Duration, max_pending: usize) -> Self {
        Self {
            throttle,
            max_pending,
        }
    }
}

impl Default for Batching {
    fn default() -> Self {
        Self::new(Duration::from_millis(100), 10_000)
    }
}

/// What to do, if a change hook fails
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookFailure {
    /// Stop syncing after finishing pending work and exit with an error
    #[default]
    Exit,
    /// Log a warning and continue syncing
    Warn,
}

#[derive(Debug, Error)]
pub enum HookError {
    #[error("cannot start hook: {0}")]
    Spawn(io::Error),
    #[error("cannot write changes to hook: {0}")]
    Write(io::Error),
    #[error("cannot wait for hook: {0}")]
    Wait(io::Error),
    #[error("hook failed with {0}")]
    Status(ExitStatus),
}

/// Called with the failure of a hook. The hook keeps running, if it returns.
pub type FailureHandler = Arc<dyn Fn(HookError) + Send + Sync>;

/// Handles hook failures according to `failure`. Fails `shutdown` to stop syncing.
#[must_use]
pub fn failure_handler(
    name: &'static str,
    failure: HookFailure,
    shutdown: Shutdown,
) -> FailureHandler {
    Arc::new(move |error| match failure {
        HookFailure::Exit => {
            log::error!("{name} hook failed: {error}. Stopping");
            shutdown.fail();
        }
        HookFailure::Warn => log::warn!("{name} hook failed: {error}"),
    })
}

fn escape(value: &str) -> String {
    value
        .replace('\\', r"\\")
        .replace('\t', r"\t")
        .replace('\n', r"\n")
}

fn flag_difference(flags: &str, other: &str) -> String {
    flags
        .chars()
        .filter(|flag| !other.contains(*flag))
        .collect()
}

/// Formats `change` as `<mailbox>\t<type>\t<path>\t<details>\t<old_path>\n`
fn format_line(mailbox: &str, change: &MailChange) -> String {
    let (kind, path, details, old_path) = match change {
        MailChange::Downloaded {
            uid, path, flags, ..
        } => (
            "downloaded",
            Some(path),
            format!("uid={uid} flags={flags}"),
            None,
        ),
        MailChange::Uploaded {
            uid,
            from,
            to,
            flags,
        } => (
            "uploaded",
            Some(to),
            format!("uid={uid} flags={flags}"),
            Some(from),
        ),
        MailChange::Deleted { uid, path } => ("deleted", Some(path), format!("uid={uid}"), None),
        MailChange::FlagsChanged {
            uid,
            from,
            to,
            old,
            new,
        } => (
            "flags",
            Some(to),
            format!(
                "uid={uid} added={} removed={}",
                flag_difference(new, old),
                flag_difference(old, new)
            ),
            Some(from),
        ),
        MailChange::Overflow => ("overflow", None, String::new(), None),
        MailChange::KeywordsChanged { .. } => {
            unreachable!("keyword changes should not be batched")
        }
    };
    let path = path.map(|path| escape(&path.to_string_lossy()));
    let old_path = old_path.map(|path| escape(&path.to_string_lossy()));

    format!(
        "{}\t{kind}\t{}\t{details}\t{}\n",
        escape(mailbox),
        path.unwrap_or_default(),
        old_path.unwrap_or_default()
    )
}

fn uid_of(change: &MailChange) -> Option<Uid> {
    match change {
        MailChange::Downloaded { uid, .. }
        | MailChange::Uploaded { uid, .. }
        | MailChange::Deleted { uid, .. }
        | MailChange::FlagsChanged { uid, .. }
        | MailChange::KeywordsChanged { uid, .. } => Some(*uid),
        MailChange::Overflow => None,
    }
}

/// Changes of a mailbox, merged per mail. Holds at most `max_pending` changes and drops all of
/// them for a single overflow beyond that.
#[derive(Debug)]
struct ChangeBatch {
    changes: Vec<Option<MailChange>>,
    positions: HashMap<Uid, usize>,
    len: usize,
    overflowed: bool,
    max_pending: usize,
    /// When the first change of the batch got recorded
    first_recorded: Option<Instant>,
}

impl ChangeBatch {
    fn new(max_pending: usize) -> Self {
        Self {
            changes: Vec::new(),
            positions: HashMap::new(),
            len: 0,
            overflowed: false,
            max_pending,
            first_recorded: None,
        }
    }

    fn is_empty(&self) -> bool {
        !self.overflowed && self.len == 0
    }

    fn overflow(&mut self) {
        self.overflowed = true;
        self.changes = Vec::new();
        self.positions = HashMap::new();
        self.len = 0;
    }

    /// Ignores keyword changes, as they are not part of the reported lines
    fn add(&mut self, change: MailChange) {
        if self.overflowed || matches!(change, MailChange::KeywordsChanged { .. }) {
            return;
        }
        self.first_recorded.get_or_insert_with(Instant::now);
        let Some(uid) = uid_of(&change) else {
            self.overflow();
            return;
        };
        // a mail keeps its slot, even if its changes cancel out, so that repeated changes of the
        // same mail cannot grow the batch beyond its distinct mails
        let position = *self.positions.entry(uid).or_insert_with(|| {
            self.changes.push(None);
            self.changes.len() - 1
        });
        if let Some(previous) = self.changes[position].take() {
            let merged = Self::merge(previous, change);
            if merged.is_none() {
                self.len -= 1;
            }
            self.changes[position] = merged;
        } else {
            self.changes[position] = Some(change);
            self.len += 1;
            if self.len > self.max_pending {
                self.overflow();
            }
        }
    }

    /// Merges `next` into `previous` change of the same mail. `None` if nothing changed overall.
    fn merge(previous: MailChange, next: MailChange) -> Option<MailChange> {
        match (previous, next) {
            (
                MailChange::Downloaded { uid, keywords, .. },
                MailChange::FlagsChanged {
                    to: path,
                    new: flags,
                    ..
                },
            ) => Some(MailChange::Downloaded {
                uid,
                path,
                flags,
                keywords,
            }),
            (
                MailChange::Uploaded { uid, from, .. },
                MailChange::FlagsChanged { to, new: flags, .. },
            ) => Some(MailChange::Uploaded {
                uid,
                from,
                to,
                flags,
            }),
            (
                MailChange::FlagsChanged { uid, from, old, .. },
                MailChange::FlagsChanged { to, new, .. },
            ) => (old != new).then_some(MailChange::FlagsChanged {
                uid,
                from,
                to,
                old,
                new,
            }),
            (MailChange::Downloaded { .. }, MailChange::Deleted { .. }) => None,
            (_, next) => Some(next),
        }
    }

    fn into_lines(self, mailbox: &str) -> String {
        if self.overflowed {
            format_line(mailbox, &MailChange::Overflow)
        } else {
            self.changes
                .iter()
                .flatten()
                .map(|change| format_line(mailbox, change))
                .collect()
        }
    }
}

struct BatchSink {
    batch: Arc<Mutex<ChangeBatch>>,
    changed: MTx<mpsc::Array<()>>,
}

impl ChangeSink for BatchSink {
    fn record(&self, change: MailChange) {
        self.batch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .add(change);
        // a full channel already announces a change, a closed one has nobody to tell
        let _ = self.changed.try_send(());
    }
}

/// Runs `cmd` once per batch of changes of `mailbox`, passing them on stdin. A batch gets handled
/// the configured throttle after its first change got recorded. The returned task finishes after
/// the sink got dropped and the last batch got handled.
pub fn batched_hook(
    cmd: &[OsString],
    mailbox: &str,
    batching: Batching,
    on_failure: FailureHandler,
) -> (Arc<dyn ChangeSink>, JoinHandle<()>) {
    let (changed_tx, changed_rx) = mpsc::bounded_blocking_async(1);
    let max_pending = batching.max_pending();
    let batch = Arc::new(Mutex::new(ChangeBatch::new(max_pending)));
    let sink = Arc::new(BatchSink {
        batch: batch.clone(),
        changed: changed_tx,
    });
    let cmd = cmd.to_vec();
    let mailbox = mailbox.to_string();
    let handle = compio::runtime::spawn(async move {
        loop {
            let mut closed = changed_rx.recv().await.is_err();
            let first_recorded = batch
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .first_recorded;
            if let Some(first_recorded) = first_recorded {
                // changes recorded while the previous hook ran wait only for the rest of it
                let deadline = first_recorded + batching.throttle();
                while !closed {
                    match compio::time::timeout_at(deadline, changed_rx.recv()).await {
                        Err(_) => break,
                        Ok(received) => closed = received.is_err(),
                    }
                }
            } else if !closed {
                // notified of a change, that got handled with the previous batch already
                continue;
            }
            // changes recorded while the hook runs go into the next batch
            let pending = mem::replace(
                &mut *batch.lock().unwrap_or_else(PoisonError::into_inner),
                ChangeBatch::new(max_pending),
            );
            run_batch(&cmd, &mailbox, pending, &on_failure).await;
            if closed {
                break;
            }
        }
    });

    (sink, handle)
}

async fn run_batch(
    cmd: &[OsString],
    mailbox: &str,
    batch: ChangeBatch,
    on_failure: &FailureHandler,
) {
    if batch.is_empty() {
        return;
    }
    let Some((executable, args)) = cmd.split_first() else {
        return;
    };
    log::info!("calling on_local_change hook for {mailbox}");
    if let Err(e) = call_with_input(executable, args, batch.into_lines(mailbox), mailbox).await {
        on_failure(e);
    }
}

/// Lines of hook output get logged in parts of at most this many bytes
const MAX_OUTPUT_LINE: u64 = 64 * 1024;

/// Time to log output, that a hook wrote right before exiting
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

async fn call_with_input(
    executable: &OsString,
    args: &[OsString],
    input: String,
    mailbox: &str,
) -> Result<(), HookError> {
    let mut command = compio::process::Command::new(executable);
    command.args(args);
    // signals of a terminal meant for imapmaildir must not reach hooks, which finish their batch
    // while shutting down. Without the terminal's process group, they must not use the terminal
    // either, so their output gets logged.
    command.process_group(0);
    let piped = |command: &mut compio::process::Command| {
        command.stdin(Stdio::piped())?;
        command.stdout(Stdio::piped())?;
        command.stderr(Stdio::piped())?;
        Ok::<_, std::convert::Infallible>(())
    };
    piped(&mut command).map_err(|e| HookError::Spawn(io::Error::other(e.to_string())))?;
    let mut child = command.spawn().map_err(HookError::Spawn)?;
    let stdout_logged = log_output(child.stdout.take(), mailbox, log::Level::Info);
    let stderr_logged = log_output(child.stderr.take(), mailbox, log::Level::Warn);
    let stdin = child.stdin.take();
    let writing = async move {
        let Some(mut stdin) = stdin else {
            return Ok(());
        };
        let compio::BufResult(written, _) = stdin.write_all(input).await;
        // closing stdin lets the hook see the end of its input
        written
    };
    // processes started by a hook, that exited without reading its input, may keep stdin open
    let (written, status) = match future::select(pin!(writing), pin!(child.wait())).await {
        Either::Left((written, exited)) => (written, exited.await),
        Either::Right((status, _)) => (Ok(()), status),
    };
    // output written right before exiting, e.g. explaining a failure, belongs to this call, but
    // processes started by the hook may keep the output open
    let _ = compio::time::timeout(
        OUTPUT_DRAIN_TIMEOUT,
        future::join(stdout_logged, stderr_logged),
    )
    .await;
    let status = status.map_err(HookError::Wait)?;
    match written {
        // one-shot hooks may not care about the changes
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
        Err(e) => return Err(HookError::Write(e)),
        Ok(()) => {}
    }
    if status.success() {
        Ok(())
    } else {
        Err(HookError::Status(status))
    }
}

/// Logs the lines of a hook's `output` at `level` on a separate thread, until it got closed.
/// Processes started by the hook may keep writing to it after the hook exited, which must neither
/// block the hook nor fail them. The returned receiver completes, once the output got closed.
fn log_output(
    output: Option<impl AsFd>,
    mailbox: &str,
    level: log::Level,
) -> oneshot::Receiver<()> {
    // dropping the sender completes the receiver
    let (closed_tx, closed_rx) = oneshot::channel::<()>();
    let Some(output) = output else {
        return closed_rx;
    };
    let output = match output.as_fd().try_clone_to_owned() {
        Ok(output) => File::from(output),
        Err(e) => {
            log::warn!("cannot log on_local_change hook output for {mailbox}: {e}");
            return closed_rx;
        }
    };
    let mailbox = mailbox.to_string();
    let logging = thread::Builder::new()
        .name(format!("{mailbox} hook"))
        .spawn(move || {
            let _closed_tx = closed_tx;
            let mut output = BufReader::new(output);
            let mut line = Vec::new();
            let mut split = false;
            loop {
                line.clear();
                match output
                    .by_ref()
                    .take(MAX_OUTPUT_LINE)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) => return,
                    Ok(_) => {
                        let complete = line.last() == Some(&b'\n');
                        if complete {
                            line.pop();
                            if line.last() == Some(&b'\r') {
                                line.pop();
                            }
                        }
                        // the end of a line split at the limit
                        if !(split && line.is_empty()) {
                            log_line(&line, &mailbox, level);
                        }
                        split = !complete;
                    }
                    Err(e) => {
                        log::warn!("cannot read on_local_change hook output for {mailbox}: {e}");
                        return;
                    }
                }
            }
        });
    if let Err(e) = logging {
        log::warn!("cannot log on_local_change hook output: {e}");
    }

    closed_rx
}

fn log_line(line: &[u8], mailbox: &str, level: log::Level) {
    log::log!(
        level,
        "on_local_change hook for {mailbox}: {}",
        String::from_utf8_lossy(line)
    );
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use assertables::*;
    use rstest::*;
    use tempfile::TempDir;

    use super::*;
    use crate::repository::Keywords;

    fn uid(value: u32) -> Uid {
        assert_ok!(Uid::try_from(value))
    }

    fn downloaded(id: u32) -> MailChange {
        MailChange::Downloaded {
            uid: uid(id),
            path: PathBuf::from(format!("/m/cur/{id}:2,S")),
            flags: "S".to_string(),
            keywords: Keywords::default(),
        }
    }

    fn flags_changed(id: u32, old: &str, new: &str) -> MailChange {
        MailChange::FlagsChanged {
            uid: uid(id),
            from: PathBuf::from(format!("/m/cur/{id}:2,{old}")),
            to: PathBuf::from(format!("/m/cur/{id}:2,{new}")),
            old: old.to_string(),
            new: new.to_string(),
        }
    }

    fn deleted(id: u32) -> MailChange {
        MailChange::Deleted {
            uid: uid(id),
            path: PathBuf::from(format!("/m/cur/{id}:2,S")),
        }
    }

    fn command(script: &str) -> Vec<OsString> {
        ["sh", "-c", script]
            .into_iter()
            .map(OsString::from)
            .collect()
    }

    fn collecting_handler() -> (FailureHandler, Arc<Mutex<Vec<String>>>) {
        let failures = Arc::new(Mutex::new(Vec::new()));
        let collected = failures.clone();
        let handler: FailureHandler = Arc::new(move |error| {
            assert_ok!(collected.lock()).push(error.to_string());
        });

        (handler, failures)
    }

    #[rstest]
    #[case::downloaded(downloaded(3), "INBOX\tdownloaded\t/m/cur/3:2,S\tuid=3 flags=S\t\n")]
    #[case::deleted(deleted(3), "INBOX\tdeleted\t/m/cur/3:2,S\tuid=3\t\n")]
    #[case::flags(
        flags_changed(5, "RS", "FS"),
        "INBOX\tflags\t/m/cur/5:2,FS\tuid=5 added=F removed=R\t/m/cur/5:2,RS\n"
    )]
    #[case::uploaded(
        MailChange::Uploaded {
            uid: uid(7),
            from: PathBuf::from("/m/cur/x:2,"),
            to: PathBuf::from("/m/cur/x,U=7:2,"),
            flags: String::new(),
        },
        "INBOX\tuploaded\t/m/cur/x,U=7:2,\tuid=7 flags=\t/m/cur/x:2,\n"
    )]
    #[case::overflow(MailChange::Overflow, "INBOX\toverflow\t\t\t\n")]
    fn test_format_line(#[case] change: MailChange, #[case] expected: &str) {
        assert_eq!(format_line("INBOX", &change), expected);
    }

    #[test]
    fn test_format_line_escapes_separators() {
        let change = MailChange::Deleted {
            uid: uid(1),
            path: PathBuf::from("/m/a\tb\nc\\d"),
        };

        assert_eq!(
            format_line("a\tb", &change),
            "a\\tb\tdeleted\t/m/a\\tb\\nc\\\\d\tuid=1\t\n"
        );
    }

    #[test]
    fn test_batch_merges_flag_changes_into_download() {
        let mut batch = ChangeBatch::new(10);

        batch.add(downloaded(1));
        batch.add(flags_changed(1, "S", "FS"));

        assert_eq!(
            batch.into_lines("INBOX"),
            "INBOX\tdownloaded\t/m/cur/1:2,FS\tuid=1 flags=FS\t\n"
        );
    }

    #[test]
    fn test_batch_merges_flag_changes() {
        let mut batch = ChangeBatch::new(10);

        batch.add(flags_changed(1, "S", "FS"));
        batch.add(flags_changed(1, "FS", "F"));

        assert_eq!(
            batch.into_lines("INBOX"),
            "INBOX\tflags\t/m/cur/1:2,F\tuid=1 added=F removed=S\t/m/cur/1:2,S\n"
        );
    }

    #[test]
    fn test_batch_drops_reverted_flag_changes() {
        let mut batch = ChangeBatch::new(10);

        batch.add(flags_changed(1, "S", "FS"));
        batch.add(flags_changed(1, "FS", "S"));

        assert!(batch.is_empty());
    }

    #[test]
    fn test_batch_reuses_slot_of_reverted_changes() {
        let mut batch = ChangeBatch::new(10);

        for _ in 0..100 {
            batch.add(flags_changed(1, "S", "FS"));
            batch.add(flags_changed(1, "FS", "S"));
        }
        batch.add(flags_changed(1, "S", "FS"));

        assert_len_eq_x!(&batch.changes, 1);
        assert_eq!(
            batch.into_lines("INBOX"),
            "INBOX\tflags\t/m/cur/1:2,FS\tuid=1 added=F removed=\t/m/cur/1:2,S\n"
        );
    }

    #[test]
    fn test_batch_drops_deleted_download() {
        let mut batch = ChangeBatch::new(10);

        batch.add(downloaded(1));
        batch.add(flags_changed(1, "S", "FS"));
        batch.add(deleted(1));

        assert!(batch.is_empty());
    }

    #[test]
    fn test_batch_replaces_flag_change_by_deletion() {
        let mut batch = ChangeBatch::new(10);

        batch.add(flags_changed(1, "S", "FS"));
        batch.add(deleted(2));
        batch.add(deleted(1));

        assert_eq!(
            batch.into_lines("INBOX"),
            "INBOX\tdeleted\t/m/cur/1:2,S\tuid=1\t\nINBOX\tdeleted\t/m/cur/2:2,S\tuid=2\t\n"
        );
    }

    #[test]
    fn test_batch_overflows_beyond_max_pending() {
        let mut batch = ChangeBatch::new(2);

        batch.add(downloaded(1));
        batch.add(downloaded(2));
        batch.add(downloaded(3));
        batch.add(downloaded(4));

        assert!(!batch.is_empty());
        assert_eq!(batch.into_lines("INBOX"), "INBOX\toverflow\t\t\t\n");
    }

    #[test]
    fn test_batch_remembers_first_change() {
        let mut batch = ChangeBatch::new(10);
        assert_none!(batch.first_recorded);

        batch.add(flags_changed(1, "S", "FS"));
        let first = assert_some!(batch.first_recorded);
        batch.add(flags_changed(1, "FS", "S"));
        batch.add(downloaded(2));

        assert_eq!(batch.first_recorded, Some(first));
    }

    #[compio::test]
    async fn test_batched_hook_passes_batch_on_stdin() {
        let dir = assert_ok!(TempDir::new());
        let output = dir.path().join("out");
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command(&format!("cat >> '{}'", output.display())),
            "INBOX",
            Batching::default(),
            on_failure,
        );

        sink.record(downloaded(1));
        sink.record(deleted(2));
        drop(sink);
        handle
            .await
            .unwrap_or_else(|_| panic!("hook task panicked"));

        assert_eq!(
            assert_ok!(fs::read_to_string(&output)),
            "INBOX\tdownloaded\t/m/cur/1:2,S\tuid=1 flags=S\t\nINBOX\tdeleted\t/m/cur/2:2,S\tuid=2\t\n"
        );
        assert_is_empty!(assert_ok!(failures.lock()).as_slice());
    }

    #[compio::test]
    async fn test_batched_hook_reports_failure() {
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) =
            batched_hook(&command("exit 3"), "INBOX", Batching::default(), on_failure);

        sink.record(downloaded(1));
        drop(sink);
        handle
            .await
            .unwrap_or_else(|_| panic!("hook task panicked"));

        assert_len_eq_x!(assert_ok!(failures.lock()).as_slice(), 1);
    }

    #[compio::test]
    async fn test_batched_hook_runs_throttled_during_continuous_changes() {
        let dir = assert_ok!(TempDir::new());
        let output = dir.path().join("out");
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command(&format!("cat >> '{}'", output.display())),
            "INBOX",
            Batching::new(Duration::from_millis(100), 1000),
            on_failure,
        );

        let mut id = 0;
        while fs::read_to_string(&output).unwrap_or_default().is_empty() {
            id += 1;
            // a debounce would never run the hook, as changes keep coming
            assert_lt!(id, 250);
            sink.record(downloaded(id));
            compio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(sink);
        handle
            .await
            .unwrap_or_else(|_| panic!("hook task panicked"));
        assert_is_empty!(assert_ok!(failures.lock()).as_slice());
    }

    #[compio::test]
    async fn test_batched_hook_ignores_hook_not_reading_stdin() {
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command("true"),
            "INBOX",
            Batching::new(Duration::from_millis(100), 100_000),
            on_failure,
        );

        for id in 1..=10_000 {
            sink.record(downloaded(id));
        }
        drop(sink);
        handle
            .await
            .unwrap_or_else(|_| panic!("hook task panicked"));

        assert_is_empty!(assert_ok!(failures.lock()).as_slice());
    }

    #[compio::test]
    async fn test_batched_hook_does_not_wait_for_processes_keeping_its_output_open() {
        let dir = assert_ok!(TempDir::new());
        let marker = dir.path().join("written late");
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command(&format!(
                "cat > /dev/null; echo done; printf oops >&2; (sleep 1; echo late; touch '{}') &",
                marker.display()
            )),
            "INBOX",
            Batching::new(Duration::ZERO, 100),
            on_failure,
        );
        let started = Instant::now();

        sink.record(downloaded(1));
        drop(sink);
        handle
            .await
            .unwrap_or_else(|_| panic!("hook task panicked"));

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_is_empty!(assert_ok!(failures.lock()).as_slice());
        // writing after the hook exited must not kill the started process
        compio::time::sleep(Duration::from_secs(2)).await;
        assert!(marker.exists());
    }

    #[compio::test]
    async fn test_batched_hook_does_not_wait_for_processes_keeping_its_input_open() {
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            // background jobs get /dev/null as stdin without redirecting it explicitly
            &command("exec 3<&0; sleep 3 <&3 & exit 0"),
            "INBOX",
            Batching::new(Duration::ZERO, 100_000),
            on_failure,
        );
        let started = Instant::now();

        // more than fits into the pipe to the hook
        for id in 1..=10_000 {
            sink.record(downloaded(id));
        }
        drop(sink);
        handle
            .await
            .unwrap_or_else(|_| panic!("hook task panicked"));

        assert!(started.elapsed() < Duration::from_secs(2));
        assert_is_empty!(assert_ok!(failures.lock()).as_slice());
    }
}
