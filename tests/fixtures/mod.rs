use std::{
    fs,
    marker::PhantomData,
    path::{Path, PathBuf},
};

use assertables::assert_ok;
use imapmaildir::{config as config_m, logging};
use rstest::fixture;
use tempfile::{TempDir, tempdir};
use testcontainers::{
    ContainerAsync, GenericImage, Healthcheck, ImageExt,
    core::{AccessMode, ContainerPort, Mount, WaitFor},
    runners::AsyncRunner,
};

const IMAPS_PORT: ContainerPort = ContainerPort::Tcp(31993);

macro_rules! mock_path {
    ($($suffix:literal),*) => {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock/", $($suffix),*)
    };
}

fn copy_dir(from: impl AsRef<Path>, to: impl AsRef<Path>) {
    let from = from.as_ref();
    let to = to.as_ref();
    assert_ok!(fs::create_dir_all(to));
    for entry in assert_ok!(from.read_dir()) {
        let entry = assert_ok!(entry);
        let ftype = assert_ok!(entry.file_type());
        if ftype.is_dir() {
            copy_dir(entry.path(), to.join(entry.file_name()));
        } else {
            if entry.file_name() == ".gitkeep" {
                continue;
            }
            assert_ok!(fs::copy(entry.path(), to.join(entry.file_name())));
        }
    }
}

#[derive(Debug)]
pub struct MailFile<'a> {
    pd: PhantomData<&'a ()>,
}

impl<'a> MailFile<'a> {
    pub fn new(_maildir: &'a Maildir) -> Self {
        Self { pd: PhantomData }
    }
}

pub struct Maildir<'a> {
    pd: PhantomData<&'a ServerMailStorage>,
    cur: PathBuf,
}

impl Maildir<'_> {
    fn new(storage: &'_ impl MailStorage, top_level_inbox: bool, name: &str) -> Self {
        let cur = if top_level_inbox && name == "INBOX" {
            storage.dir().join("cur")
        } else {
            let mut cur = storage.dir().join(name);
            cur.push("cur");

            cur
        };
        Maildir {
            pd: PhantomData,
            cur,
        }
    }

    pub fn mails(&'_ self) -> impl Iterator<Item = MailFile<'_>> {
        let read_dir = assert_ok!(self.cur.read_dir());
        read_dir
            .map(|entry| assert_ok!(entry))
            .map(|_entry| MailFile::new(self))
    }
}

pub struct ClientMailStorage<'a> {
    dir: &'a Path,
}

impl MailStorage for ClientMailStorage<'_> {
    fn dir(&self) -> &Path {
        self.dir
    }
    fn mailbox(&'_ self, name: &str) -> Maildir<'_> {
        Maildir::new(self, false, name)
    }
}

pub struct ServerMailStorage {
    dir: PathBuf,
}

impl MailStorage for ServerMailStorage {
    fn dir(&self) -> &Path {
        &self.dir
    }
    fn mailbox(&'_ self, name: &str) -> Maildir<'_> {
        Maildir::new(self, true, name)
    }
}

pub trait MailStorage: Sized {
    fn dir(&self) -> &Path;

    fn mailbox(&'_ self, name: &str) -> Maildir<'_>;
}

pub struct MailSetup {
    config: config_m::Account,
    container: ContainerAsync<GenericImage>,
    #[expect(unused)]
    tmp_dir: TempDir,
    server_mail_storge: ServerMailStorage,
}

impl MailSetup {
    pub fn config(&self) -> &config_m::Account {
        &self.config
    }

    pub fn container(&self) -> &ContainerAsync<GenericImage> {
        &self.container
    }

    pub fn server_mail(&self) -> &ServerMailStorage {
        &self.server_mail_storge
    }

    pub fn client_mail(&self) -> ClientMailStorage<'_> {
        ClientMailStorage {
            dir: self.config.maildir_base_path(),
        }
    }
}

#[fixture]
#[once]
fn __setup_logging() {
    logging::init(log::LevelFilter::Trace);
}

const CERTIFICATE_PATH: &str = mock_path!("certificate.crt");

#[fixture]
pub async fn mail_setup(__setup_logging: ()) -> MailSetup {
    let password = "password".to_string();
    let tmp_dir = assert_ok!(tempdir());
    copy_dir(mock_path!("data/local"), tmp_dir.path());
    let client_base_path = tmp_dir.path().join("data/local");
    let server_dir = tmp_dir.path().join("remote");
    copy_dir(mock_path!("data/remote"), &server_dir);
    let container = assert_ok!(
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
            .with_mount(Mount::bind_mount(
                server_dir.to_string_lossy(),
                "/srv/vmail/user/mail"
            ))
            .with_env_var("USER_PASSWORD", &password)
            .start()
            .await
    );

    MailSetup {
        config: config_m::Account::new(
            config_m::Auth::Plain(config_m::PlainAuth::new(
                "user".to_string(),
                vec!["echo".to_string(), password],
            )),
            assert_ok!(container.get_host().await).to_string(),
            assert_ok!(container.get_host_port_ipv4(IMAPS_PORT).await),
            Some(PathBuf::from(CERTIFICATE_PATH)),
            vec!["INBOX".to_string(), "DRAFT".to_string()],
            client_base_path.clone(),
            client_base_path,
        ),
        container,
        tmp_dir,
        server_mail_storge: ServerMailStorage { dir: server_dir },
    }
}
