use ::std::env;
use std::{
    fs::{create_dir, read_to_string},
    path::PathBuf,
    str::FromStr,
};

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    username: String,
    password_cmd: String,
}

impl Config {
    pub fn load_from_file() -> Self {
        let mut config_dir = if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
            PathBuf::from_str(&config_home).expect("XDG_CONFIG_HOME should be a parseable path")
        } else {
            let mut config_home = PathBuf::from_str(&env::var("HOME").expect("HOME should be set"))
                .expect("XDG_CONFIG_HOME should be a parseable path");
            config_home.push(".config");
            config_home
        };
        config_dir.push(env!("CARGO_PKG_NAME"));
        if !config_dir.exists() {
            create_dir(&config_dir).expect("config_dir should be creatable");
        }
        config_dir.push("config.toml");

        let config_contents = read_to_string(config_dir).expect("config file should be readable");
        toml::from_str(&config_contents).expect("config should be parseable")
    }
}
