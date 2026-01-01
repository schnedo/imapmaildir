use std::{
    env,
    process::{Child, Command},
};

use log::error;

use crate::{config::Config, imap::Client, sync::Syncer};

pub fn sync_mailbox(config: &Config, mailbox: &str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("tokio runtime should be buildable");

    rt.block_on(async {
        let client = Client::login(config.host(), config.port(), config.auth()).await;

        let sync_handle = Syncer::sync(
            mailbox,
            config.maildir_base_path(),
            config.state_dir(),
            client,
        )
        .await;

        sync_handle.await
    })
    .expect("syncing should complete without error");
}

pub fn sync_all(config: &Config, account: &str) {
    let program = env::args_os()
        .next()
        .expect("first command line argument should always be program name");
    let children: Vec<(&str, Child)> = config
        .mailboxes()
        .iter()
        .map(|mailbox| {
            let mut subprocess = Command::new(&program);
            subprocess.args(["--account", account, "--mailbox", mailbox]);
            (
                mailbox.as_str(),
                subprocess
                    .spawn()
                    .expect("mailbox specific subprocess should be runnable"),
            )
        })
        .collect();

    let mut error_happened = false;
    for (mailbox, mut child) in children {
        let exit_code = child.wait().expect("child process should be awaitable");
        if !exit_code.success() {
            error!("syncing mailbox {mailbox} failed");
            error_happened = true;
        }
    }
    assert!(!error_happened, "error happened during syncing mailboxes");
}
