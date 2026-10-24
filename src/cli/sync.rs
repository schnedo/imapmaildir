use std::{
    convert::Infallible,
    ffi::OsString,
    panic, process,
    sync::Arc,
    thread::{self, JoinHandle},
};

use imapmaildir::{
    ChangeRecorder, Client, IdleTimeout, Syncer, Tls, batched_hook, config::Account,
    failure_handler, reconnecting,
};

use crate::cli::Error;

pub fn exit_on_panic() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        default_hook(info);
        process::exit(18);
    }));
}

/// Sets up the configured hook for a sync session of `mailbox`, if any. Returns the task of the
/// `on_local_change` hook, which finishes after the recorder got dropped.
fn change_recorder(
    config: &Account,
    mailbox: &str,
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
        failure_handler("on_local_change", hooks.on_local_change_failure()),
    );

    Some((ChangeRecorder::new(vec![sink]), task))
}

pub fn sync_mailbox(config: &Account, tls: &Tls, mailbox: &str, idle: bool) -> Result<(), Error> {
    if idle {
        let idle_timeout = IdleTimeout::new(config.idle_timout());
        Err(reconnecting(mailbox, || {
            sync_continuously(config, tls, mailbox, &idle_timeout)
        }))
    } else {
        sync_once(config, tls, mailbox)
    }
}

/// Runs a single session on a new runtime, which cancels all its tasks when dropped
fn sync_continuously(
    config: &Account,
    tls: &Tls,
    mailbox: &str,
    idle_timeout: &IdleTimeout,
) -> Result<Infallible, Error> {
    let rt = compio::runtime::Runtime::new().map_err(Error::Runtime)?;

    rt.block_on(async {
        let client = Client::login(config.connection(), tls, config.auth())
            .await
            .map_err(Error::Login)?;
        let (changes, hook_task) = change_recorder(config, mailbox).unzip();
        let Err(cause) = Syncer::sync_continuously(
            mailbox,
            config.maildir_base_path(),
            config.state_dir(),
            client,
            idle_timeout.get(),
            config.sync(),
            changes,
        )
        .await;
        if let Some(idled) = cause.idle_terminated() {
            idle_timeout.on_terminated(idled);
        }
        // the session's changes must not get lost with its runtime
        if let Some(hook_task) = hook_task
            && let Err(e) = hook_task.await
        {
            log::warn!("on_local_change hook task failed: {e:?}");
        }

        Err(Error::Sync {
            mailbox: mailbox.to_string(),
            cause,
        })
    })
}

fn sync_once(config: &Account, tls: &Tls, mailbox: &str) -> Result<(), Error> {
    let rt = compio::runtime::Runtime::new().map_err(Error::Runtime)?;

    rt.block_on(async {
        let client = Client::login(config.connection(), tls, config.auth())
            .await
            .map_err(Error::Login)?;
        let (changes, hook_task) = change_recorder(config, mailbox).unzip();
        let result = Syncer::sync_once(
            mailbox,
            config.maildir_base_path(),
            config.state_dir(),
            client,
            config.sync(),
            changes,
        )
        .await;
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

pub fn sync_all(config: Account, idle: bool) -> Result<(), Error> {
    let config = Arc::new(config);
    let tls = Tls::new(config.connection()).map_err(Error::Tls)?;
    let sync_handles = config
        .mailboxes()
        .iter()
        .map(|mailbox| {
            let config = config.clone();
            let tls = tls.clone();
            let mailbox_clone = mailbox.clone();
            let thread_builder = thread::Builder::new().name(mailbox.clone());
            let handle = thread_builder
                .spawn(move || {
                    if let Err(e) = sync_mailbox(&config, &tls, &mailbox_clone, idle) {
                        if idle {
                            // other mailboxes never finish, which would hide this one's failure
                            log::error!("{e}");
                            process::exit(18);
                        }
                        return Err(e);
                    }
                    log::info!("finished syncing {mailbox_clone}");

                    Ok(())
                })
                .map_err(|cause| Error::Spawn {
                    mailbox: mailbox.clone(),
                    cause,
                })?;

            Ok((mailbox.clone(), handle))
        })
        .collect::<Result<Vec<(String, JoinHandle<Result<(), Error>>)>, Error>>()?;

    let mut failed = Vec::new();
    for (mailbox, handle) in sync_handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::error!("{e}");
                failed.push(mailbox);
            }
            Err(_) => {
                log::error!("syncing mailbox {mailbox} panicked");
                failed.push(mailbox);
            }
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::SyncAll(failed))
    }
}
