use crate::{
    config::{self, Auth},
    imap::{
        client::{
            AuthenticatedClient,
            capability::{AuthCapabilities, AuthCapability, Capabilities},
            encoding,
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
    pub async fn login(
        connection_config: &config::Connection,
        tls: &Tls,
        auth_config: &Auth,
    ) -> AuthenticatedClient {
        let connected = Self::connect(connection_config, tls).await;
        connected.authenticate(auth_config).await
    }

    async fn connect(connection_config: &config::Connection, tls: &Tls) -> Self {
        let mut connection = Connection::start(connection_config, tls)
            .await
            .expect("creating a connection should succeed");

        let mut capabilities = Capabilities::default();
        let mut auth_capabilities = AuthCapabilities::default();

        let greeting = connection.next_response().await;
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
                    .await;
                }
            }
            imap_proto::Response::Data {
                status: imap_proto::Status::Bad,
                outcome: _,
            } => {
                todo!("handle server rejecting connection");
            }
            imap_proto::Response::Data {
                status: imap_proto::Status::PreAuth,
                outcome: _,
            } => {
                todo!("handle pre-authenticated state");
            }
            _ => {
                panic!("greeting should only ever be ok, bad or preauth, as per specification")
            }
        }

        Self {
            connection,
            capabilities,
            auth_capabilities,
        }
    }

    async fn authenticate(mut self, auth_config: &Auth) -> AuthenticatedClient {
        match auth_config {
            Auth::Plain(plain_auth_config) => {
                assert!(
                    self.auth_capabilities.contains(AuthCapability::Plain),
                    "server should support PLAIN auth capability"
                );
                let password = plain_auth_config.password().await;
                log::debug!("LOGIN <user> <password>");
                let response = self
                    .connection
                    .send(
                        format!(
                            "LOGIN {} {}",
                            encoding::quote(plain_auth_config.user()),
                            encoding::quote(&password)
                        )
                        .into(),
                        &mut CapabilityHandler {
                            capabilities: &mut self.capabilities,
                            auth_capabilities: &mut self.auth_capabilities,
                        },
                    )
                    .await
                    .expect("login should succeed");
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
                    .await;
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
) {
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
        .await
        .expect("capability request should succeed");
}

struct CapabilityHandler<'a> {
    capabilities: &'a mut Capabilities,
    auth_capabilities: &'a mut AuthCapabilities,
}

impl ResponseHandler for CapabilityHandler<'_> {
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>> {
        std::future::ready(
            if let imap_proto::Response::Capabilities(caps) = response.parsed() {
                update_capabilities(self.capabilities, self.auth_capabilities, caps);
                None
            } else {
                Some(response)
            },
        )
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
