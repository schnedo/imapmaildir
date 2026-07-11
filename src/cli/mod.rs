mod nuke;
mod sync;

use std::io::stdin;

use clap::{Parser, Subcommand};
use imapmaildir::{config::Account, logging};
use log::LevelFilter;

use crate::{
    cli::nuke::nuke,
    cli::sync::{sync_all, sync_mailbox},
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

pub fn run(cli: &Cli) {
    logging::init(cli.log_level);
    let config = Account::load_from_file(&cli.account);
    log::debug!("parsed config: {config:?}");

    match &cli.command {
        Command::Nuke(nuke_args) => {
            if !nuke_args.yes {
                println!("Nuke account {} [Y]es/[N]o?", cli.account);
                let mut input = String::new();
                stdin()
                    .read_line(&mut input)
                    .expect("reading user input should succeed");
                input.make_ascii_lowercase();
                if input != "y" || input != "yes" {
                    return;
                }
            }
            nuke(&config);
        }
        Command::Sync(sync_args) => {
            if let Some(mailbox) = &sync_args.mailbox {
                sync_mailbox(&config, mailbox, sync_args.idle);
            } else {
                sync_all(config, sync_args.idle);
            }
        }
    }
}
