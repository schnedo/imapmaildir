use std::fs::remove_dir_all;

use log::{info, trace};

use crate::config::Config;

pub fn nuke(config: &Config) {
    info!("Nuking mails and state");
    let mails = config.maildir().join(config.account());
    if mails.try_exists().expect("cannot read mail directory") {
        trace!("removing {:}", mails.display());
        remove_dir_all(&mails).expect("removing mails of account should succeed");
    }
    let state = config.statedir().join(config.account());
    if state.try_exists().expect("cannot read state directory") {
        trace!("removing {:}", state.display());
        remove_dir_all(&state).expect("removing state of account should succeed");
    }
}
