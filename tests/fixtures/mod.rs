use std::{
    fs::{self},
    marker::PhantomData,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use assertables::*;
use imapmaildir::{config as config_m, logging};
use rstest::fixture;
use tempfile::{TempDir, tempdir};
use testcontainers::{
    ContainerAsync, GenericImage, Healthcheck, ImageExt,
    core::{AccessMode, ContainerPort, ExecCommand, Mount, WaitFor},
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

mod mailfile {
    #![expect(clippy::elidable_lifetime_names)]
    use assertables::*;
    use std::{
        collections::{HashMap, HashSet},
        fs::{self, File},
        io::{BufRead, BufReader},
        marker::PhantomData,
        path::{Path, PathBuf},
    };

    use derivative::Derivative;

    use crate::fixtures::Maildir;

    pub struct UidList {
        filename_to_uid: HashMap<String, u64>,
    }
    impl UidList {
        pub fn new(path: &Path) -> Self {
            let reader = BufReader::new(assert_ok!(File::open(path)));
            let mut lines = reader.lines();
            lines.next();
            let mut filename_to_uid = HashMap::new();
            for line in lines {
                let line = assert_ok!(line);
                let (uid, rest) = assert_some!(line.split_once(' '));
                let uid: u64 = assert_ok!(uid.parse());
                let (_, filename) = assert_some!(rest.split_once(':'));
                let filename = if let Some((prefix, _)) = filename.rsplit_once(":2,") {
                    prefix
                } else {
                    filename
                };
                filename_to_uid.insert(filename.to_string(), uid);
            }

            Self { filename_to_uid }
        }

        pub fn get_uid(&self, filename: &str) -> u64 {
            let filename = if let Some((prefix, _)) = filename.rsplit_once(":2,") {
                prefix
            } else {
                filename
            };

            *assert_some!(self.filename_to_uid.get(filename))
        }
    }

    #[derive(Derivative)]
    #[derivative(Debug, PartialEq, Eq, Hash)]
    pub struct MailFile<'a> {
        pd: PhantomData<&'a ()>,
        uid: u64,
        content: Vec<u8>,
        flags: String,
        #[derivative(PartialEq = "ignore")]
        #[derivative(PartialEq = "ignore", Hash = "ignore")]
        path: PathBuf,
    }

    impl MailFile<'_> {
        pub fn new(_maildir: &'_ impl Maildir, uid: Option<u64>, path: PathBuf) -> Self {
            let content = assert_ok!(fs::read(&path));
            let name = assert_some!(path.file_name());
            let name = name.to_string_lossy();
            let (prefix, flags) = assert_some!(name.rsplit_once(":2,"));
            let uid = if let Some(uid) = uid {
                uid
            } else {
                let uid = prefix.rsplit_once("U=").map_or(prefix, |(_, uid)| uid);

                assert_ok!(uid.parse())
            };

            Self {
                pd: PhantomData,
                content,
                flags: flags.into(),
                uid,
                path,
            }
        }

        pub fn has_flag(&self, flag: char) -> bool {
            self.flags.contains(flag)
        }

        pub fn add_flag(&mut self, flag: char) -> bool {
            let (flags, mut file_name) = if self.flags.is_empty() {
                (
                    String::from(flag),
                    assert_some!(self.path.file_name())
                        .to_string_lossy()
                        .to_string(),
                )
            } else {
                let mut flags: HashSet<_> = self.flags.chars().collect();
                if !flags.insert(flag) {
                    return false;
                }
                let mut flags: Vec<_> = flags.drain().collect();
                flags.sort_unstable();
                let flags: String = flags.into_iter().collect();
                let file_name = assert_some!(self.path.file_name()).to_string_lossy();
                let (prefix, _) = assert_some!(file_name.rsplit_once(":2,"));
                let mut file_name = String::with_capacity(prefix.len() + 3 + flags.len());
                file_name.push_str(prefix);
                file_name.push_str(":2,");

                (flags, file_name)
            };
            file_name.push_str(&flags);
            let new_path = self.path.with_file_name(file_name);
            assert_ok!(fs::rename(&self.path, &new_path));
            self.flags = flags;
            self.path = new_path;

            true
        }

        pub fn remove_flag(&mut self, flag: char) -> bool {
            if self.has_flag(flag) {
                let mut flags: HashSet<_> = self.flags.chars().collect();
                flags.remove(&flag);
                let mut flags: Vec<_> = flags.into_iter().collect();
                flags.sort_unstable();
                let flags: String = flags.into_iter().collect();

                let file_name = assert_some!(self.path.file_name())
                    .to_string_lossy()
                    .to_string();
                let (prefix, _) = assert_some!(file_name.rsplit_once(":2,"));
                let mut file_name = String::with_capacity(prefix.len() + 3 + flags.len());
                file_name.push_str(prefix);
                file_name.push_str(":2,");
                file_name.push_str(&flags);
                let new_path = self.path.with_file_name(file_name);
                assert_ok!(fs::rename(&self.path, &new_path));
                self.flags = flags;

                true
            } else {
                false
            }
        }

        pub fn delete(self) {
            assert_ok!(fs::remove_file(self.path));
        }
    }
}
pub use mailfile::MailFile;

use crate::fixtures::mailfile::UidList;

pub struct ClientMaildir<'a> {
    pd: PhantomData<&'a ServerMailStorage>,
    cur: PathBuf,
}

impl ClientMaildir<'_> {
    fn new(storage: &'_ impl MailStorage, name: &str) -> Self {
        let mut cur = storage.dir().join(name);
        cur.push("cur");

        Self {
            pd: PhantomData,
            cur,
        }
    }
}

pub trait Maildir {
    async fn mails(&'_ self) -> Vec<MailFile<'_>>;

    async fn mail_with_flag(&self) -> Option<MailFile<'_>>;

    fn add_mail(&self, content: &[u8]);
}

impl Maildir for ClientMaildir<'_> {
    async fn mails(&'_ self) -> Vec<MailFile<'_>> {
        let read_dir = assert_ok!(self.cur.read_dir());
        let mut all_mails: Vec<_> = read_dir.map(|entry| assert_ok!(entry).path()).collect();
        all_mails.sort();

        all_mails
            .into_iter()
            .map(|mail| MailFile::new(self, None, mail))
            .collect()
    }

    async fn mail_with_flag(&self) -> Option<MailFile<'_>> {
        self.mails()
            .await
            .into_iter()
            .find(|mail| mail.has_flag('S'))
    }

    fn add_mail(&self, content: &[u8]) {
        let file_name = String::from("foo:2,S");
        let path = self.cur.join(file_name);
        assert!(!assert_ok!(path.try_exists()));
        assert_ok!(fs::write(path, content));
    }
}

pub struct ServerMaildir<'a> {
    storage: &'a ServerMailStorage,
    cur: PathBuf,
    new: PathBuf,
}

impl<'a> ServerMaildir<'a> {
    fn new(storage: &'a ServerMailStorage, name: &str) -> Self {
        let (cur, new) = if name == "INBOX" {
            (storage.dir().join("cur"), storage.dir().join("new"))
        } else {
            let mut cur = storage.dir().join(name);
            let new = cur.join("new");
            cur.push("cur");

            (cur, new)
        };

        Self { storage, cur, new }
    }
}

impl Maildir for ServerMaildir<'_> {
    async fn mails(&'_ self) -> Vec<MailFile<'_>> {
        let read_dir = assert_ok!(self.cur.read_dir());
        let mut all_mails: Vec<_> = read_dir.map(|entry| assert_ok!(entry).path()).collect();
        all_mails.sort();
        let index_location = assert_some!(self.cur.parent());
        assert_ok!(
            self.storage
                .container
                .exec(ExecCommand::new([
                    "rm",
                    assert_some!(index_location.join("dovecot-uidlist*").to_str()),
                    assert_some!(index_location.join("dovecot.index*").to_str()),
                    "&&",
                    "doveadm",
                    "index",
                    "-u",
                    "user",
                    "'*'"
                ]))
                .await
        );
        let uidlist = UidList::new(&self.storage.dir().join("dovecot-uidlist"));

        all_mails
            .into_iter()
            .map(|mail| {
                MailFile::new(
                    self,
                    Some(uidlist.get_uid(assert_some!(assert_some!(mail.file_name()).to_str()))),
                    mail,
                )
            })
            .collect()
    }

    async fn mail_with_flag(&self) -> Option<MailFile<'_>> {
        self.mails()
            .await
            .into_iter()
            .find(|mail| mail.has_flag('S'))
    }

    fn add_mail(&self, content: &[u8]) {
        let file_name = String::from("foo");
        let path = self.new.join(file_name);
        assert!(!assert_ok!(path.try_exists()));
        assert_ok!(fs::write(path, content));
    }
}

pub struct ClientMailStorage<'a> {
    dir: &'a Path,
}

impl MailStorage for ClientMailStorage<'_> {
    fn dir(&self) -> &Path {
        self.dir
    }
    fn mailbox(&'_ self, name: &str) -> impl Maildir {
        ClientMaildir::new(self, name)
    }
}

pub struct ServerMailStorage {
    dir: PathBuf,
    container: ContainerAsync<GenericImage>,
}

impl MailStorage for ServerMailStorage {
    fn dir(&self) -> &Path {
        &self.dir
    }
    fn mailbox(&'_ self, name: &str) -> impl Maildir {
        ServerMaildir::new(self, name)
    }
}

pub trait MailStorage: Sized {
    fn dir(&self) -> &Path;
    fn mailbox(&'_ self, name: &str) -> impl Maildir;
    fn wipe(&self) {
        for entry in assert_ok!(self.dir().read_dir()) {
            let entry = assert_ok!(entry);
            let ftype = assert_ok!(entry.file_type());
            if ftype.is_dir() {
                assert_ok!(fs::remove_dir_all(entry.path()));
            } else {
                assert_ok!(fs::remove_file(entry.path()));
            }
        }
    }
}

pub struct MailSetup {
    config: config_m::Account,
    #[expect(unused)]
    tmp_dir: TempDir,
    server_mail_storge: ServerMailStorage,
}

impl MailSetup {
    pub fn config(&self) -> &config_m::Account {
        &self.config
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

fn fix_permissions(path: &Path) {
    let mut permissions = assert_ok!(path.metadata()).permissions();
    permissions.set_mode(0o777);
    assert_ok!(fs::set_permissions(path, permissions));
    if path.is_dir() {
        for path in assert_ok!(fs::read_dir(path)) {
            let entry = assert_ok!(path);
            fix_permissions(&entry.path());
        }
    }
}

const CERTIFICATE_PATH: &str = mock_path!("certificate.crt");

#[fixture]
pub async fn mail_setup(__setup_logging: ()) -> MailSetup {
    let password = "password".to_string();
    let tmp_dir = assert_ok!(tempdir());
    let client_base_path = tmp_dir.path().join("local");
    copy_dir(mock_path!("data/local"), &client_base_path);
    let server_dir = tmp_dir.path().join("remote");
    copy_dir(mock_path!("data/remote"), &server_dir);
    fix_permissions(&server_dir);
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
        tmp_dir,
        server_mail_storge: ServerMailStorage {
            dir: server_dir,
            container,
        },
    }
}
