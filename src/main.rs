use clap::Parser;
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
}

fn main() {
    logging::init();

    let args = Args::parse();
    let config = Config::load_from_file(&args.account);

    cli::run(&args, &config);
}
