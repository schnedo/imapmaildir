use std::fs::remove_dir_all;

use log::trace;

use crate::config::Config;

pub fn nuke(config: &Config) {
    let mails = config.maildir_base_path();
    if mails.try_exists().expect("cannot read mail directory") {
        trace!("removing {:}", mails.display());
        remove_dir_all(mails).expect("removing mails of account should succeed");
    }
    let state = config.state_dir();
    if state.try_exists().expect("cannot read state directory") {
        trace!("removing {:}", state.display());
        remove_dir_all(state).expect("removing state of account should succeed");
    }
}
