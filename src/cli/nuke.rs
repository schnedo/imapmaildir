use std::fs::remove_dir_all;

use imapmaildir::config::Account;

pub fn nuke(config: &Account) {
    let mails = config.maildir_base_path();
    if mails.try_exists().expect("cannot read mail directory") {
        log::trace!("removing {:}", mails.display());
        remove_dir_all(mails).expect("removing mails of account should succeed");
    }
    let state = config.state_dir();
    if state.try_exists().expect("cannot read state directory") {
        log::trace!("removing {:}", state.display());
        remove_dir_all(state).expect("removing state of account should succeed");
    }
}
