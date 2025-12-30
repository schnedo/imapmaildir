use std::env;
use std::process::{Child, Command};

use clap::Parser;
mod config;
mod imap;
mod logging;
mod maildir;
mod nuke;
mod repository;
mod sync;

use crate::config::Config;
use crate::imap::Client;
use crate::nuke::nuke;
use crate::sync::Syncer;
use anyhow::Result;

#[derive(Parser, Debug)]
#[command(version, about, long_about=None)]
struct Args {
    /// `rm -rf` the configured account (WARNING: includes all mails)
    #[arg(long)]
    nuke: bool,
    #[arg(long)]
    account: String,
    #[arg(long)]
    mailbox: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    logging::init();

    let config = Config::load_from_file(&args.account);

    if args.nuke {
        nuke(&config);

        Ok(())
    } else if let Some(mailbox) = args.mailbox {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()?;

        rt.block_on(async {
            let client = Client::login(config.host(), config.port(), config.auth()).await;

            let sync_handle = Syncer::sync(
                &mailbox,
                config.maildir_base_path(),
                config.state_dir(),
                client,
            )
            .await;

            sync_handle.await
        })?;

        Ok(())
    } else {
        let program = env::args_os()
            .next()
            .expect("first command line argument should always be program name");
        let children: Vec<Child> = config
            .mailboxes()
            .iter()
            .map(|mailbox| {
                let mut subprocess = Command::new(&program);
                subprocess.args(["--account", &args.account, "--mailbox", mailbox]);
                subprocess
                    .spawn()
                    .expect("mailbox specific subprocess should be runnable")
            })
            .collect();

        for mut child in children {
            child.wait()?;
        }

        Ok(())
    }
}
