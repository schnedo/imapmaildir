use ::std::env;
use std::{
    fs::{create_dir_all, read_to_string},
    path::PathBuf,
    str::FromStr,
};

use derive_getters::Getters;
use serde::Deserialize;

use crate::config::auth::AuthConfig;

#[derive(Deserialize)]
struct AccountConfigFile {
    auth: AuthConfig,
    host: String,
    port: u16,
    mailboxes: Vec<String>,
}

#[derive(Getters)]
pub struct AccountConfig {
    account: String,
    auth: AuthConfig,
    host: String,
    port: u16,
    mailboxes: Vec<String>,
}

impl AccountConfig {
    pub fn load_from_file(account: String) -> Self {
        let mut config_home = config_home();
        config_home.push("accounts");
        let mut config_file_name = account.clone();
        config_file_name.push_str(".toml");
        config_home.push(&config_file_name);
        let contents = read_to_string(config_home).expect("account config should be readable");
        let config: AccountConfigFile =
            toml::from_str(&contents).expect("account config should be parsable");

        Self {
            account,
            auth: config.auth,
            host: config.host,
            port: config.port,
            mailboxes: config.mailboxes,
        }
    }

    pub fn data_dir(&self) -> PathBuf {
        let mut data_dir = data_home();
        data_dir.push(self.account());
        create_dir_all(&data_dir).expect("data_dir should be creatable");

        data_dir
    }
}

fn config_home() -> PathBuf {
    let mut config_dir = if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        PathBuf::from_str(&config_home).expect("XDG_CONFIG_HOME should be a parseable path")
    } else {
        let mut config_home = home();
        config_home.push(".config/");
        config_home
    };
    config_dir.push(env!("CARGO_PKG_NAME"));

    config_dir
}

fn home() -> PathBuf {
    PathBuf::from_str(&env::var("HOME").expect("HOME should be set"))
        .expect("HOME should be a parseable path")
}

fn data_home() -> PathBuf {
    let mut data_home = if let Ok(data_home) = env::var("XDG_DATA_HOME") {
        PathBuf::from_str(&data_home).expect("XDG_DATA_HOME should be a parseable path")
    } else {
        let mut data_home = home();
        data_home.push(".local/share/");
        data_home
    };
    data_home.push(env!("CARGO_PKG_NAME"));

    data_home
}
