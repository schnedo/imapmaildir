use std::{
    fs::{create_dir_all, read_to_string, write},
    io,
    path::{Path, PathBuf},
};

use derive_getters::Getters;
use log::trace;
use serde::{Deserialize, Serialize};

use crate::imap::UidValidity;

#[derive(Getters, Debug, Deserialize, Serialize)]
pub struct State {
    #[serde(skip)]
    #[getter(skip)]
    path: PathBuf,
    uid_validity: UidValidity,
}

impl State {
    pub fn create_new(state_dir: &Path, mailbox: &str, uid_validity: UidValidity) -> Self {
        let this = Self {
            path: Self::prepare_state_file(state_dir, mailbox),
            uid_validity,
        };
        this.write().expect("writing state should succeed");

        this
    }

    pub fn set_uid_validity(&mut self, uid_validity: UidValidity) {
        self.uid_validity = uid_validity;
        self.write().expect("writing state should succeed");
    }

    fn prepare_state_file(state_dir: &Path, mailbox: &str) -> PathBuf {
        let mut state_dir = state_dir.join(mailbox);
        create_dir_all(&state_dir).expect("creation of state_dir should succeed");

        state_dir.push("state");
        state_dir
    }

    pub fn load(state_dir: &Path, mailbox: &str) -> io::Result<Self> {
        let state_file = Self::prepare_state_file(state_dir, mailbox);
        let state_content = read_to_string(&state_file)?;
        let mut state: State =
            toml::from_str(&state_content).expect("state file should be parseable");
        state.path = state_file;

        Ok(state)
    }

    pub fn write(&self) -> io::Result<()> {
        trace!("writing {self:?}");
        let serialized = toml::to_string(self).expect("state should be serializable");
        write(&self.path, serialized)
    }
}

impl Serialize for UidValidity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let num: u32 = (*self).into();
        num.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for UidValidity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let num = u32::deserialize(deserializer)?;
        Ok(num.into())
    }
}
