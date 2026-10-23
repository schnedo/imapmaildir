use crate::{
    config::{self, Auth},
    imap::{
        client::{
            AuthenticatedClient, Error,
            capability::{AuthCapabilities, AuthCapability, Capabilities},
            encoding::CommandBuilder,
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
        connected.authenticate(auth_config).await
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

    async fn authenticate(mut self, auth_config: &Auth) -> Result<AuthenticatedClient, Error> {
        match auth_config {
            Auth::Plain(plain_auth_config) => {
                if !self.auth_capabilities.contains(AuthCapability::Plain) {
                    return Err(Error::MissingAuthCapability(AuthCapability::Plain));
                }
                let password = plain_auth_config.password().await?;
                log::debug!("LOGIN <user> <password>");
                let mut command = CommandBuilder::new(self.capabilities.literal_mode(), "LOGIN ");
                command.push_astring(plain_auth_config.user());
                command.push_str(" ");
                command.push_astring(&password);
                let response = self
                    .connection
                    .send_streamed(
                        &command.parts(),
                        &mut CapabilityHandler {
                            capabilities: &mut self.capabilities,
                            auth_capabilities: &mut self.auth_capabilities,
                        },
                    )
                    .await?;
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
