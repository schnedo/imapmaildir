mod account;
mod imapmaildir;

use std::env;
use std::fs::create_dir_all;
use std::path::PathBuf;
use std::str::FromStr;

pub use account::AccountConfig;
pub use account::AuthConfig;
pub use imapmaildir::Config;

fn default_config_dir() -> PathBuf {
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

    config_dir
}
