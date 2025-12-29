use std::fs::remove_dir_all;

use log::{info, trace};

use crate::config::Config;

pub fn nuke(config: &Config) {
    info!("Nuking mails and state");
    let data = config.data_dir();
    if data.try_exists().expect("cannot read data directory") {
        trace!("removing {:}", data.display());
        remove_dir_all(&data).expect("removing mails of account should succeed");
    }
}
