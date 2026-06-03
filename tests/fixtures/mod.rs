use std::path::PathBuf;

use assertables::assert_ok;
use imapmaildir::{config as config_m, logging};
use rstest::fixture;
use tempfile::{TempDir, tempdir};
use testcontainers::{
    ContainerAsync, ContainerRequest, CopyDataSource, CopyTargetOptions, GenericImage, Healthcheck,
    ImageExt,
    core::{AccessMode, ContainerPort, Mount, WaitFor},
    runners::AsyncRunner,
};

const IMAPS_PORT: ContainerPort = ContainerPort::Tcp(31993);

struct MockContainerRequest {
    image: ContainerRequest<GenericImage>,
    password: String,
}

impl MockContainerRequest {
    fn with_copy_to(
        self,
        target: impl Into<CopyTargetOptions>,
        source: impl Into<CopyDataSource>,
    ) -> MockContainerRequest {
        Self {
            image: self.image.with_copy_to(target, source),
            ..self
        }
    }

    async fn start(self) -> MockServer {
        let container = assert_ok!(self.image.start().await);
        let tmp = assert_ok!(tempdir());
        MockServer {
            config: config_m::Account::new(
                config_m::Auth::Plain(config_m::PlainAuth::new(
                    "user".to_string(),
                    vec!["echo".to_string(), self.password],
                )),
                assert_ok!(container.get_host().await).to_string(),
                assert_ok!(container.get_host_port_ipv4(IMAPS_PORT).await),
                Some(PathBuf::from(CERTIFICATE_PATH)),
                vec!["INBOX".to_string(), "DRAFT".to_string()],
                tmp.path().to_path_buf(),
                tmp.path().to_path_buf(),
            ),
            container,
            tmp_dir: tmp,
        }
    }
}

pub struct MockServer {
    config: config_m::Account,
    container: ContainerAsync<GenericImage>,
    #[expect(unused)]
    tmp_dir: TempDir,
}

impl MockServer {
    pub fn config(&self) -> &config_m::Account {
        &self.config
    }

    pub fn container(&self) -> &ContainerAsync<GenericImage> {
        &self.container
    }
}

#[fixture]
#[once]
fn __setup_logging() {
    logging::init(log::LevelFilter::Trace);
}

macro_rules! mock_path {
    ($($suffix:literal),*) => {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock/", $($suffix),*)
    };
}

const CERTIFICATE_PATH: &str = mock_path!("certificate.crt");

#[fixture]
fn container(__setup_logging: ()) -> MockContainerRequest {
    let password = "password".to_string();
    MockContainerRequest {
        image: GenericImage::new("dovecot/dovecot", "2.4.4-dev")
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
                Mount::bind_mount(CERTIFICATE_PATH, "/etc/dovecot/ssl/tls.crt")
                    .with_access_mode(AccessMode::ReadOnly),
            )
            .with_mount(
                Mount::bind_mount(mock_path!("private_key.pem"), "/etc/dovecot/ssl/tls.key")
                    .with_access_mode(AccessMode::ReadOnly),
            )
            .with_mount(
                Mount::bind_mount(mock_path!("dovecot.conf"), "/etc/dovecot/dovecot.conf")
                    .with_access_mode(AccessMode::ReadOnly),
            )
            .with_env_var("USER_PASSWORD", &password),
        password,
    }
}

#[fixture]
pub async fn no_changes_server(container: MockContainerRequest) -> MockServer {
    container
        .with_copy_to(
            "/srv/vmail/user",
            PathBuf::from(mock_path!("data/remote/no_changes")),
        )
        .start()
        .await
}
