mod cli;

use std::process;

use clap::Parser;

use crate::cli::Cli;

fn main() {
    let cli = Cli::parse();
    if let Err(e) = cli::run(&cli) {
        log::error!("{e}");
        process::exit(18);
    }
}
