use std::{io, process::Stdio, string::FromUtf8Error};

use compio::process::Command;

use derive_getters::Getters;
use serde::Deserialize;
use thiserror::Error;

#[derive(Deserialize, Getters, Debug)]
#[serde(deny_unknown_fields)]
pub struct PlainAuth {
    user: String,
    #[getter(skip)]
    password_cmd: Vec<String>,
}

impl PlainAuth {
    #[must_use]
    pub fn new(user: String, password_cmd: Vec<String>) -> Self {
        Self { user, password_cmd }
    }

    /// # Errors
    /// If `password_cmd` is empty, cannot be executed or does not output a utf8 password
    pub async fn password(&self) -> Result<String, Error> {
        let mut cmd_parts = self.password_cmd.iter();
        let mut cmd = Command::new(cmd_parts.next().ok_or(Error::NoProgram)?);
        cmd.args(cmd_parts);
        let Ok(_) = cmd.stdin(Stdio::null());
        let Ok(_) = cmd.stdout(Stdio::piped());
        let output = cmd.output().await.map_err(Error::Execution)?;

        if output.stdout.is_empty() {
            return Err(Error::EmptyOutput);
        }

        Ok(String::from_utf8(output.stdout)
            .map_err(Error::InvalidOutput)?
            .trim_end()
            .to_string())
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("password_cmd should specify a program")]
    NoProgram,
    #[error("Cannot execute password_cmd: {0}")]
    Execution(io::Error),
    #[error("Could not retrieve password from password_cmd")]
    EmptyOutput,
    #[error("password_cmd output is no valid utf8: {0}")]
    InvalidOutput(FromUtf8Error),
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum Auth {
    Plain(PlainAuth),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[compio::test]
    async fn test_password_returns_trimmed_output_of_password_cmd() {
        let auth = PlainAuth::new(
            "user".to_string(),
            vec!["echo".to_string(), "secret".to_string()],
        );

        assert_eq!(assertables::assert_ok!(auth.password().await), "secret");
    }
}
