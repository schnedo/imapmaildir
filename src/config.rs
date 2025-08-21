use ::std::env;
use std::{
    fs::{create_dir_all, read_to_string},
    path::PathBuf,
    process::Command,
    str::FromStr,
};

use derive_getters::Getters;
use serde::Deserialize;

#[derive(Deserialize, Getters)]
pub struct Config {
    #[serde(default = "statedir")]
    statedir: PathBuf,
    #[serde(default = "maildir")]
    maildir: PathBuf,
    user: String,
    #[getter(skip)]
    password_cmd: String,
    host: String,
    port: u16,
    mailboxes: Vec<String>,
}

impl Config {
    pub fn load_from_file() -> Self {
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

        assert!(
            !output.stdout.is_empty(),
            "could not retrieve password from password_cmd"
        );

        String::from_utf8(output.stdout)
            .expect("password_cmd should evaluate to password")
            .trim_end()
            .to_string()
    }
}

fn home() -> PathBuf {
    PathBuf::from_str(&env::var("HOME").expect("HOME should be set"))
        .expect("HOME should be a parseable path")
}

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
