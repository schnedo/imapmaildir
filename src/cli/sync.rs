use std::{
    panic, process,
    sync::Arc,
    thread::{self, JoinHandle},
};

use imapmaildir::{Client, Syncer, Tls, config::Account, on_local_change};

use crate::cli::Error;

pub fn exit_on_panic() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        default_hook(info);
        process::exit(18);
    }));
}

pub fn sync_mailbox(config: &Account, tls: &Tls, mailbox: &str, idle: bool) -> Result<(), Error> {
    let rt = compio::runtime::Runtime::new().map_err(Error::Runtime)?;

    rt.block_on(async {
        let client = Client::login(config.connection(), tls, config.auth())
            .await
            .map_err(Error::Login)?;
        let sync_error = |cause| Error::Sync {
            mailbox: mailbox.to_string(),
            cause,
        };

        let (on_change, hook_task) = on_local_change(config.on_local_change());
        if idle {
            match Syncer::sync_continuously(
                mailbox,
                config.maildir_base_path(),
                config.state_dir(),
                client,
                config.idle_timout(),
                config.sync(),
                on_change,
            )
            .await
            {
                Ok(never) => match never {},
                Err(e) => Err(sync_error(e)),
            }
        } else {
            let result = Syncer::sync_once(
                mailbox,
                config.maildir_base_path(),
                config.state_dir(),
                client,
                config.sync(),
                on_change,
            )
            .await;
            if let Err(e) = hook_task.await {
                log::warn!("on_change hook task failed: {e:?}");
            }

            result.map_err(sync_error)
        }
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
            // todo: Cow instead of cloning String multiple times
            let mailbox_clone = mailbox.clone();
            let thread_builder = thread::Builder::new().name(mailbox.clone());
            let handle = thread_builder
                .spawn(move || {
                    sync_mailbox(&config, &tls, &mailbox_clone, idle)?;
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
