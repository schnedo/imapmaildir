use std::{
    collections::HashMap,
    ffi::OsString,
    io,
    mem::MaybeUninit,
    panic,
    pin::pin,
    process, ptr,
    sync::{Arc, mpsc},
    task::Poll,
    thread,
};

use compio::signal::unix::signal;
use futures::future::{self, Either};
use imapmaildir::{
    ChangeRecorder, Client, Detection, HookRunner, HookThread, IdleTimeout, MoveLink, MoveReporter,
    Shutdown, SyncError, Syncer, TagSync, Tls, batched_hook, config::Account, failure_handler,
    planner, reconnecting,
};
use rustix::process::Signal;

use crate::cli::Error;

/// Exits the process if any thread panics, instead of running on without it
pub fn exit_on_panic() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        default_hook(info);
        process::exit(18);
    }));
}

/// Signals, that shut down syncing. A closed terminal (SIGHUP) must not cut off pending work
/// either.
const SHUTDOWN_SIGNALS: [(Signal, &str); 3] = [
    (Signal::TERM, "SIGTERM"),
    (Signal::INT, "SIGINT"),
    (Signal::HUP, "SIGHUP"),
];

/// Triggers the returned [`Shutdown`] on any of [`SHUTDOWN_SIGNALS`]. Afterwards they terminate
/// the process right away again, in case shutting down hangs.
pub fn handle_signals() -> Result<Shutdown, Error> {
    let shutdown = Shutdown::new();
    let triggering = shutdown.clone();
    let (registered_tx, registered_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("signals".to_string())
        .spawn(move || match compio::runtime::Runtime::new() {
            Ok(rt) => rt.block_on(wait_for_signal(&triggering, &registered_tx)),
            Err(e) => {
                let _ = registered_tx.send(Err(e));
            }
        })
        .map_err(Error::Signals)?;
    registered_rx
        .recv()
        .unwrap_or_else(|_| Err(io::Error::other("signal thread stopped")))
        .map_err(Error::Signals)?;

    Ok(shutdown)
}

/// Reports to `registered`, once listening for signals, before waiting for the first of them
async fn wait_for_signal(shutdown: &Shutdown, registered: &mpsc::SyncSender<io::Result<()>>) {
    let handled: Vec<_> = SHUTDOWN_SIGNALS
        .iter()
        .filter(|(signal_number, _)| !is_ignored(*signal_number))
        .collect();
    if handled.is_empty() {
        let _ = registered.send(Ok(()));
        return;
    }
    let mut listeners: Vec<_> = handled
        .iter()
        .map(|(signal_number, _)| Box::pin(signal(signal_number.as_raw())))
        .collect();
    // the listeners get registered when polled first
    let mut received = None;
    for (index, listener) in listeners.iter_mut().enumerate() {
        match futures::poll!(listener.as_mut()) {
            Poll::Ready(Err(e)) => {
                let _ = registered.send(Err(e));
                return;
            }
            Poll::Ready(Ok(())) => {
                received.get_or_insert(index);
            }
            Poll::Pending => {}
        }
    }
    let _ = registered.send(Ok(()));
    let received = match received {
        Some(received) => received,
        None => future::select_all(listeners).await.1,
    };
    log::info!("received {}. Shutting down...", handled[received].1);
    shutdown.trigger();
}

/// Whether `signal_number` got ignored, e.g. SIGHUP by nohup or SIGINT for background jobs of
/// scripts, which must stay in effect
fn is_ignored(signal_number: Signal) -> bool {
    let mut action = MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: passing no new action only queries the current one
    let queried =
        unsafe { libc::sigaction(signal_number.as_raw(), ptr::null(), action.as_mut_ptr()) };
    // SAFETY: zeroed is a valid sigaction, which got filled on success
    queried == 0 && unsafe { action.assume_init() }.sa_sigaction == libc::SIG_IGN
}

/// Starts the thread running the `on_local_change` hooks of all mailboxes one at a time, unless no
/// hook is configured
fn start_hook_thread(config: &Account) -> Result<Option<HookThread>, Error> {
    if config.hooks().on_local_change().is_empty() {
        return Ok(None);
    }

    HookThread::start().map(Some).map_err(Error::HookRunner)
}

/// How a mailbox session finds out about mails moved to or from other mailboxes
pub enum MoveSource {
    /// Syncing continuously: the watchers report renames, which the mailboxes agree on
    Watched(MoveLink),
    /// Syncing once: the sessions report their local changes to a planner, which pairs them
    Reported(MoveReporter),
}

/// What the sessions of all mailboxes of an account have in common
struct Session<'a> {
    config: &'a Account,
    tls: &'a Tls,
    tags: Option<&'a TagSync>,
    /// Runs the hooks of all mailboxes one at a time, if there are any
    runner: Option<&'a HookRunner>,
    shutdown: &'a Shutdown,
}

impl Session<'_> {
    /// Sets up the configured hook for a sync session of `mailbox`, if any. Returns the task of
    /// the `on_local_change` hook, which finishes after the recorder got dropped.
    fn change_recorder(
        &self,
        mailbox: &str,
    ) -> Option<(ChangeRecorder, compio::runtime::JoinHandle<()>)> {
        let hooks = self.config.hooks();
        if hooks.on_local_change().is_empty() {
            return None;
        }
        let runner = self.runner?;
        let cmd: Vec<OsString> = hooks.on_local_change().iter().map(OsString::from).collect();
        let (sink, task) = batched_hook(
            &cmd,
            mailbox,
            hooks.max_pending(),
            hooks.on_local_change_timeout(),
            failure_handler(
                "on_local_change",
                hooks.on_local_change_failure(),
                self.shutdown.clone(),
            ),
            runner.clone(),
        );

        Some((ChangeRecorder::new(vec![sink]), task))
    }

    /// Syncs `mailbox` until done or shutdown got triggered. `moves` connects it to the other
    /// mailboxes synced together. It matches `idle`, as watching and reporting suit one mode each.
    fn sync_mailbox(
        &self,
        mailbox: &str,
        idle: bool,
        moves: Option<MoveSource>,
    ) -> Result<(), Error> {
        let (link, reporter) = match moves {
            Some(MoveSource::Watched(link)) => (Some(link), None),
            Some(MoveSource::Reported(reporter)) => (None, Some(reporter)),
            None => (None, None),
        };
        if idle {
            let idle_timeout = IdleTimeout::new(self.config.idle_timout());
            reconnecting(mailbox, self.shutdown, || {
                self.sync_continuously(mailbox, &idle_timeout, link.as_ref())
            })
        } else {
            self.sync_once(mailbox, reporter)
        }
    }

    /// Runs a single session on a new runtime, which cancels all its tasks when dropped. Returns
    /// after shutting down.
    fn sync_continuously(
        &self,
        mailbox: &str,
        idle_timeout: &IdleTimeout,
        moves: Option<&MoveLink>,
    ) -> Result<(), Error> {
        let config = self.config;
        let shutdown = self.shutdown;
        let rt = compio::runtime::Runtime::new().map_err(Error::Runtime)?;

        rt.block_on(async {
            let login = Client::login(config.connection(), self.tls, config.auth());
            let Some(client) = unless_shut_down(login, shutdown).await else {
                return Ok(());
            };
            let client = client.map_err(Error::Login)?;
            let (changes, hook_task) = self.change_recorder(mailbox).unzip();
            let result = Syncer::sync_continuously(
                mailbox,
                config.maildir_base_path(),
                config.state_dir(),
                client,
                idle_timeout.get(),
                config.sync(),
                changes,
                self.tags,
                moves,
                shutdown,
            )
            .await;
            if let Err(cause) = &result
                && let Some(idled) = cause.idle_terminated()
            {
                idle_timeout.on_terminated(idled);
            }
            // the session's changes must not get lost with its runtime
            if let Some(hook_task) = hook_task
                && let Err(e) = hook_task.await
            {
                log::warn!("on_local_change hook task failed: {e:?}");
            }

            result.map_err(|cause| Error::Sync {
                mailbox: mailbox.to_string(),
                cause,
            })
        })
    }

    fn sync_once(&self, mailbox: &str, reporter: Option<MoveReporter>) -> Result<(), Error> {
        let rt = compio::runtime::Runtime::new().map_err(Error::Runtime)?;

        rt.block_on(async {
            let (changes, hook_task) = self.change_recorder(mailbox).unzip();
            // detecting local changes only needs the local files, so it can overlap with
            // connecting
            let detecting = reporter.map(|reporter| {
                let recorder = changes.clone();
                let mailbox = mailbox.to_string();
                let mail_dir = self.config.maildir_base_path().clone();
                let state_dir = self.config.state_dir().clone();
                compio::runtime::spawn(async move {
                    reporter
                        .detect(&mailbox, &mail_dir, &state_dir, recorder)
                        .await
                })
            });
            let result = self.sync_detected(mailbox, changes, detecting).await;
            // the recorder is gone by now, which lets the hook task finish
            if let Some(hook_task) = hook_task
                && let Err(e) = hook_task.await
            {
                log::warn!("on_local_change hook task failed: {e:?}");
            }

            result
        })
    }

    /// Connects and syncs `mailbox` once with the local changes `detecting` finds. Drops everything
    /// that holds `changes` before returning on every path, as hooks wait for that.
    async fn sync_detected(
        &self,
        mailbox: &str,
        changes: Option<ChangeRecorder>,
        detecting: Option<compio::runtime::JoinHandle<Result<Option<Detection>, SyncError>>>,
    ) -> Result<(), Error> {
        let config = self.config;
        let shutdown = self.shutdown;
        let login = Client::login(config.connection(), self.tls, config.auth());
        let client = unless_shut_down(login, shutdown).await;
        // always finished, so its recorder is dropped even if connecting failed
        let detection = match detecting {
            Some(detecting) => await_detection(detecting, mailbox).await,
            None => Ok(None),
        };
        let Some(client) = client else {
            return Ok(());
        };
        let client = client.map_err(Error::Login)?;
        let result = Syncer::sync_once(
            mailbox,
            config.maildir_base_path(),
            config.state_dir(),
            client,
            config.sync(),
            changes,
            self.tags,
            detection?,
            shutdown,
        )
        .await;

        match result {
            // the next sync retries, as when shutting down during idle
            Err(cause) if shutdown.is_triggered() && cause.is_transient() => {
                log::warn!("{mailbox}: {cause}. Stopped while shutting down");
                Ok(())
            }
            result => result.map_err(|cause| Error::Sync {
                mailbox: mailbox.to_string(),
                cause,
            }),
        }
    }
}

/// Syncs a single `mailbox` until done or `shutdown` got triggered. Runs its hooks on a thread of
/// its own.
pub fn sync_one(
    config: &Account,
    tls: &Tls,
    mailbox: &str,
    idle: bool,
    tags: Option<&TagSync>,
    shutdown: &Shutdown,
) -> Result<(), Error> {
    let hook_thread = start_hook_thread(config)?;
    let runner = hook_thread.as_ref().map(HookThread::runner);

    Session {
        config,
        tls,
        tags,
        runner: runner.as_ref(),
        shutdown,
    }
    .sync_mailbox(mailbox, idle, None)
}

/// Runs `future`, unless `shutdown` gets triggered before it completes. Then returns `None`.
async fn unless_shut_down<T>(future: impl Future<Output = T>, shutdown: &Shutdown) -> Option<T> {
    match future::select(pin!(future), pin!(shutdown.wait())).await {
        Either::Left((output, _)) => Some(output),
        Either::Right(_) => None,
    }
}

/// Waits for the detection of the local changes of `mailbox`. `None` if there is none to use,
/// e.g. as the detection panicked, which makes the session detect them itself.
async fn await_detection(
    detecting: compio::runtime::JoinHandle<Result<Option<Detection>, SyncError>>,
    mailbox: &str,
) -> Result<Option<Detection>, Error> {
    let Ok(detection) = detecting.await else {
        log::warn!("{mailbox}: detecting local changes panicked");
        return Ok(None);
    };

    detection.map_err(|cause| Error::Sync {
        mailbox: mailbox.to_string(),
        cause,
    })
}

/// Syncs all configured mailboxes of the account, each on its own thread, until done or shut down
pub fn sync_all(
    config: Account,
    idle: bool,
    tags: Option<&TagSync>,
    shutdown: &Shutdown,
) -> Result<(), Error> {
    let config = Arc::new(config);
    let tls = Tls::new(config.connection()).map_err(Error::Tls)?;
    let hook_thread = start_hook_thread(&config)?;
    let mut sync_handles = Vec::new();
    let mut spawn_error = None;
    // exactly one of both is in use: the watchers of the sessions of continuous syncs report
    // moves to links, while the sessions of a sync once report their local changes to the planner
    let mut links = if idle {
        MoveLink::connect(config.mailboxes())
    } else {
        HashMap::new()
    };
    let (reporter, planner) = if idle {
        (None, None)
    } else {
        let (reporter, planner) = planner();
        (Some(reporter), Some(planner))
    };
    for mailbox in config.mailboxes() {
        let moves = if idle {
            links.remove(mailbox).map(MoveSource::Watched)
        } else {
            reporter.clone().map(MoveSource::Reported)
        };
        let config = config.clone();
        let tls = tls.clone();
        let mailbox_clone = mailbox.clone();
        let thread_shutdown = shutdown.clone();
        let tags = tags.cloned();
        let runner = hook_thread.as_ref().map(HookThread::runner);
        let spawned = thread::Builder::new().name(mailbox.clone()).spawn(move || {
            let session = Session {
                config: &config,
                tls: &tls,
                tags: tags.as_ref(),
                runner: runner.as_ref(),
                shutdown: &thread_shutdown,
            };
            if let Err(e) = session.sync_mailbox(&mailbox_clone, idle, moves) {
                // other mailboxes may take long to finish
                log::error!("{e}");
                if idle {
                    // other mailboxes never finish otherwise, which would hide this one's
                    // failure
                    thread_shutdown.fail();
                }
                return Err(e);
            }
            if thread_shutdown.is_triggered() {
                log::info!("stopped syncing {mailbox_clone}");
            } else {
                log::info!("finished syncing {mailbox_clone}");
            }

            Ok(())
        });
        match spawned {
            Ok(handle) => sync_handles.push((mailbox.clone(), handle)),
            Err(cause) => {
                let e = Error::Spawn {
                    mailbox: mailbox.clone(),
                    cause,
                };
                // mailboxes already syncing must finish their pending work before exiting
                shutdown.fail();
                spawn_error = Some(e);
                break;
            }
        }
    }

    // the planner finishes, once all sessions reported their local changes or gave up
    drop(reporter);
    if let Some(planner) = planner {
        planner.plan();
    }

    let mut failed = Vec::new();
    for (mailbox, handle) in sync_handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(_)) => failed.push(mailbox),
            Err(_) => {
                log::error!("syncing mailbox {mailbox} panicked");
                failed.push(mailbox);
            }
        }
    }

    // waits for the queued hooks, as the sessions dropped their runners
    drop(hook_thread);

    if let Some(e) = spawn_error {
        Err(e)
    } else if !failed.is_empty() {
        Err(Error::SyncAll(failed))
    } else if shutdown.has_failed() {
        Err(Error::HookFailed)
    } else {
        Ok(())
    }
}
