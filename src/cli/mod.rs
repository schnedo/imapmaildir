mod nuke;
mod sync;

pub use nuke::nuke;

use crate::{
    Args,
    cli::sync::{sync_all, sync_mailbox},
    config::Config,
};

pub fn run(args: &Args, config: &Config) {
    if args.nuke {
        nuke(config);
    } else if let Some(mailbox) = &args.mailbox {
        sync_mailbox(config, mailbox);
    } else {
        sync_all(config, &args.account);
    }
}
