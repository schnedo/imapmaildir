use std::{ffi::OsString, fmt, io, thread, time::Duration};

use crossfire::{
    MAsyncTx, mpsc,
    oneshot::{RxOneshot, TxOneshot, oneshot},
};

use crate::sync::change_hook::{ChangeBatch, FailureHandler, HookError, run_batch};

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

/// Queues hooks of all mailboxes, which run one at a time in the order they got queued. Clones
/// queue to the same [`HookThread`].
#[derive(Clone)]
pub struct HookRunner {
    jobs: MAsyncTx<mpsc::Array<HookJob>>,
}

impl fmt::Debug for HookRunner {
    // the queued jobs are no `Debug`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookRunner").finish_non_exhaustive()
    }
}

impl HookRunner {
    /// Returns after the hook handled `batch`. Reports a stopped runner as failure.
    pub(super) async fn run(
        &self,
        cmd: &[OsString],
        mailbox: &str,
        batch: ChangeBatch,
        timeout: Option<Duration>,
        on_failure: &FailureHandler,
    ) {
        if batch.is_empty() {
            return;
        }
        let (done_tx, done_rx): (_, RxOneshot<()>) = oneshot();
        let job = HookJob {
            cmd: cmd.to_vec(),
            mailbox: mailbox.to_string(),
            batch,
            timeout,
            on_failure: on_failure.clone(),
            done: done_tx,
        };
        if self.jobs.send(job).await.is_err() || done_rx.await.is_err() {
            on_failure(HookError::RunnerStopped);
        }
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

impl HookThread {
    /// Starts the thread, that runs the hooks
    ///
    /// # Errors
    /// If the thread or its runtime cannot be created
    pub fn start() -> io::Result<Self> {
        let (jobs_tx, jobs_rx) = mpsc::bounded_async::<HookJob>(1);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("hooks".to_string())
            .spawn(move || match compio::runtime::Runtime::new() {
                Ok(rt) => {
                    let _ = started_tx.send(Ok(()));
                    rt.block_on(async {
                        while let Ok(job) = jobs_rx.recv().await {
                            run_batch(
                                &job.cmd,
                                &job.mailbox,
                                job.batch,
                                job.timeout,
                                &job.on_failure,
                            )
                            .await;
                            job.done.send(());
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
