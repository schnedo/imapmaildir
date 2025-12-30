use clap::Parser;
use log::LevelFilter;
mod cli;
mod config;
mod imap;
mod logging;
mod maildir;
mod repository;
mod sync;

use crate::config::Config;

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
    #[arg(long, default_value_t=LevelFilter::Trace)]
    log_level: LevelFilter,
}

fn main() {
    let args = Args::parse();
    logging::init(args.log_level, args.mailbox.as_deref());
    let config = Config::load_from_file(&args.account);

    cli::run(&args, &config);
}
