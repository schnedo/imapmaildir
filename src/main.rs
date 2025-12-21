use core::str;

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
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging::init();

    let config = Config::load_from_file();

    if args.nuke {
        nuke(&config);
        Ok(())
    } else {
        let host: &str = config.host();
        let port = config.port();
        let username = config.user();
        let password = &config.password();
        let mailbox = config
            .mailboxes()
            .first()
            .expect("there should be a mailbox configured");
        let state_dir = config.statedir();
        let account = config.account();
        let mail_dir = config.maildir();

        let client = Client::login(host, port, username, password).await;

        let sync_handle = Syncer::sync(account, mailbox, mail_dir, state_dir, client).await;
        sync_handle.await?;

        Ok(())
    }
}
