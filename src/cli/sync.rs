use std::{
    process,
    sync::Arc,
    thread::{self, JoinHandle},
};

use imapmaildir::{Client, Syncer, config::Account, on_local_change};

pub fn sync_mailbox(config: &Account, mailbox: &str, idle: bool) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("tokio runtime should be buildable");

    rt.block_on(async {
        let client = Client::login(config.connection(), config.auth()).await;

        let on_change = on_local_change(config.on_local_change());
        if idle {
            Syncer::sync_continuously(
                mailbox,
                config.maildir_base_path(),
                config.state_dir(),
                client,
                config.idle_timout(),
                on_change,
            )
            .await;
        } else {
            Syncer::sync_once(
                mailbox,
                config.maildir_base_path(),
                config.state_dir(),
                client,
                on_change,
            )
            .await;
        }
    });
}

pub fn sync_all(config: Account, idle: bool) {
    let config = Arc::new(config);
    let sync_handles: Vec<JoinHandle<()>> = config
        .mailboxes()
        .iter()
        .map(|mailbox| {
            let config = config.clone();
            // todo: Cow instead of cloning String multiple times
            let mailbox_clone = mailbox.clone();
            let thread_builder = thread::Builder::new().name(mailbox.clone());
            thread_builder
                .spawn(move || {
                    sync_mailbox(&config, &mailbox_clone, idle);
                    log::info!("finished syncing {mailbox_clone}");
                })
                .expect("spawning sync thread should succeed")
        })
        .collect();

    let mut error_happened = false;
    for handle in sync_handles {
        let mailbox = handle
            .thread()
            .name()
            .expect("thread should have mailbox as name")
            .to_string();
        let sync_result = handle.join();
        if sync_result.is_err() {
            log::error!("syncing mailbox {mailbox} failed");
            error_happened = true;
        }
    }
    if error_happened {
        log::warn!("error happened during syncing mailboxes");
        process::exit(18);
    }
}
