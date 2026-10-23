use std::{io, process::Stdio, string::FromUtf8Error};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use compio::process::Command;
use derive_getters::Getters;
use serde::Deserialize;
use thiserror::Error;

use crate::imap::{
    client::{
        Error as ClientError,
        capability::{AuthCapabilities, AuthCapability},
        encoding::{CommandBuilder, LiteralMode},
    },
    transport::{self, Connection, Continued, ResponseData, ResponseHandler},
};

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
        run_secret_cmd(&self.password_cmd, "password_cmd").await
    }
}

#[derive(Deserialize, Getters, Debug)]
#[serde(deny_unknown_fields)]
pub struct OAuth2Auth {
    user: String,
    /// Prints a valid access token, e.g. by refreshing it via an OAuth 2.0 helper
    #[getter(skip)]
    token_cmd: Vec<String>,
}

impl OAuth2Auth {
    #[must_use]
    pub fn new(user: String, token_cmd: Vec<String>) -> Self {
        Self { user, token_cmd }
    }

    /// # Errors
    /// If `token_cmd` is empty, cannot be executed or does not output a utf8 token
    pub async fn token(&self) -> Result<String, Error> {
        run_secret_cmd(&self.token_cmd, "token_cmd").await
    }
}

async fn run_secret_cmd(cmd: &[String], field: &'static str) -> Result<String, Error> {
    let mut cmd_parts = cmd.iter();
    let mut cmd = Command::new(cmd_parts.next().ok_or(Error::NoProgram(field))?);
    cmd.args(cmd_parts);
    let Ok(_) = cmd.stdin(Stdio::null());
    let Ok(_) = cmd.stdout(Stdio::piped());
    let output = cmd.output().await.map_err(|e| Error::Execution(field, e))?;

    if output.stdout.is_empty() {
        return Err(Error::EmptyOutput(field));
    }

    Ok(String::from_utf8(output.stdout)
        .map_err(|e| Error::InvalidOutput(field, e))?
        .trim_end()
        .to_string())
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0} should specify a program")]
    NoProgram(&'static str),
    #[error("Cannot execute {0}: {1}")]
    Execution(&'static str, io::Error),
    #[error("Could not retrieve secret from {0}")]
    EmptyOutput(&'static str),
    #[error("{0} output is no valid utf8: {1}")]
    InvalidOutput(&'static str, FromUtf8Error),
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum Auth {
    Plain(PlainAuth),
    OAuth2(OAuth2Auth),
}

/// What the server offers for logging in
pub struct Server<'a> {
    pub host: &'a str,
    pub port: u16,
    pub literal_mode: LiteralMode,
    pub sasl_ir: bool,
    pub auth_capabilities: &'a AuthCapabilities,
}

impl Auth {
    /// Logs in and returns the tagged completion
    pub(super) async fn authenticate<H: ResponseHandler<Error = ClientError>>(
        &self,
        connection: &mut Connection,
        server: Server<'_>,
        handler: &mut H,
    ) -> Result<ResponseData, ClientError> {
        let response = match self {
            Auth::Plain(plain) => {
                if !server.auth_capabilities.contains(AuthCapability::Plain) {
                    return Err(ClientError::MissingAuthCapability(AuthCapability::Plain));
                }
                let password = plain.password().await?;
                log::debug!("LOGIN <user> <password>");
                let mut command = CommandBuilder::new(server.literal_mode, "LOGIN ");
                command.push_astring(plain.user());
                command.push_str(" ");
                command.push_astring(&password);
                connection.send_streamed(&command.parts(), handler).await
            }
            Auth::OAuth2(oauth2) => {
                let (mechanism, response) = if server
                    .auth_capabilities
                    .contains(AuthCapability::OAuthBearer)
                {
                    let token = oauth2.token().await?;
                    let response = oauthbearer(oauth2.user(), server.host, server.port, &token);
                    ("OAUTHBEARER", response)
                } else if server.auth_capabilities.contains(AuthCapability::XOAuth2) {
                    let token = oauth2.token().await?;
                    ("XOAUTH2", xoauth2(oauth2.user(), &token))
                } else {
                    return Err(ClientError::MissingOAuth2Mechanism);
                };
                log::debug!("AUTHENTICATE {mechanism} <token>");
                sasl(
                    connection,
                    mechanism,
                    &STANDARD.encode(response),
                    server.sasl_ir,
                    handler,
                )
                .await
            }
        };

        response.map_err(|e| match e {
            ClientError::Transport(transport::Error::No(information)) => {
                ClientError::LoginRejected(information)
            }
            e => e,
        })
    }
}

/// Runs a single round SASL exchange (RFC 3501 6.2.2) sending the base64 encoded `response`.
/// Answers further challenges, which report failures (e.g. RFC 7628 3.2.2), with an empty
/// response, so that the server completes the command.
async fn sasl<H: ResponseHandler>(
    connection: &mut Connection,
    mechanism: &str,
    response: &str,
    initial_response: bool,
    handler: &mut H,
) -> Result<ResponseData, H::Error> {
    let mut command = format!("AUTHENTICATE {mechanism}");
    if initial_response {
        command.push(' ');
        command.push_str(response);
    }
    let tag = connection.write_command(command.into_bytes()).await?;
    let mut responded = initial_response;
    loop {
        match connection.receive_continued(&tag, handler).await? {
            Continued::Request(challenge) if responded => {
                let challenge = challenge.unwrap_or_default();
                let decoded = STANDARD
                    .decode(challenge.trim())
                    .map_or(challenge, |decoded| {
                        String::from_utf8_lossy(&decoded).into_owned()
                    });
                log::warn!("{mechanism} authentication failed: {decoded}");
                connection.write_continuation(Vec::new()).await?;
            }
            Continued::Request(_) => {
                connection
                    .write_continuation(response.as_bytes().to_vec())
                    .await?;
                responded = true;
            }
            Continued::Done(completion) => return Ok(completion),
        }
    }
}

/// Initial client response of XOAUTH2 (<https://developers.google.com/workspace/gmail/imap/xoauth2-protocol>)
fn xoauth2(user: &str, token: &str) -> String {
    format!("user={user}\x01auth=Bearer {token}\x01\x01")
}

/// Initial client response of OAUTHBEARER (RFC 7628 3.1)
fn oauthbearer(user: &str, host: &str, port: u16, token: &str) -> String {
    let user = user.replace('=', "=3D").replace(',', "=2C");
    format!("n,a={user},\x01host={host}\x01port={port}\x01auth=Bearer {token}\x01\x01")
}

#[cfg(test)]
mod tests {
    use assertables::*;

    use super::*;

    #[compio::test]
    async fn test_password_returns_trimmed_output_of_password_cmd() {
        let auth = PlainAuth::new(
            "user".to_string(),
            vec!["echo".to_string(), "secret".to_string()],
        );

        assert_eq!(assert_ok!(auth.password().await), "secret");
    }

    #[compio::test]
    async fn test_token_returns_trimmed_output_of_token_cmd() {
        let auth = OAuth2Auth::new(
            "user".to_string(),
            vec!["echo".to_string(), "token".to_string()],
        );

        assert_eq!(assert_ok!(auth.token().await), "token");
    }

    #[compio::test]
    async fn test_empty_token_cmd_names_field() {
        let auth = OAuth2Auth::new("user".to_string(), Vec::new());

        let error = assert_err!(auth.token().await);
        assert_eq!(error.to_string(), "token_cmd should specify a program");
    }

    #[test]
    fn test_xoauth2_response() {
        assert_eq!(
            xoauth2("me@example.com", "tok"),
            "user=me@example.com\x01auth=Bearer tok\x01\x01"
        );
    }

    #[test]
    fn test_oauthbearer_response_escapes_user() {
        assert_eq!(
            oauthbearer("a=b,c@example.com", "imap.example.com", 993, "tok"),
            "n,a=a=3Db=2Cc@example.com,\x01host=imap.example.com\x01port=993\x01auth=Bearer tok\x01\x01"
        );
    }

    #[test]
    fn test_auth_config_parses_oauth2() {
        let auth: Auth = assert_ok!(toml::from_str(
            "type = \"OAuth2\"\nuser = \"me\"\ntoken_cmd = [\"echo\", \"tok\"]\n"
        ));

        let Auth::OAuth2(oauth2) = auth else {
            panic!("auth should be OAuth2");
        };
        assert_eq!(oauth2.user(), "me");
    }
}
