use std::{
    panic, process,
    sync::Arc,
    thread::{self, JoinHandle},
};

use imapmaildir::{Client, Syncer, Tls, config::Account, on_local_change};

pub fn exit_on_panic() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        default_hook(info);
        process::exit(18);
    }));
}

pub fn sync_mailbox(config: &Account, tls: &Tls, mailbox: &str, idle: bool) {
    let rt = compio::runtime::Runtime::new().expect("compio runtime should be buildable");

    rt.block_on(async {
        let client = Client::login(config.connection(), tls, config.auth()).await;

        let on_change = on_local_change(config.on_local_change());
        if idle {
            Syncer::sync_continuously(
                mailbox,
                config.maildir_base_path(),
                config.state_dir(),
                client,
                config.idle_timout(),
                config.sync(),
                on_change,
            )
            .await;
        } else {
            Syncer::sync_once(
                mailbox,
                config.maildir_base_path(),
                config.state_dir(),
                client,
                config.sync(),
                on_change,
            )
            .await;
        }
    });
}

pub fn sync_all(config: Account, idle: bool) {
    let config = Arc::new(config);
    let tls = Tls::new(config.connection()).expect("setting up TLS should succeed");
    let sync_handles: Vec<JoinHandle<()>> = config
        .mailboxes()
        .iter()
        .map(|mailbox| {
            let config = config.clone();
            let tls = tls.clone();
            // todo: Cow instead of cloning String multiple times
            let mailbox_clone = mailbox.clone();
            let thread_builder = thread::Builder::new().name(mailbox.clone());
            thread_builder
                .spawn(move || {
                    sync_mailbox(&config, &tls, &mailbox_clone, idle);
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
