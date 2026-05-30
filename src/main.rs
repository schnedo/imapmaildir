mod cli;

use clap::Parser;
use log::LevelFilter;

use imapmaildir::{config::Account, logging};

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
    logging::init(args.log_level);
    let config = Account::load_from_file(&args.account);

    cli::run(&args, config);
}
