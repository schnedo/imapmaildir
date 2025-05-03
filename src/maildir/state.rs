use std::{
    fs::{create_dir_all, read_to_string, write},
    io,
    path::{Path, PathBuf},
};

use derive_getters::Getters;
use serde::{Deserialize, Serialize};

#[derive(Getters, Debug, Deserialize, Serialize)]
pub struct State {
    #[serde(skip)]
    #[getter(skip)]
    path: PathBuf,
    uid_validity: u32,
}

impl State {
    pub fn load(state_dir: &Path, maildir: &str) -> io::Result<Self> {
        let mut state_dir = state_dir.join(maildir);
        create_dir_all(&state_dir).expect("creation of state_dir should succeed");

        state_dir.push("state");
        let state_content = read_to_string(&state_dir)?;
        let mut state: State =
            toml::from_str(&state_content).expect("state file should be parseable");
        state.path = state_dir;

        Ok(state)
    }

    pub fn write(&self) -> io::Result<()> {
        let serialized = toml::to_string(self).expect("state should be serializable");
        write(&self.path, serialized)
    }
}
