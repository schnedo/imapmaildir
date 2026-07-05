use ::std::env;
use std::{ffi::OsString, fs::read_to_string, path::PathBuf, str::FromStr, time::Duration};

use derive_getters::Getters;
use serde::Deserialize;

use crate::config::auth::Auth;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountConfigFile {
    auth: Auth,
    host: String,
    port: u16,
    #[serde(default, with = "humantime_serde")]
    idle_timeout: Option<Duration>,
    server_certificate_file: Option<PathBuf>,
    // todo: "all" for generic fetch of all mailboxes
    mailboxes: Vec<String>,
    maildir_base_path: Option<PathBuf>,
    #[serde(default)]
    on_local_change: Vec<OsString>,
}

// todo: move config to code using it
#[derive(Getters, Debug)]
pub struct Connection {
    host: String,
    port: u16,
    server_certificate_file: Option<PathBuf>,
}

impl Connection {
    #[must_use]
    pub fn new(host: String, port: u16, server_certificate_file: Option<PathBuf>) -> Self {
        Self {
            host,
            port,
            server_certificate_file,
        }
    }
}

// todo: why 2 structs? custom logic should be configurable in serde/derive_getters
#[derive(Getters, Debug)]
pub struct Account {
    auth: Auth,
    connection: Connection,
    mailboxes: Vec<String>,
    maildir_base_path: PathBuf,
    state_dir: PathBuf,
    #[getter(copy)]
    idle_timout: Duration,
    on_local_change: Vec<OsString>,
}

impl Account {
    #[expect(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        auth: Auth,
        host: String,
        port: u16,
        server_certificate_file: Option<PathBuf>,
        mailboxes: Vec<String>,
        maildir_base_path: PathBuf,
        state_dir: PathBuf,
        idle_timout: Duration,
        on_local_change: Vec<OsString>,
    ) -> Self {
        Self {
            auth,
            connection: Connection::new(host, port, server_certificate_file),
            mailboxes,
            maildir_base_path,
            state_dir,
            idle_timout,
            on_local_change,
        }
    }

    #[expect(clippy::missing_panics_doc)] // todo: use in IDLE
    #[must_use]
    pub fn load_from_file(account: &str) -> Self {
        let mut config_home = config_home();
        config_home.push("accounts");
        let mut config_file_name = account.to_string();
        config_file_name.push_str(".toml");
        config_home.push(&config_file_name);
        let contents = read_to_string(config_home).expect("account config should be readable");
        let config: AccountConfigFile =
            toml::from_str(&contents).expect("account config should be parsable");

        let maildir_base_path = config.maildir_base_path.unwrap_or_else(|| {
            let mut data_home = data_home();
            data_home.push(account);

            data_home
        });

        let mut state_dir = data_home();
        state_dir.push(account);

        Self {
            auth: config.auth,
            connection: Connection::new(config.host, config.port, config.server_certificate_file),
            mailboxes: config.mailboxes,
            maildir_base_path,
            state_dir,
            idle_timout: config.idle_timeout.unwrap_or(Duration::from_mins(29)),
            on_local_change: config.on_local_change,
        }
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
