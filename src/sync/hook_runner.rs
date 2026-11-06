use std::{ffi::OsString, fmt, io, thread, time::Duration};

use crossfire::{
    MTx, mpsc,
    oneshot::{RxOneshot, TxOneshot, oneshot},
};

use crate::sync::change_hook::{ChangeBatch, FailureHandler, HookError, run_batches};

/// A batch of changes, that waits for its turn to be passed to the hook
struct HookJob {
    cmd: Vec<OsString>,
    mailbox: String,
    batch: ChangeBatch,
    timeout: Option<Duration>,
    on_failure: FailureHandler,
    /// Completes, once the hook handled the batch
    done: TxOneshot<()>,
}

/// Queues hooks of all mailboxes, which run one at a time. Batches waiting for their turn share the
/// next run. Clones queue to the same [`HookThread`].
#[derive(Clone)]
pub struct HookRunner {
    jobs: MTx<mpsc::List<HookJob>>,
}

impl fmt::Debug for HookRunner {
    // the queued jobs are no `Debug`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookRunner").finish_non_exhaustive()
    }
}

impl HookRunner {
    /// Queues `batch` for the hook. The returned receiver completes, once the hook handled it,
    /// and fails if the runner stopped before.
    ///
    /// # Errors
    /// If the runner stopped
    pub(super) fn queue(
        &self,
        cmd: &[OsString],
        mailbox: &str,
        batch: ChangeBatch,
        timeout: Option<Duration>,
        on_failure: &FailureHandler,
    ) -> Result<RxOneshot<()>, HookError> {
        let (done_tx, done_rx): (_, RxOneshot<()>) = oneshot();
        let job = HookJob {
            cmd: cmd.to_vec(),
            mailbox: mailbox.to_string(),
            batch,
            timeout,
            on_failure: on_failure.clone(),
            done: done_tx,
        };
        // a mailbox queues as many jobs as it passes on, which the hook handles one by one
        self.jobs.send(job).map_err(|_| HookError::RunnerStopped)?;

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

/// Runs the hook once for all `jobs`, which share the hook of the first one. Reports a failure to
/// every job.
async fn run_jobs(jobs: Vec<HookJob>) {
    let mut hook = None;
    let mut batches = Vec::with_capacity(jobs.len());
    let mut waiting = Vec::with_capacity(jobs.len());
    for job in jobs {
        hook.get_or_insert((job.cmd, job.timeout));
        batches.push((job.mailbox, job.batch));
        waiting.push((job.on_failure, job.done));
    }
    let Some((cmd, timeout)) = hook else {
        return;
    };
    let failure = run_batches(&cmd, batches, timeout).await.err();
    for (on_failure, done) in waiting {
        if let Some(failure) = &failure {
            on_failure(failure);
        }
        done.send(());
    }
}

impl HookThread {
    /// Starts the thread, that runs the hooks
    ///
    /// # Errors
    /// If the thread or its runtime cannot be created
    pub fn start() -> io::Result<Self> {
        let (jobs_tx, jobs_rx) = mpsc::unbounded_async::<HookJob>();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("hooks".to_string())
            .spawn(move || match compio::runtime::Runtime::new() {
                Ok(rt) => {
                    let _ = started_tx.send(Ok(()));
                    rt.block_on(async {
                        while let Ok(first) = jobs_rx.recv().await {
                            // batches queued while the previous hook ran share the next run
                            let mut jobs = vec![first];
                            while let Ok(job) = jobs_rx.try_recv() {
                                jobs.push(job);
                            }
                            run_jobs(jobs).await;
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
            runner: Some(HookRunner { jobs: jobs_tx }),
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
