mod nuke;
mod sync;

use imapmaildir::config::Account;

use crate::{
    Args,
    cli::nuke::nuke,
    cli::sync::{sync_all, sync_mailbox},
};

pub fn run(args: &Args, config: Account) {
    if args.nuke {
        nuke(&config);
    } else if let Some(mailbox) = &args.mailbox {
        sync_mailbox(&config, mailbox);
    } else {
        sync_all(config);
    }
}
