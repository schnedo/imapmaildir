use ::std::env;
use std::{fs::read_to_string, io, num::NonZeroUsize, path::PathBuf, time::Duration};

use derive_getters::Getters;
use serde::Deserialize;
use thiserror::Error;

use crate::{config::Auth, sync};

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
    on_local_change: Vec<String>,
    store_batch_size: Option<NonZeroUsize>,
    store_batch_max_bytes: Option<NonZeroUsize>,
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
    on_local_change: Vec<String>,
    #[getter(copy)]
    sync: sync::Config,
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
        on_local_change: Vec<String>,
        sync: sync::Config,
    ) -> Self {
        Self {
            auth,
            connection: Connection::new(host, port, server_certificate_file),
            mailboxes,
            maildir_base_path,
            state_dir,
            idle_timout,
            on_local_change,
            sync,
        }
    }

    /// # Errors
    /// If the config or data directory cannot be determined or the account config cannot be
    /// read or parsed
    pub fn load_from_file(account: &str) -> Result<Self, Error> {
        let mut config_home = config_home()?;
        config_home.push("accounts");
        let mut config_file_name = account.to_string();
        config_file_name.push_str(".toml");
        config_home.push(&config_file_name);
        let contents =
            read_to_string(&config_home).map_err(|e| Error::Read(config_home.clone(), e))?;
        let config: AccountConfigFile =
            toml::from_str(&contents).map_err(|e| Error::Parse(config_home, e))?;

        let data_home = data_home()?;
        let maildir_base_path = config
            .maildir_base_path
            .unwrap_or_else(|| data_home.join(account));
        let state_dir = data_home.join(account);

        Ok(Self {
            auth: config.auth,
            connection: Connection::new(config.host, config.port, config.server_certificate_file),
            mailboxes: config.mailboxes,
            maildir_base_path,
            state_dir,
            idle_timout: config.idle_timeout.unwrap_or(Duration::from_mins(29)),
            on_local_change: config.on_local_change,
            sync: {
                let default = sync::Config::default();
                sync::Config::new(
                    config
                        .store_batch_size
                        .map_or(default.store_batch_size(), NonZeroUsize::get),
                    config
                        .store_batch_max_bytes
                        .map_or(default.store_batch_max_bytes(), NonZeroUsize::get),
                )
            },
        })
    }
}

fn config_home() -> Result<PathBuf, Error> {
    let mut config_dir = if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config_home)
    } else {
        let mut config_home = home()?;
        config_home.push(".config/");
        config_home
    };
    config_dir.push(env!("CARGO_PKG_NAME"));

    Ok(config_dir)
}

fn home() -> Result<PathBuf, Error> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(Error::HomeUnset)
}

fn data_home() -> Result<PathBuf, Error> {
    let mut data_home = if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data_home)
    } else {
        let mut data_home = home()?;
        data_home.push(".local/share/");
        data_home
    };
    data_home.push(env!("CARGO_PKG_NAME"));

    Ok(data_home)
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("HOME is not set")]
    HomeUnset,
    #[error("Cannot read account config {0}: {1}")]
    Read(PathBuf, io::Error),
    #[error("Cannot parse account config {0}: {1}")]
    Parse(PathBuf, toml::de::Error),
}
