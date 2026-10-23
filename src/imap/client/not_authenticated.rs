use crate::{
    config,
    imap::{
        client::{
            AuthenticatedClient, Error,
            auth::{Auth, Server},
            capability::{AuthCapabilities, Capabilities, Capability},
        },
        transport::{Connection, ResponseData, ResponseHandler, Tls},
    },
};

pub struct Client {
    connection: Connection,
    capabilities: Capabilities,
    auth_capabilities: AuthCapabilities,
}

impl Client {
    /// # Errors
    /// If connecting to or authenticating with the server fails or the server lacks required
    /// capabilities
    pub async fn login(
        connection_config: &config::Connection,
        tls: &Tls,
        auth_config: &Auth,
    ) -> Result<AuthenticatedClient, Error> {
        let connected = Self::connect(connection_config, tls).await?;
        connected.authenticate(connection_config, auth_config).await
    }

    async fn connect(connection_config: &config::Connection, tls: &Tls) -> Result<Self, Error> {
        let mut connection = Connection::start(connection_config, tls).await?;

        let mut capabilities = Capabilities::default();
        let mut auth_capabilities = AuthCapabilities::default();

        let greeting = connection.next_response().await?;
        match greeting.parsed() {
            imap_proto::Response::Data {
                status: imap_proto::Status::Ok,
                outcome: imap_proto::Outcome { code, information },
            } => {
                if let Some(information) = information {
                    log::trace!("greeting information: {information}");
                }
                if let Some(imap_proto::ResponseCode::Capabilities(caps)) = code {
                    update_capabilities(&mut capabilities, &mut auth_capabilities, caps);
                } else {
                    request_capabilities(
                        &mut connection,
                        &mut capabilities,
                        &mut auth_capabilities,
                    )
                    .await?;
                }
            }
            imap_proto::Response::Data {
                status: imap_proto::Status::Bad,
                outcome: imap_proto::Outcome { information, .. },
            } => {
                return Err(Error::ConnectionRejected(
                    information.as_ref().map(ToString::to_string),
                ));
            }
            imap_proto::Response::Data {
                status: imap_proto::Status::PreAuth,
                outcome: _,
            } => {
                return Err(Error::PreAuthUnsupported);
            }
            _ => return Err(Error::InvalidGreeting),
        }

        Ok(Self {
            connection,
            capabilities,
            auth_capabilities,
        })
    }

    async fn authenticate(
        mut self,
        connection_config: &config::Connection,
        auth_config: &Auth,
    ) -> Result<AuthenticatedClient, Error> {
        let server = Server {
            host: connection_config.host(),
            port: connection_config.port(),
            literal_mode: self.capabilities.literal_mode(),
            sasl_ir: self.capabilities.contains(Capability::SaslIr),
            auth_capabilities: &self.auth_capabilities,
        };
        let mut new_capabilities = Capabilities::default();
        let mut new_auth_capabilities = AuthCapabilities::default();
        let response = auth_config
            .authenticate(
                &mut self.connection,
                server,
                &mut CapabilityHandler {
                    capabilities: &mut new_capabilities,
                    auth_capabilities: &mut new_auth_capabilities,
                },
            )
            .await?;
        self.capabilities = new_capabilities;
        self.auth_capabilities = new_auth_capabilities;
        if let Some(imap_proto::ResponseCode::Capabilities(caps)) =
            response.unsafe_get_tagged_response_code()
        {
            update_capabilities(&mut self.capabilities, &mut self.auth_capabilities, caps);
        } else {
            request_capabilities(
                &mut self.connection,
                &mut self.capabilities,
                &mut self.auth_capabilities,
            )
            .await?;
        }

        AuthenticatedClient::new(self.connection, self.capabilities)
    }
}

async fn request_capabilities(
    connection: &mut Connection,
    capabilities: &mut Capabilities,
    auth_capabilities: &mut AuthCapabilities,
) -> Result<(), Error> {
    let command = "CAPABILITY";
    log::debug!("{command}");
    connection
        .send(
            command.into(),
            &mut CapabilityHandler {
                capabilities,
                auth_capabilities,
            },
        )
        .await?;

    Ok(())
}

struct CapabilityHandler<'a> {
    capabilities: &'a mut Capabilities,
    auth_capabilities: &'a mut AuthCapabilities,
}

impl ResponseHandler for CapabilityHandler<'_> {
    type Error = Error;

    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>> {
        std::future::ready(Ok(
            if let imap_proto::Response::Capabilities(caps) = response.parsed() {
                update_capabilities(self.capabilities, self.auth_capabilities, caps);
                None
            } else {
                Some(response)
            },
        ))
    }
}

fn update_capabilities(
    capabilities: &mut Capabilities,
    auth_capabilities: &mut AuthCapabilities,
    caps: &[imap_proto::Capability],
) {
    // capability lists are complete and replace previous ones, e.g. after login (RFC 3501 7.2.1)
    *capabilities = Capabilities::default();
    *auth_capabilities = AuthCapabilities::default();
    for cap in caps {
        match cap {
            imap_proto::Capability::Auth(_) => auth_capabilities.insert(cap),
            imap_proto::Capability::Imap4rev1 | imap_proto::Capability::Atom(_) => {
                capabilities.insert(cap);
            }
        }
    }
    log::trace!("updated capabilities to {capabilities:?}");
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use crate::imap::client::encoding::LiteralMode;

    #[test]
    fn test_update_capabilities_replaces_previous_ones() {
        let mut capabilities = Capabilities::default();
        let mut auth_capabilities = AuthCapabilities::default();
        update_capabilities(
            &mut capabilities,
            &mut auth_capabilities,
            &[
                imap_proto::Capability::Atom(Cow::Borrowed("LITERAL+")),
                imap_proto::Capability::Atom(Cow::Borrowed("APPENDLIMIT=100")),
            ],
        );

        update_capabilities(
            &mut capabilities,
            &mut auth_capabilities,
            &[imap_proto::Capability::Atom(Cow::Borrowed("LITERAL-"))],
        );

        assert_eq!(capabilities.literal_mode(), LiteralMode::Minus);
        assert_eq!(capabilities.append_limit(), None);
    }
}
