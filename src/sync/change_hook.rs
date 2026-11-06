use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsString,
    fs::File,
    io::{self, BufRead as _, BufReader, Read as _},
    mem,
    os::fd::AsFd,
    pin::pin,
    process::{ExitStatus, Stdio},
    rc::Rc,
    sync::Arc,
    thread,
    time::Duration,
};

use compio::{io::AsyncWriteExt as _, runtime::JoinHandle};
use crossfire::{MTx, mpsc, oneshot::RxOneshot};
use futures::{
    channel::oneshot,
    future::{self, Either},
};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    maildir::{ChangeSink, MailChange},
    repository::Uid,
    sync::{HookRunner, Shutdown},
};

/// Changes a mailbox collects before it queues them for the hook, unless set otherwise
pub const DEFAULT_MAX_PENDING: usize = 10_000;

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

/// Failures of running the change hook
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
    #[error("hook did not finish within {0:?}")]
    Timeout(Duration),
    #[error("hook runner stopped")]
    RunnerStopped,
}

/// Called with the failure of a hook. The hook keeps running, if it returns.
pub type FailureHandler = Arc<dyn Fn(&HookError) + Send + Sync>;

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
        MailChange::Initial => ("initial", None, String::new(), None),
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
        MailChange::Initial => None,
    }
}

/// Changes of a mailbox, merged per mail. Is full at `max_pending` changes. Reports a single
/// initial sync instead of its changes, if it got told so.
#[derive(Debug)]
pub(super) struct ChangeBatch {
    changes: Vec<Option<MailChange>>,
    positions: HashMap<Uid, usize>,
    len: usize,
    initial: bool,
    max_pending: usize,
}

impl ChangeBatch {
    fn new(max_pending: usize) -> Self {
        Self {
            changes: Vec::new(),
            positions: HashMap::new(),
            len: 0,
            initial: false,
            max_pending,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.initial && self.len == 0
    }

    /// Whether the batch reached the limit of changes it should hold
    fn is_full(&self) -> bool {
        self.len >= self.max_pending
    }

    /// Replaces all changes by the report of an initial sync
    fn mark_initial(&mut self) {
        self.initial = true;
        self.changes = Vec::new();
        self.positions = HashMap::new();
        self.len = 0;
    }

    /// Ignores keyword changes, as they are not part of the reported lines
    fn add(&mut self, change: MailChange) {
        if self.initial || matches!(change, MailChange::KeywordsChanged { .. }) {
            return;
        }
        let Some(uid) = uid_of(&change) else {
            self.mark_initial();
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

    pub(super) fn into_lines(self, mailbox: &str) -> String {
        if self.initial {
            format_line(mailbox, &MailChange::Initial)
        } else {
            self.changes
                .iter()
                .flatten()
                .map(|change| format_line(mailbox, change))
                .collect()
        }
    }
}

/// Collects changes into a batch, which it passes on to the hook runner, once it is full or got
/// flushed. Flushes when dropped.
struct BatchSink {
    batch: RefCell<ChangeBatch>,
    cmd: Vec<OsString>,
    mailbox: String,
    timeout: Option<Duration>,
    on_failure: FailureHandler,
    runner: HookRunner,
    /// Tells the task awaiting the hook about batches, that got passed on
    passed_on: MTx<mpsc::List<RxOneshot<()>>>,
}

impl BatchSink {
    /// Passes the batch on to the runner, if it has changes
    fn pass_on(&self) {
        let max_pending = self.batch.borrow().max_pending;
        let batch = mem::replace(&mut *self.batch.borrow_mut(), ChangeBatch::new(max_pending));
        if batch.is_empty() {
            return;
        }
        match self.runner.queue(
            &self.cmd,
            &self.mailbox,
            batch,
            self.timeout,
            &self.on_failure,
        ) {
            Ok(handled) => {
                // closed, if the task got dropped, which has nobody to tell then
                let _ = self.passed_on.send(handled);
            }
            Err(e) => (self.on_failure)(&e),
        }
    }
}

impl ChangeSink for BatchSink {
    fn record(&self, change: MailChange) {
        let full = {
            let mut batch = self.batch.borrow_mut();
            batch.add(change);
            batch.is_full()
        };
        if full {
            self.pass_on();
        }
    }

    fn flush(&self) {
        self.pass_on();
    }
}

impl Drop for BatchSink {
    fn drop(&mut self) {
        self.pass_on();
    }
}

/// Collects the changes of `mailbox` and runs `cmd` with them on stdin, once a batch is full, got
/// flushed or the sink got dropped. The hooks of all mailboxes sharing `runner` run one at a time,
/// and share the next run, if they wait. A hook running longer than `timeout` gets killed and
/// fails. A batch is full at `max_pending` changes. The returned task finishes after the sink got
/// dropped and the hook handled the last batch.
pub fn batched_hook(
    cmd: &[OsString],
    mailbox: &str,
    max_pending: usize,
    timeout: Option<Duration>,
    on_failure: FailureHandler,
    runner: HookRunner,
) -> (Rc<dyn ChangeSink>, JoinHandle<()>) {
    let (passed_on_tx, passed_on_rx) = mpsc::unbounded_async::<RxOneshot<()>>();
    let sink = Rc::new(BatchSink {
        batch: RefCell::new(ChangeBatch::new(max_pending)),
        cmd: cmd.to_vec(),
        mailbox: mailbox.to_string(),
        timeout,
        on_failure: on_failure.clone(),
        runner,
        passed_on: passed_on_tx,
    });
    let handle = compio::runtime::spawn(async move {
        // ends with the sink, after all batches it passed on got handled
        while let Ok(handled) = passed_on_rx.recv().await {
            if handled.await.is_err() {
                on_failure(&HookError::RunnerStopped);
            }
        }
    });

    (sink, handle)
}

/// Calls the hook once with the changes of all `batches`, each of a mailbox. Does nothing, if they
/// are empty.
pub(super) async fn run_batches(
    cmd: &[OsString],
    batches: Vec<(String, ChangeBatch)>,
    timeout: Option<Duration>,
) -> Result<(), HookError> {
    let Some((executable, args)) = cmd.split_first() else {
        return Ok(());
    };
    let (mailboxes, input): (Vec<_>, String) = batches
        .into_iter()
        .filter(|(_, batch)| !batch.is_empty())
        .map(|(mailbox, batch)| {
            let lines = batch.into_lines(&mailbox);
            (mailbox, lines)
        })
        .unzip();
    if input.is_empty() {
        return Ok(());
    }
    let mailboxes = mailboxes.join(", ");
    log::info!("calling on_local_change hook for {mailboxes}");

    call_with_input(executable, args, input, &mailboxes, timeout).await
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
    timeout: Option<Duration>,
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
    let pid = child.id();
    // processes started by a hook, that exited without reading its input, may keep stdin open
    let mut exit = pin!(async {
        match future::select(pin!(writing), pin!(child.wait())).await {
            Either::Left((written, exited)) => (written, exited.await),
            Either::Right((status, _)) => (Ok(()), status),
        }
    });
    let (written, status, timed_out) = if let Some(limit) = timeout {
        if let Ok((written, status)) = compio::time::timeout(limit, exit.as_mut()).await {
            (written, status, false)
        } else {
            kill_group(pid);
            // reaps the killed hook
            let (written, status) = exit.await;
            (written, status, true)
        }
    } else {
        let (written, status) = exit.await;
        (written, status, false)
    };
    // output written right before exiting, e.g. explaining a failure, belongs to this call, but
    // processes started by the hook may keep the output open
    let _ = compio::time::timeout(
        OUTPUT_DRAIN_TIMEOUT,
        future::join(stdout_logged, stderr_logged),
    )
    .await;
    if timed_out && let Some(limit) = timeout {
        return Err(HookError::Timeout(limit));
    }
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

/// Kills the process group of the hook `pid`, as hooks run in a group of their own
fn kill_group(pid: u32) {
    let Some(group) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    if let Err(e) = rustix::process::kill_process_group(group, rustix::process::Signal::KILL) {
        log::warn!("cannot kill hook {pid}: {e}");
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
    use std::{fs, path::PathBuf, sync::Mutex, time::Instant};

    use assertables::*;
    use rstest::*;
    use tempfile::TempDir;

    use super::*;
    use crate::repository::Keywords;
    use crate::sync::HookThread;

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
    #[case::initial(MailChange::Initial, "INBOX\tinitial\t\t\t\n")]
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
    fn test_batch_is_full_at_max_pending() {
        let mut batch = ChangeBatch::new(2);

        batch.add(downloaded(1));
        assert!(!batch.is_full());
        batch.add(downloaded(2));

        assert!(batch.is_full());
        assert_eq!(
            batch.into_lines("INBOX"),
            "INBOX\tdownloaded\t/m/cur/1:2,S\tuid=1 flags=S\t\nINBOX\tdownloaded\t/m/cur/2:2,S\tuid=2 flags=S\t\n"
        );
    }

    #[test]
    fn test_batch_reports_initial_sync_instead_of_changes() {
        let mut batch = ChangeBatch::new(10);

        batch.add(downloaded(1));
        batch.add(MailChange::Initial);
        batch.add(downloaded(2));

        assert_eq!(batch.into_lines("INBOX"), "INBOX\tinitial\t\t\t\n");
    }

    #[compio::test]
    async fn test_batched_hook_passes_batch_on_stdin() {
        let hooks = assert_ok!(HookThread::start());
        let dir = assert_ok!(TempDir::new());
        let output = dir.path().join("out");
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command(&format!("cat >> '{}'", output.display())),
            "INBOX",
            10_000,
            None,
            on_failure,
            hooks.runner(),
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
        let hooks = assert_ok!(HookThread::start());
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command("exit 3"),
            "INBOX",
            10_000,
            None,
            on_failure,
            hooks.runner(),
        );

        sink.record(downloaded(1));
        drop(sink);
        handle
            .await
            .unwrap_or_else(|_| panic!("hook task panicked"));

        assert_len_eq_x!(assert_ok!(failures.lock()).as_slice(), 1);
    }

    #[compio::test]
    async fn test_batched_hook_passes_full_batch_on_without_waiting_for_the_sink() {
        let hooks = assert_ok!(HookThread::start());
        let dir = assert_ok!(TempDir::new());
        let output = dir.path().join("out");
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command(&format!("cat >> '{}'", output.display())),
            "INBOX",
            2,
            None,
            on_failure,
            hooks.runner(),
        );

        for id in 1..=3 {
            sink.record(downloaded(id));
        }
        while fs::read_to_string(&output).unwrap_or_default().is_empty() {
            compio::time::sleep(Duration::from_millis(10)).await;
        }
        // the third change is still held back
        assert_eq!(assert_ok!(fs::read_to_string(&output)).lines().count(), 2);
        drop(sink);
        handle
            .await
            .unwrap_or_else(|_| panic!("hook task panicked"));

        assert_eq!(assert_ok!(fs::read_to_string(&output)).lines().count(), 3);
        assert_is_empty!(assert_ok!(failures.lock()).as_slice());
    }

    #[compio::test]
    async fn test_batched_hook_passes_changes_on_when_flushed() {
        let hooks = assert_ok!(HookThread::start());
        let dir = assert_ok!(TempDir::new());
        let output = dir.path().join("out");
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command(&format!("cat >> '{}'", output.display())),
            "INBOX",
            100,
            None,
            on_failure,
            hooks.runner(),
        );

        sink.record(downloaded(1));
        assert!(!output.exists());
        sink.flush();
        while fs::read_to_string(&output).unwrap_or_default().is_empty() {
            compio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(sink);
        handle
            .await
            .unwrap_or_else(|_| panic!("hook task panicked"));

        assert_eq!(assert_ok!(fs::read_to_string(&output)).lines().count(), 1);
        assert_is_empty!(assert_ok!(failures.lock()).as_slice());
    }

    #[compio::test]
    async fn test_batched_hook_ignores_hook_not_reading_stdin() {
        let hooks = assert_ok!(HookThread::start());
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command("true"),
            "INBOX",
            100_000,
            None,
            on_failure,
            hooks.runner(),
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
        let hooks = assert_ok!(HookThread::start());
        let dir = assert_ok!(TempDir::new());
        let marker = dir.path().join("written late");
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command(&format!(
                "cat > /dev/null; echo done; printf oops >&2; (sleep 1; echo late; touch '{}') &",
                marker.display()
            )),
            "INBOX",
            100,
            None,
            on_failure,
            hooks.runner(),
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
        let hooks = assert_ok!(HookThread::start());
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            // background jobs get /dev/null as stdin without redirecting it explicitly
            &command("exec 3<&0; sleep 3 <&3 & exit 0"),
            "INBOX",
            100_000,
            None,
            on_failure,
            hooks.runner(),
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

    /// A hook, that logs when it starts and ends, and keeps its input
    fn logging_hook(dir: &TempDir) -> (Vec<OsString>, PathBuf, PathBuf) {
        let log = dir.path().join("log");
        let input = dir.path().join("input");
        let script = format!(
            "echo start >> '{0}'; cat >> '{1}'; sleep 0.3; echo end >> '{0}'",
            log.display(),
            input.display()
        );

        (command(&script), log, input)
    }

    #[compio::test]
    async fn test_batched_hooks_of_mailboxes_sharing_a_runner_do_not_overlap() {
        let thread = assert_ok!(HookThread::start());
        let dir = assert_ok!(TempDir::new());
        let (cmd, log, input) = logging_hook(&dir);
        let (on_failure, failures) = collecting_handler();
        let tasks: Vec<_> = ["INBOX", "DRAFT", "SENT"]
            .into_iter()
            .map(|mailbox| {
                let (sink, task) = batched_hook(
                    &cmd,
                    mailbox,
                    100,
                    None,
                    on_failure.clone(),
                    thread.runner(),
                );
                sink.record(downloaded(1));
                task
            })
            .collect();
        for task in tasks {
            task.await.unwrap_or_else(|_| panic!("hook task panicked"));
        }

        let log = assert_ok!(fs::read_to_string(&log));
        assert!(!log.contains("start\nstart"), "hooks overlapped: {log}");
        assert!(!log.contains("end\nend"), "hooks overlapped: {log}");
        let input = assert_ok!(fs::read_to_string(&input));
        for mailbox in ["INBOX", "DRAFT", "SENT"] {
            assert_contains!(input, &format!("{mailbox}\tdownloaded"));
        }
        assert_is_empty!(assert_ok!(failures.lock()).as_slice());
    }

    #[compio::test]
    async fn test_batched_hooks_of_mailboxes_waiting_for_a_hook_share_the_next_run() {
        let thread = assert_ok!(HookThread::start());
        let dir = assert_ok!(TempDir::new());
        let (cmd, log, input) = logging_hook(&dir);
        let (on_failure, failures) = collecting_handler();
        let hook = |mailbox: &str| {
            batched_hook(
                &cmd,
                mailbox,
                100,
                None,
                on_failure.clone(),
                thread.runner(),
            )
        };
        let (first_sink, first_task) = hook("INBOX");
        first_sink.record(downloaded(1));
        first_sink.flush();
        while !fs::read_to_string(&log).is_ok_and(|log| log.contains("start")) {
            compio::time::sleep(Duration::from_millis(10)).await;
        }
        // the first hook is running, so these two have to wait for it
        let (draft_sink, draft_task) = hook("DRAFT");
        let (sent_sink, sent_task) = hook("SENT");
        draft_sink.record(downloaded(2));
        sent_sink.record(downloaded(3));
        drop((first_sink, draft_sink, sent_sink));

        for task in [first_task, draft_task, sent_task] {
            task.await.unwrap_or_else(|_| panic!("hook task panicked"));
        }

        assert_eq!(
            assert_ok!(fs::read_to_string(&log)),
            "start\nend\nstart\nend\n"
        );
        let input = assert_ok!(fs::read_to_string(&input));
        assert_eq!(input.lines().count(), 3);
        assert_is_empty!(assert_ok!(failures.lock()).as_slice());
    }

    #[test]
    fn test_hook_thread_stops_after_dropping_runners() {
        let hooks = assert_ok!(HookThread::start());
        let runner = hooks.runner();

        drop(runner);
        // would hang, if the thread did not stop
        drop(hooks);
    }

    #[compio::test]
    async fn test_batched_hook_kills_hook_running_too_long() {
        let hooks = assert_ok!(HookThread::start());
        let (on_failure, failures) = collecting_handler();
        let (sink, handle) = batched_hook(
            &command("sleep 30"),
            "INBOX",
            100,
            Some(Duration::from_millis(200)),
            on_failure,
            hooks.runner(),
        );
        let started = Instant::now();

        sink.record(downloaded(1));
        drop(sink);
        handle
            .await
            .unwrap_or_else(|_| panic!("hook task panicked"));

        assert!(started.elapsed() < Duration::from_secs(5));
        let failures = assert_ok!(failures.lock());
        assert_len_eq_x!(failures.as_slice(), 1);
        assert_starts_with!(failures[0], "hook did not finish within");
    }
}
