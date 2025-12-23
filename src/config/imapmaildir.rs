use ::std::env;
use std::{fs::read_to_string, path::PathBuf, str::FromStr};

use derive_getters::Getters;
use serde::Deserialize;

use crate::config::default_config_dir;

// todo: is this even necessary?
#[derive(Deserialize, Getters)]
pub struct Config {
    #[serde(default = "statedir")]
    statedir: PathBuf,
    #[serde(default = "accountsdir")]
    accountsdir: PathBuf,
    #[serde(default = "maildir")]
    maildir: PathBuf,
}

impl Config {
    pub fn load_from_file(config_file: Option<PathBuf>) -> Self {
        let mut config_file = config_file.unwrap_or_else(default_config_file);
        config_file.push("config.toml");

        let config_contents = read_to_string(config_file).expect("config file should be readable");
        toml::from_str(&config_contents).expect("config should be parseable")
    }
}

fn default_config_file() -> PathBuf {
    let mut config_file = default_config_dir();
    config_file.push("config.toml");

    config_file
}

fn home() -> PathBuf {
    PathBuf::from_str(&env::var("HOME").expect("HOME should be set"))
        .expect("HOME should be a parseable path")
}

// todo: default to state and colocate state?
fn maildir() -> PathBuf {
    let mut maildir = home();
    maildir.push(".mail");
    maildir
}

fn accountsdir() -> PathBuf {
    let mut config_dir = default_config_dir();
    config_dir.push("accounts");

    config_dir
}

fn statedir() -> PathBuf {
    let mut state_home = if let Ok(state_home) = env::var("XDG_STATE_HOME") {
        PathBuf::from_str(&state_home).expect("XDG_STATE_HOME should be a parseable path")
    } else {
        let mut state_home = home();
        state_home.push(".local/state");
        state_home
    };
    state_home.push(env!("CARGO_PKG_NAME"));
    state_home
}
