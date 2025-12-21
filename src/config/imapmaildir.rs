use ::std::env;
use std::{
    collections::HashMap,
    fs::{create_dir_all, read_to_string},
    path::PathBuf,
    str::FromStr,
};

use derive_getters::Getters;
use serde::Deserialize;

use crate::config::account::AccountConfig;

#[derive(Deserialize, Getters)]
pub struct Config {
    #[serde(default = "statedir")]
    statedir: PathBuf,
    #[serde(default = "maildir")]
    maildir: PathBuf,
    accounts: HashMap<String, AccountConfig>,
}

impl Config {
    pub fn load_from_file(file: Option<PathBuf>) -> Self {
        let config_file = file.unwrap_or_else(default_location);
        let config_contents = read_to_string(config_file).expect("config file should be readable");
        toml::from_str(&config_contents).expect("config should be parseable")
    }
}

fn default_location() -> PathBuf {
    let mut config_dir = if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        PathBuf::from_str(&config_home).expect("XDG_CONFIG_HOME should be a parseable path")
    } else {
        let mut config_home = PathBuf::from_str(&env::var("HOME").expect("HOME should be set"))
            .expect("HOME should be a parseable path");
        config_home.push(".config");
        config_home
    };
    config_dir.push(env!("CARGO_PKG_NAME"));
    if !config_dir.exists() {
        create_dir_all(&config_dir).expect("config_dir should be creatable");
    }
    config_dir.push("config.toml");

    config_dir
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
