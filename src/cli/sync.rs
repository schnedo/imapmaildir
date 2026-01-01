use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

use log::{error, info};

use crate::{config::Config, imap::Client, sync::Syncer};

pub fn sync_mailbox(config: &Config, mailbox: &str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("tokio runtime should be buildable");

    rt.block_on(async {
        let client = Client::login(config.host(), config.port(), config.auth()).await;

        Syncer::sync(
            mailbox,
            config.maildir_base_path(),
            config.state_dir(),
            client,
        )
        .await;
    });
}

pub fn sync_all(config: Config) {
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
                    sync_mailbox(&config, &mailbox_clone);
                    info!("finished syncing {mailbox_clone}");
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
            error!("syncing mailbox {mailbox} failed");
            error_happened = true;
        }
    }
    assert!(!error_happened, "error happened during syncing mailboxes");
}
