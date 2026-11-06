use std::{
    ffi::OsString,
    fmt, io, mem,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread,
    time::Duration,
};

use crossfire::{
    MTx, mpsc,
    oneshot::{RxOneshot, TxOneshot, oneshot},
};

use crate::sync::change_hook::{ChangeBatch, FailureHandler, HookError, run_batches};

/// Changes of a mailbox, that wait for their turn to be passed to the hook. Batches of the same
/// mailbox, that queue while waiting, merge, so that waiting needs no more memory than its
/// distinct mails.
struct PendingJob {
    mailbox: Arc<str>,
    batch: ChangeBatch,
    /// Complete, once the hook handled the batch
    done: Vec<TxOneshot<()>>,
}

type Pending = Arc<Mutex<Vec<PendingJob>>>;

fn lock(pending: &Pending) -> MutexGuard<'_, Vec<PendingJob>> {
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Queues hooks of all mailboxes, which run one at a time. Batches waiting for their turn share the
/// next run. Clones queue to the same [`HookThread`].
#[derive(Clone)]
pub struct HookRunner {
    pending: Pending,
    /// Tells the thread about jobs, that got added to `pending`
    wake: MTx<mpsc::List<()>>,
    on_failure: FailureHandler,
}

impl fmt::Debug for HookRunner {
    // the queued jobs are no `Debug`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookRunner").finish_non_exhaustive()
    }
}

impl HookRunner {
    /// Called with the failure of a hook
    pub(super) fn on_failure(&self) -> &FailureHandler {
        &self.on_failure
    }

    /// Queues `batch` for the hook. The returned receiver completes, once the hook handled it,
    /// and fails if the runner stopped before. A batch of a mailbox, that is still waiting,
    /// merges with `batch`.
    ///
    /// # Errors
    /// If the runner stopped
    pub(super) fn queue(
        &self,
        mailbox: &Arc<str>,
        batch: ChangeBatch,
    ) -> Result<RxOneshot<()>, HookError> {
        let (done_tx, done_rx): (_, RxOneshot<()>) = oneshot();
        let mut pending = lock(&self.pending);
        if let Some(job) = pending.iter_mut().find(|job| job.mailbox == *mailbox) {
            job.batch.absorb(batch);
            job.done.push(done_tx);

            return Ok(done_rx);
        }
        pending.push(PendingJob {
            mailbox: Arc::clone(mailbox),
            batch,
            done: vec![done_tx],
        });
        if self.wake.send(()).is_err() {
            pending.pop();
            return Err(HookError::RunnerStopped);
        }

        Ok(done_rx)
    }
}

/// Owns the thread, that runs the hooks queued by its [`HookRunner`]s on a runtime of its own.
/// Dropping it waits for the thread to finish all queued hooks, which needs all runners to be
/// dropped before.
#[derive(Debug)]
pub struct HookThread {
    /// Dropped before joining the thread, as the thread ends after the last runner
    runner: Option<HookRunner>,
    thread: Option<thread::JoinHandle<()>>,
}

/// Runs `cmd` once for all `jobs`. Reports a failure once, as all jobs share the hook.
async fn run_jobs(
    cmd: &[OsString],
    timeout: Option<Duration>,
    on_failure: &FailureHandler,
    jobs: Vec<PendingJob>,
) {
    let mut batches = Vec::with_capacity(jobs.len());
    let mut waiting = Vec::new();
    for job in jobs {
        batches.push((job.mailbox, job.batch));
        waiting.extend(job.done);
    }
    if let Err(failure) = run_batches(cmd, batches, timeout).await {
        on_failure(&failure);
    }
    for done in waiting {
        done.send(());
    }
}

impl HookThread {
    /// Starts the thread, that runs `cmd` with the changes queued by its runners. A hook running
    /// longer than `timeout` gets killed and fails. `on_failure` handles failures.
    ///
    /// # Errors
    /// If the thread or its runtime cannot be created
    pub fn start(
        cmd: Vec<OsString>,
        timeout: Option<Duration>,
        on_failure: FailureHandler,
    ) -> io::Result<Self> {
        let pending = Pending::default();
        let (wake_tx, wake_rx) = mpsc::unbounded_async::<()>();
        let thread_pending = Arc::clone(&pending);
        let thread_failure = on_failure.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("hooks".to_string())
            .spawn(move || match compio::runtime::Runtime::new() {
                Ok(rt) => {
                    let _ = started_tx.send(Ok(()));
                    rt.block_on(async {
                        while wake_rx.recv().await.is_ok() {
                            // batches queued while the previous hook ran share the next run
                            let jobs = mem::take(&mut *lock(&thread_pending));
                            run_jobs(&cmd, timeout, &thread_failure, jobs).await;
                        }
                    });
                }
                Err(e) => {
                    let _ = started_tx.send(Err(e));
                }
            })?;
        started_rx
            .recv()
            .unwrap_or_else(|_| Err(io::Error::other("hook thread stopped")))?;

        Ok(Self {
            runner: Some(HookRunner {
                pending,
                wake: wake_tx,
                on_failure,
            }),
            thread: Some(thread),
        })
    }

    /// Returns a runner, that queues hooks to this thread
    ///
    /// # Panics
    /// Never, as the thread owns its runner until dropped
    #[must_use]
    pub fn runner(&self) -> HookRunner {
        self.runner
            .clone()
            .expect("the runner exists until the thread got dropped")
    }
}

impl Drop for HookThread {
    fn drop(&mut self) {
        // the thread finishes once the last runner got dropped
        self.runner.take();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::error!("hook thread panicked");
        }
    }
}
