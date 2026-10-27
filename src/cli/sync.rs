use std::{
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
    ChangeRecorder, Client, IdleTimeout, Shutdown, Syncer, TagSync, Tls, batched_hook,
    config::Account, failure_handler, reconnecting,
};
use rustix::process::Signal;

use crate::cli::Error;

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

/// Sets up the configured hook for a sync session of `mailbox`, if any. Returns the task of the
/// `on_local_change` hook, which finishes after the recorder got dropped.
fn change_recorder(
    config: &Account,
    mailbox: &str,
    shutdown: &Shutdown,
) -> Option<(ChangeRecorder, compio::runtime::JoinHandle<()>)> {
    let hooks = config.hooks();
    if hooks.on_local_change().is_empty() {
        return None;
    }
    let cmd: Vec<OsString> = hooks.on_local_change().iter().map(OsString::from).collect();
    let (sink, task) = batched_hook(
        &cmd,
        mailbox,
        hooks.batching(),
        failure_handler(
            "on_local_change",
            hooks.on_local_change_failure(),
            shutdown.clone(),
        ),
    );

    Some((ChangeRecorder::new(vec![sink]), task))
}

/// Syncs `mailbox` until done or `shutdown` got triggered
pub fn sync_mailbox(
    config: &Account,
    tls: &Tls,
    mailbox: &str,
    idle: bool,
    tags: Option<&TagSync>,
    shutdown: &Shutdown,
) -> Result<(), Error> {
    if idle {
        let idle_timeout = IdleTimeout::new(config.idle_timout());
        reconnecting(mailbox, shutdown, || {
            sync_continuously(config, tls, mailbox, &idle_timeout, tags, shutdown)
        })
    } else {
        sync_once(config, tls, mailbox, tags, shutdown)
    }
}

/// Runs `future`, unless `shutdown` gets triggered before it completes. Then returns `None`.
async fn unless_shut_down<T>(future: impl Future<Output = T>, shutdown: &Shutdown) -> Option<T> {
    match future::select(pin!(future), pin!(shutdown.wait())).await {
        Either::Left((output, _)) => Some(output),
        Either::Right(_) => None,
    }
}

/// Runs a single session on a new runtime, which cancels all its tasks when dropped. Returns
/// after shutting down.
fn sync_continuously(
    config: &Account,
    tls: &Tls,
    mailbox: &str,
    idle_timeout: &IdleTimeout,
    tags: Option<&TagSync>,
    shutdown: &Shutdown,
) -> Result<(), Error> {
    let rt = compio::runtime::Runtime::new().map_err(Error::Runtime)?;

    rt.block_on(async {
        let login = Client::login(config.connection(), tls, config.auth());
        let Some(client) = unless_shut_down(login, shutdown).await else {
            return Ok(());
        };
        let client = client.map_err(Error::Login)?;
        let (changes, hook_task) = change_recorder(config, mailbox, shutdown).unzip();
        let result = Syncer::sync_continuously(
            mailbox,
            config.maildir_base_path(),
            config.state_dir(),
            client,
            idle_timeout.get(),
            config.sync(),
            changes,
            tags,
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

fn sync_once(
    config: &Account,
    tls: &Tls,
    mailbox: &str,
    tags: Option<&TagSync>,
    shutdown: &Shutdown,
) -> Result<(), Error> {
    let rt = compio::runtime::Runtime::new().map_err(Error::Runtime)?;

    rt.block_on(async {
        let login = Client::login(config.connection(), tls, config.auth());
        let Some(client) = unless_shut_down(login, shutdown).await else {
            return Ok(());
        };
        let client = client.map_err(Error::Login)?;
        let (changes, hook_task) = change_recorder(config, mailbox, shutdown).unzip();
        let result = Syncer::sync_once(
            mailbox,
            config.maildir_base_path(),
            config.state_dir(),
            client,
            config.sync(),
            changes,
            tags,
            shutdown,
        )
        .await;
        if let Some(hook_task) = hook_task
            && let Err(e) = hook_task.await
        {
            log::warn!("on_local_change hook task failed: {e:?}");
        }

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
    })
}

pub fn sync_all(
    config: Account,
    idle: bool,
    tags: Option<&TagSync>,
    shutdown: &Shutdown,
) -> Result<(), Error> {
    let config = Arc::new(config);
    let tls = Tls::new(config.connection()).map_err(Error::Tls)?;
    let mut sync_handles = Vec::new();
    let mut spawn_error = None;
    for mailbox in config.mailboxes() {
        let config = config.clone();
        let tls = tls.clone();
        let mailbox_clone = mailbox.clone();
        let thread_shutdown = shutdown.clone();
        let tags = tags.cloned();
        let spawned = thread::Builder::new().name(mailbox.clone()).spawn(move || {
            if let Err(e) = sync_mailbox(
                &config,
                &tls,
                &mailbox_clone,
                idle,
                tags.as_ref(),
                &thread_shutdown,
            ) {
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
