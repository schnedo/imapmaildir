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
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging::init();

    let config = Config::load_from_file(&args.account);

    if args.nuke {
        nuke(&config);

        Ok(())
    } else {
        let mailbox = config
            .mailboxes()
            .first()
            .expect("there should be a mailbox configured");

        let client = Client::login(config.host(), config.port(), config.auth()).await;

        let sync_handle = Syncer::sync(
            mailbox,
            config.maildir_base_path(),
            config.state_dir(),
            client,
        )
        .await;
        sync_handle.await?;

        Ok(())
    }
}
