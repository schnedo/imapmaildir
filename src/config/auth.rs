use std::process::Stdio;

use compio::process::Command;

use derive_getters::Getters;
use serde::Deserialize;

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

    #[expect(clippy::missing_panics_doc)] // todo: use in IDLE
    pub async fn password(&self) -> String {
        let mut cmd_parts = self.password_cmd.iter();
        let mut cmd = Command::new(
            cmd_parts
                .next()
                .expect("password_cmd should specify a program"),
        );
        cmd.args(cmd_parts);
        let Ok(_) = cmd.stdin(Stdio::null());
        let Ok(_) = cmd.stdout(Stdio::piped());
        let output = cmd
            .output()
            .await
            .expect("password_cmd should be executable");

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

        assert_eq!(auth.password().await, "secret");
    }
}
