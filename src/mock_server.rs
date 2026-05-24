use assertables::assert_ok;
use rstest::fixture;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{AccessMode, IntoContainerPort, Mount, WaitFor, wait::HttpWaitStrategy},
    runners::AsyncRunner,
};

pub struct MockServer {
    server: ContainerAsync<GenericImage>,
}

impl MockServer {
    pub async fn hostname(&self) -> String {
        assert_ok!(self.server.get_host().await).to_string()
    }

    pub async fn port(&self) -> u16 {
        assert_ok!(self.server.get_host_port_ipv4(3993).await)
    }
}

#[fixture]
pub async fn server() -> MockServer {
    MockServer {
        server: assert_ok!(
            GenericImage::new("greenmail/standalone", "2.1.8")
                .with_exposed_port(3993.tcp())
                .with_exposed_port(8080.tcp())
                .with_wait_for(WaitFor::http(
                    HttpWaitStrategy::new("/api/service/readiness")
                        .with_port(8080.tcp())
                        .with_expected_status_code(200u16)
                ))
                .with_mount(
                    Mount::bind_mount(
                        format!("{}/mock/data", env!("CARGO_MANIFEST_DIR")),
                        "/preload",
                    )
                    .with_access_mode(AccessMode::ReadOnly)
                )
                .with_mount(
                    Mount::bind_mount(
                        format!("{}/mock/keystore.p12", env!("CARGO_MANIFEST_DIR")),
                        "/keystore.p12",
                    )
                    .with_access_mode(AccessMode::ReadOnly)
                )
                .with_env_var(
                    "GREENMAIL_OPTS",
                    [
                        "-Dgreenmail.setup.test.imaps",
                        "-Dgreenmail.setup.test.smtp",
                        "-Dgreenmail.auth.disabled",
                        "-Dgreenmail.preload.dir=/preload",
                        "-Dgreenmail.tls.keystore.file=/keystore.p12",
                        "-Dgreenmail.tls.keystore.password=password",
                        "-Dgreenmail.hostname=0.0.0.0",
                    ]
                    .join(" "),
                )
                .start()
                .await
        ),
    }
}
