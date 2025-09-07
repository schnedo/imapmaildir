#![expect(dead_code, unused_variables, unused_imports)]
mod config;
mod imap;
mod logging;
mod maildir;
mod nuke;
mod state;
mod sync;

use anyhow::Result;
use clap::Parser;
use config::Config;
use imap::{Authenticator, Connection, ImapRepository};
use log::info;
use maildir::MaildirRepository;
use nuke::nuke;
use state::State;
use sync::Repository;
use sync::Syncer;

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
        let mailbox = config
            .mailboxes()
            .first()
            .expect("there should be one mailbox set");
        let account = config.account();
        let state_dir = config.statedir();

        let state = State::load(state_dir, account, mailbox);
        let state_existed = state.is_ok();
        let state = state.unwrap_or_else(|_| State::init(state_dir, account, mailbox));

        let mut syncer = if state_existed {
            let maildir_repository =
                MaildirRepository::load(config.account(), mailbox, config.maildir(), &state);
            let imap_repository = ImapRepository::connect::<Connection>(
                config.host(),
                config.port(),
                config.user(),
                &config.password(),
                mailbox,
                &state,
            )
            .await
            .expect("connecting imap repository should succeed");
            Syncer::new(imap_repository, maildir_repository)
        } else {
            info!("initializing {account} {mailbox}");
            let imap_repository = ImapRepository::init::<Connection>(
                config.host(),
                config.port(),
                config.user(),
                &config.password(),
                mailbox,
                &state,
            )
            .await
            .expect("connecting imap repository should not fail");
            let maildir_repository =
                MaildirRepository::init(config.account(), mailbox, config.maildir(), &state);
            Syncer::new(imap_repository, maildir_repository)
        };

        syncer.init_remote_to_local().await;

        Ok(())
    }
}
