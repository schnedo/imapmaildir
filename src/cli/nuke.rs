use std::{fs::remove_dir_all, io};

use imapmaildir::config::Account;

pub fn nuke(config: &Account) -> io::Result<()> {
    let mails = config.maildir_base_path();
    if mails.try_exists()? {
        log::trace!("removing {:}", mails.display());
        remove_dir_all(mails)?;
    }
    let state = config.state_dir();
    if state.try_exists()? {
        log::trace!("removing {:}", state.display());
        remove_dir_all(state)?;
    }

    Ok(())
}
