use std::process::Command;

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

    #[must_use]
    #[expect(clippy::missing_panics_doc)] // todo: use in IDLE
    pub fn password(&self) -> String {
        let mut cmd_parts = self.password_cmd.iter();
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

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum Auth {
    Plain(PlainAuth),
}
