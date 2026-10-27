mod nuke;
mod remote;
mod sync;

use std::io::{self, stdin};

use clap::{Parser, Subcommand};
use std::path::PathBuf;

use imapmaildir::{
    ClientError, Notmuch, NotmuchError, SyncError, TagSync, Tls, Transient, TransportError,
    config::{Account, AccountError},
    logging,
};
use log::LevelFilter;
use thiserror::Error;

use crate::{
    cli::nuke::nuke,
    cli::sync::{exit_on_panic, handle_signals, sync_all, sync_mailbox},
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
    /// Inspect the remote server
    Remote(RemoteArgs),
    Sync(SyncArgs),
}

#[derive(clap::Args)]
pub struct RemoteArgs {
    #[command(subcommand)]
    command: RemoteCommand,
}

#[derive(Subcommand)]
pub enum RemoteCommand {
    /// List all selectable mailboxes on the server
    List,
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
        Command::Remote(remote_args) => match remote_args.command {
            RemoteCommand::List => remote::list(&config),
        },
        Command::Sync(sync_args) => {
            if sync_args.idle {
                exit_on_panic();
            }
            let tags = tag_sync(&config)?;
            let shutdown = handle_signals()?;
            if let Some(mailbox) = &sync_args.mailbox {
                let tls = Tls::new(config.connection()).map_err(Error::Tls)?;
                sync_mailbox(
                    &config,
                    &tls,
                    mailbox,
                    sync_args.idle,
                    tags.as_ref(),
                    &shutdown,
                )?;
                if shutdown.has_failed() {
                    return Err(Error::HookFailed);
                }
                Ok(())
            } else {
                sync_all(config, sync_args.idle, tags.as_ref(), &shutdown)
            }
        }
    }
}

/// Opens the notmuch database, if tags should be synced
fn tag_sync(config: &Account) -> Result<Option<TagSync>, Error> {
    let Some(notmuch) = config.notmuch() else {
        return Ok(None);
    };
    let handle = Notmuch::start(notmuch.location().clone()).map_err(Error::Notmuch)?;
    let maildir = config.maildir_base_path();
    // notmuch only indexes mails below its mail root
    if !maildir.is_absolute() || !maildir.starts_with(handle.mail_root()) {
        return Err(Error::OutsideMailRoot {
            maildir: maildir.clone(),
            mail_root: handle.mail_root().to_path_buf(),
        });
    }

    Ok(Some(TagSync {
        notmuch: handle,
        config: notmuch.tags().clone(),
    }))
}

impl Transient for Error {
    fn is_transient(&self) -> bool {
        match self {
            Self::Login(e) | Self::List(e) => e.is_transient(),
            Self::Sync { cause, .. } => cause.is_transient(),
            Self::Config(_)
            | Self::Notmuch(_)
            | Self::OutsideMailRoot { .. }
            | Self::Tls(_)
            | Self::Input(_)
            | Self::Nuke(_)
            | Self::Runtime(_)
            | Self::Signals(_)
            | Self::HookFailed
            | Self::Spawn { .. }
            | Self::SyncAll(_) => false,
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Config(AccountError),
    #[error("Cannot sync tags: {0}")]
    Notmuch(NotmuchError),
    #[error(
        "Cannot sync tags of maildir {} outside notmuch's mail root {}",
        maildir.display(),
        mail_root.display()
    )]
    OutsideMailRoot {
        maildir: PathBuf,
        mail_root: PathBuf,
    },
    #[error("Cannot set up TLS: {0}")]
    Tls(TransportError),
    #[error("Cannot read user input: {0}")]
    Input(io::Error),
    #[error("Cannot nuke account: {0}")]
    Nuke(io::Error),
    #[error("Cannot build async runtime: {0}")]
    Runtime(io::Error),
    #[error("Cannot listen for signals: {0}")]
    Signals(io::Error),
    #[error("Cannot log in: {0}")]
    Login(ClientError),
    #[error("Cannot list remote mailboxes: {0}")]
    List(ClientError),
    #[error("Syncing mailbox {mailbox} failed: {cause}")]
    Sync { mailbox: String, cause: SyncError },
    #[error("Cannot spawn sync thread for mailbox {mailbox}: {cause}")]
    Spawn { mailbox: String, cause: io::Error },
    #[error("on_local_change hook failed")]
    HookFailed,
    #[error("Syncing mailboxes {0:?} failed")]
    SyncAll(Vec<String>),
}
