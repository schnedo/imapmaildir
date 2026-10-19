mod nuke;
mod sync;

use std::io::{self, stdin};

use clap::{Parser, Subcommand};
use imapmaildir::{
    ClientError, SyncError, Tls, TransportError,
    config::{Account, AccountError},
    logging,
};
use log::LevelFilter;
use thiserror::Error;

use crate::{
    cli::nuke::nuke,
    cli::sync::{exit_on_panic, sync_all, sync_mailbox},
};

#[derive(Parser)]
#[command(version, about, long_about=None)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
    #[arg(long)]
    account: String,
    #[arg(long, default_value_t=LevelFilter::Trace)]
    log_level: LevelFilter,
}

#[derive(Subcommand)]
pub enum Command {
    /// `rm -rf` the configured account (WARNING: includes all mails)
    Nuke(NukeArgs),
    Sync(SyncArgs),
}

#[derive(clap::Args)]
#[command(version, about, long_about = None)]
pub struct NukeArgs {
    #[arg(long, short, default_value_t = false)]
    yes: bool,
}

#[derive(clap::Args)]
#[command(version, about, long_about = None)]
pub struct SyncArgs {
    #[arg(long)]
    mailbox: Option<String>,
    #[arg(long, default_value_t = false)]
    idle: bool,
}

pub fn run(cli: &Cli) -> Result<(), Error> {
    logging::init(cli.log_level);
    let config = Account::load_from_file(&cli.account).map_err(Error::Config)?;
    log::debug!("parsed config: {config:?}");

    match &cli.command {
        Command::Nuke(nuke_args) => {
            if !nuke_args.yes {
                println!("Nuke account {} [Y]es/[N]o?", cli.account);
                let mut input = String::new();
                stdin().read_line(&mut input).map_err(Error::Input)?;
                let input = input.trim().to_ascii_lowercase();
                if input != "y" && input != "yes" {
                    return Ok(());
                }
            }
            nuke(&config).map_err(Error::Nuke)
        }
        Command::Sync(sync_args) => {
            if sync_args.idle {
                exit_on_panic();
            }
            if let Some(mailbox) = &sync_args.mailbox {
                let tls = Tls::new(config.connection()).map_err(Error::Tls)?;
                sync_mailbox(&config, &tls, mailbox, sync_args.idle)
            } else {
                sync_all(config, sync_args.idle)
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Config(AccountError),
    #[error("Cannot set up TLS: {0}")]
    Tls(TransportError),
    #[error("Cannot read user input: {0}")]
    Input(io::Error),
    #[error("Cannot nuke account: {0}")]
    Nuke(io::Error),
    #[error("Cannot build async runtime: {0}")]
    Runtime(io::Error),
    #[error("Cannot log in: {0}")]
    Login(ClientError),
    #[error("Syncing mailbox {mailbox} failed: {cause}")]
    Sync { mailbox: String, cause: SyncError },
    #[error("Cannot spawn sync thread for mailbox {mailbox}: {cause}")]
    Spawn { mailbox: String, cause: io::Error },
    #[error("Syncing mailboxes {0:?} failed")]
    SyncAll(Vec<String>),
}
