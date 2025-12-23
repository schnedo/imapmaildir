use core::str;
use std::path::PathBuf;

use clap::Parser;
mod config;
mod imap;
mod logging;
mod maildir;
mod nuke;
mod repository;
mod sync;

use crate::config::{AccountConfig, Config};
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
    config: Option<PathBuf>,
    #[arg(long)]
    account: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging::init();

    let config = Config::load_from_file(args.config);
    let account_name = args.account;

    if args.nuke {
        nuke(&config, &account_name);
        Ok(())
    } else {
        let state_dir = config.statedir();
        let mail_dir = config.maildir();
        let account_config =
            AccountConfig::load_from_file(config.accountsdir().join(&account_name).as_path());

        let host: &str = account_config.host();
        let port = account_config.port();

        let mailbox = account_config
            .mailboxes()
            .first()
            .expect("there should be a mailbox configured");

        let client = Client::login(host, port, account_config.auth()).await;

        let sync_handle = Syncer::sync(&account_name, mailbox, mail_dir, state_dir, client).await;
        sync_handle.await?;

        Ok(())
    }
}
