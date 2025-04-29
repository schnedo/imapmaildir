use ::std::env;
use std::{
    fs::{create_dir, read_to_string},
    path::PathBuf,
    process::Command,
    str::FromStr,
};

use derive_getters::Getters;
use serde::Deserialize;

#[derive(Deserialize, Getters)]
pub struct Config {
    user: String,
    #[getter(skip)]
    password_cmd: String,
    host: String,
    port: u16,
    maildir: PathBuf,
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

    pub fn password(&self) -> String {
        let mut cmd_parts = self.password_cmd.split(' ');
        let mut cmd = Command::new(
            cmd_parts
                .next()
                .expect("password_cmd should specify a program"),
        );
        for part in cmd_parts {
            cmd.arg(part);
        }
        let output = cmd.output().expect("password_cmd should be executable");

        String::from_utf8(output.stdout)
            .expect("password_cmd should evaluate to password")
            .trim_end()
            .to_string()
    }
}
