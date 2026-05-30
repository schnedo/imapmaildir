use assertables::assert_ok;
use imapmaildir::logging;
use rstest::fixture;
use testcontainers::{
    ContainerAsync, GenericImage, Healthcheck, ImageExt,
    core::{AccessMode, ContainerPort, Mount, WaitFor},
    runners::AsyncRunner,
};

const IMAPS_PORT: ContainerPort = ContainerPort::Tcp(31993);

pub struct MockServer {
    server: ContainerAsync<GenericImage>,
    password: String,
}

impl MockServer {
    pub async fn hostname(&self) -> String {
        assert_ok!(self.server.get_host().await).to_string()
    }

    pub async fn port(&self) -> u16 {
        assert_ok!(self.server.get_host_port_ipv4(IMAPS_PORT).await)
    }

    pub fn password(&self) -> &str {
        &self.password
    }
}

#[fixture]
#[once]
fn __setup_logging() {
    logging::init(log::LevelFilter::Trace);
}

#[fixture]
pub async fn server(__setup_logging: ()) -> MockServer {
    let password = "password".to_string();
    MockServer {
        server: assert_ok!(
            GenericImage::new("dovecot/dovecot", "2.4.4-dev")
                .with_exposed_port(IMAPS_PORT)
                .with_wait_for(WaitFor::healthcheck())
                .with_health_check(Healthcheck::cmd([
                    "nc",
                    "-z",
                    "-w",
                    "5",
                    "localhost",
                    &IMAPS_PORT.to_string(),
                ]))
                .with_mount(
                    Mount::bind_mount(
                        format!("{}/mock/certificate.crt", env!("CARGO_MANIFEST_DIR")),
                        "/etc/dovecot/ssl/tls.crt",
                    )
                    .with_access_mode(AccessMode::ReadOnly)
                )
                .with_mount(
                    Mount::bind_mount(
                        format!("{}/mock/private_key.pem", env!("CARGO_MANIFEST_DIR")),
                        "/etc/dovecot/ssl/tls.key",
                    )
                    .with_access_mode(AccessMode::ReadOnly)
                )
                .with_env_var("USER_PASSWORD", &password)
                .start()
                .await
        ),
        password,
    }
}
