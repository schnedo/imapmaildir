use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead as _, BufReader},
    marker::PhantomData,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use assertables::*;
use compio::runtime::JoinHandle;
use crossfire::{
    AsyncRx,
    mpsc::{self, Array},
};
use derivative::Derivative;
use imapmaildir::{
    Client, ClientError, SyncError, Syncer, Tls, Transient, config as config_m, logging,
    reconnecting,
};
use rstest::fixture;
use tempfile::{TempDir, tempdir};
use testcontainers::{
    Container, GenericImage, ImageExt,
    core::{AccessMode, ContainerPort, ExecCommand, Mount, WaitFor},
    runners::SyncRunner,
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

pub struct UidList {
    filename_to_uid: HashMap<String, u64>,
}
impl UidList {
    #[must_use]
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

    #[must_use]
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

    #[must_use]
    pub fn uid(&self) -> u64 {
        self.uid
    }

    #[must_use]
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
            self.path = new_path;

            true
        } else {
            false
        }
    }

    pub fn delete(self) {
        assert_ok!(fs::remove_file(self.path));
    }

    #[must_use]
    pub fn content(&self) -> &[u8] {
        &self.content
    }
}

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
    #[expect(async_fn_in_trait)]
    async fn mails(&'_ self) -> Vec<MailFile<'_>>;

    #[expect(async_fn_in_trait)]
    async fn mail_with_flag(&self) -> Option<MailFile<'_>>;

    fn add_mail(&self, content: &[u8]);
}

impl Maildir for ClientMaildir<'_> {
    fn mails(&'_ self) -> impl Future<Output = Vec<MailFile<'_>>> {
        let read_dir = assert_ok!(self.cur.read_dir());
        let mut all_mails: Vec<_> = read_dir.map(|entry| assert_ok!(entry).path()).collect();
        all_mails.sort();

        std::future::ready(
            all_mails
                .into_iter()
                .map(|mail| MailFile::new(self, None, mail))
                .collect(),
        )
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
    file_counter: RefCell<u32>,
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

        Self {
            storage,
            cur,
            new,
            file_counter: RefCell::new(100),
        }
    }
}

impl Maildir for ServerMaildir<'_> {
    fn mails(&'_ self) -> impl Future<Output = Vec<MailFile<'_>>> {
        let read_dir = assert_ok!(self.cur.read_dir());
        let mut all_mails: Vec<_> = read_dir.map(|entry| assert_ok!(entry).path()).collect();
        all_mails.sort();
        let index_location = assert_some!(self.cur.parent());
        assert_ok!(self.storage.container.exec(ExecCommand::new([
            "rm",
            assert_some!(index_location.join("dovecot-uidlist*").to_str()),
            assert_some!(index_location.join("dovecot.index*").to_str()),
            "&&",
            "doveadm",
            "index",
            "-u",
            "user",
            "'*'"
        ])));
        let uidlist = UidList::new(&self.storage.dir().join("dovecot-uidlist"));

        std::future::ready(
            all_mails
                .into_iter()
                .map(|mail| {
                    MailFile::new(
                        self,
                        Some(
                            uidlist.get_uid(assert_some!(assert_some!(mail.file_name()).to_str())),
                        ),
                        mail,
                    )
                })
                .collect(),
        )
    }

    async fn mail_with_flag(&self) -> Option<MailFile<'_>> {
        self.mails()
            .await
            .into_iter()
            .find(|mail| mail.has_flag('S'))
    }

    fn add_mail(&self, content: &[u8]) {
        let mut file_counter = self.file_counter.borrow_mut();
        let file_name = file_counter.to_string();
        let path = self.new.join(file_name);
        assert!(!assert_ok!(path.try_exists()));
        assert_ok!(fs::write(path, content));
        *file_counter += 1;
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
    container: Container<GenericImage>,
}

impl ServerMailStorage {
    /// Disconnects all sessions of the user like a server restart
    ///
    /// # Panics
    /// If kicking the user fails
    pub fn kick(&self) {
        assert_ok!(
            self.container
                .exec(ExecCommand::new(["doveadm", "kick", "user"]))
        );
    }
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
    config: Arc<config_m::Account>,
    #[expect(unused)]
    tmp_dir: TempDir,
    server_mail_storge: ServerMailStorage,
}

impl MailSetup {
    #[must_use]
    pub fn config(&self) -> &config_m::Account {
        &self.config
    }

    #[must_use]
    pub fn server_mail(&self) -> &ServerMailStorage {
        &self.server_mail_storge
    }

    #[must_use]
    pub fn client_mail(&self) -> ClientMailStorage<'_> {
        ClientMailStorage {
            dir: self.config.maildir_base_path(),
        }
    }

    /// # Panics
    /// If logging in or syncing fails
    pub async fn sync(&self, mailbox: &str) -> () {
        let config = self.config();
        let client = Client::login(
            config.connection(),
            &assert_ok!(Tls::new(config.connection())),
            config.auth(),
        )
        .await;
        let client = client.unwrap_or_else(|e| panic!("login failed: {e}"));

        Syncer::sync_once(
            mailbox,
            config.maildir_base_path(),
            config.state_dir(),
            client,
            config.sync(),
            || {},
        )
        .await
        .unwrap_or_else(|e| panic!("sync failed: {e}"));
    }

    /// # Panics
    /// If logging in or listing fails
    pub async fn list_remote(&self) -> Vec<String> {
        let config = self.config();
        let mut client = Client::login(
            config.connection(),
            &assert_ok!(Tls::new(config.connection())),
            config.auth(),
        )
        .await
        .unwrap_or_else(|e| panic!("login failed: {e}"));

        client
            .list()
            .await
            .unwrap_or_else(|e| panic!("listing failed: {e}"))
    }

    /// Configured user
    ///
    /// # Panics
    /// If the configured auth has no user
    #[must_use]
    pub fn user(&self) -> &str {
        match self.config.auth() {
            config_m::Auth::Plain(plain) => plain.user(),
            config_m::Auth::OAuth2(oauth2) => oauth2.user(),
        }
    }

    /// OAuth 2.0 auth of the configured user with a fixed `token`
    #[must_use]
    pub fn oauth2_auth(&self, token: &str) -> config_m::Auth {
        config_m::Auth::OAuth2(config_m::OAuth2Auth::new(
            self.user().to_string(),
            vec!["echo".to_string(), token.to_string()],
        ))
    }

    /// Syncs `mailbox` once after logging in with `auth`
    ///
    /// # Errors
    /// If logging in fails
    ///
    /// # Panics
    /// If syncing fails
    pub async fn sync_with_auth(
        &self,
        mailbox: &str,
        auth: &config_m::Auth,
    ) -> Result<(), ClientError> {
        let config = self.config();
        let client = Client::login(
            config.connection(),
            &assert_ok!(Tls::new(config.connection())),
            auth,
        )
        .await?;

        Syncer::sync_once(
            mailbox,
            config.maildir_base_path(),
            config.state_dir(),
            client,
            config.sync(),
            || {},
        )
        .await
        .unwrap_or_else(|e| panic!("sync failed: {e}"));

        Ok(())
    }

    /// # Panics
    /// If logging in or syncing fails
    pub async fn sync_continuous(&self, mailbox: &str) -> (JoinHandle<()>, AsyncRx<Array<()>>) {
        let (call_tx, call_rx) = mpsc::bounded_async(1);
        let config = self.config();
        let maildir_base_path = config.maildir_base_path().clone();
        let state_dir = config.state_dir().clone();
        let idle_timout = config.idle_timout();
        let sync_config = config.sync();
        let client = Client::login(
            config.connection(),
            &assert_ok!(Tls::new(config.connection())),
            config.auth(),
        )
        .await;
        let client = client.unwrap_or_else(|e| panic!("login failed: {e}"));
        let mailbox = mailbox.to_string();
        let handle = compio::runtime::spawn(async move {
            let Err(e) = Syncer::sync_continuously(
                &mailbox,
                &maildir_base_path,
                &state_dir,
                client,
                idle_timout,
                sync_config,
                move || {
                    eprintln!("calling onchange");
                    let call = call_tx.clone();
                    compio::runtime::spawn(async move {
                        assert_ok!(call.send(()).await);
                    })
                    .detach();
                },
            )
            .await;
            panic!("continuous sync failed: {e}");
        });

        (handle, call_rx)
    }
}

#[derive(Debug)]
enum SessionError {
    Login(ClientError),
    Sync(SyncError),
    Stopped,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Login(e) => write!(f, "{e}"),
            Self::Sync(e) => write!(f, "{e}"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

impl Transient for SessionError {
    fn is_transient(&self) -> bool {
        match self {
            Self::Login(e) => e.is_transient(),
            Self::Sync(e) => e.is_transient(),
            Self::Stopped => false,
        }
    }
}

/// Continuous sync reconnecting after transient errors. Stops when dropped.
pub struct ReconnectingSync {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for ReconnectingSync {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            assert_ok!(thread.join());
        }
    }
}

impl MailSetup {
    /// Runs continuous sync like the CLI does: each session on a new runtime, reconnecting after
    /// transient errors
    ///
    /// # Panics
    /// If the sync fails permanently
    #[must_use]
    pub fn sync_reconnecting(&self, mailbox: &str) -> ReconnectingSync {
        let config = self.config.clone();
        let mailbox = mailbox.to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = thread::spawn(move || {
            let tls = assert_ok!(Tls::new(config.connection()));
            let error = reconnecting(&mailbox, || {
                let rt = assert_ok!(compio::runtime::Runtime::new());
                rt.block_on(async {
                    let session = async {
                        let client = Client::login(config.connection(), &tls, config.auth())
                            .await
                            .map_err(SessionError::Login)?;
                        Syncer::sync_continuously(
                            &mailbox,
                            config.maildir_base_path(),
                            config.state_dir(),
                            client,
                            config.idle_timout(),
                            config.sync(),
                            || {},
                        )
                        .await
                        .map_err(SessionError::Sync)
                    };
                    let stop = async {
                        while !stopped.load(Ordering::Relaxed) {
                            compio::time::sleep(Duration::from_millis(50)).await;
                        }
                        Err(SessionError::Stopped)
                    };
                    futures::pin_mut!(session, stop);
                    futures::future::select(session, stop)
                        .await
                        .factor_first()
                        .0
                })
            });
            assert!(
                matches!(error, SessionError::Stopped),
                "reconnecting sync failed: {error}"
            );
        });

        ReconnectingSync {
            stop,
            thread: Some(thread),
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

/// Secret of the HS256 key in `mock/oauth2-keys`, which dovecot validates tokens with
const OAUTH2_KEY: &[u8] = b"imapmaildir-test-key-0123456789ab";

/// Signs a JWT for `user`, that dovecot accepts as OAuth 2.0 access token
#[must_use]
pub fn oauth2_token(user: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"{user}","exp":4102444800}}"#));
    let signing_input = format!("{header}.{claims}");
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, OAUTH2_KEY);
    let signature = URL_SAFE_NO_PAD.encode(ring::hmac::sign(&key, signing_input.as_bytes()));

    format!("{signing_input}.{signature}")
}

/// Disables QRESYNC on the server if `IMAPMAILDIR_TEST_NO_QRESYNC` is set to test the CONDSTORE
/// fallback. Replaces LITERAL+ with LITERAL- if `IMAPMAILDIR_TEST_LITERAL_MINUS` is set to test
/// synchronizing literals.
fn dovecot_config(tmp_dir: &Path) -> String {
    let config = mock_path!("dovecot.conf");
    let no_qresync = std::env::var_os("IMAPMAILDIR_TEST_NO_QRESYNC").is_some();
    let literal_minus = std::env::var_os("IMAPMAILDIR_TEST_LITERAL_MINUS").is_some();
    // like servers without OAUTHBEARER and SASL-IR, which wait for the initial client response
    let xoauth2 = std::env::var_os("IMAPMAILDIR_TEST_XOAUTH2").is_some();
    if !no_qresync && !literal_minus && !xoauth2 {
        return config.to_string();
    }
    let mut content = assert_ok!(fs::read_to_string(config));
    if no_qresync {
        content.push_str("\nimap_capability {\n  QRESYNC = no\n}\n");
    }
    if literal_minus {
        content.push_str("\nimap_literal_minus = yes\n");
    }
    if xoauth2 {
        content.push_str(
            "\nauth_mechanisms = plain login xoauth2\nimap_capability {\n  SASL-IR = no\n}\n",
        );
    }
    let path = tmp_dir.join("dovecot.conf");
    assert_ok!(fs::write(&path, content));
    fix_permissions(&path);

    path.to_string_lossy().into_owned()
}

#[fixture]
pub async fn mail_setup(__setup_logging: ()) -> MailSetup {
    let password = "password".to_string();
    let tmp_dir = assert_ok!(tempdir());
    let client_base_path = tmp_dir.path().join("local");
    copy_dir(mock_path!("data/local"), &client_base_path);
    let server_dir = tmp_dir.path().join("remote");
    copy_dir(mock_path!("data/remote"), &server_dir);
    fix_permissions(&server_dir);
    let dovecot_config = dovecot_config(tmp_dir.path());
    let container = assert_ok!(
        GenericImage::new("dovecot/dovecot", "2.4.4-dev")
            .with_exposed_port(IMAPS_PORT)
            .with_wait_for(WaitFor::message_on_stdout("starting up for"))
            .with_mount(
                Mount::bind_mount(CERTIFICATE_PATH, "/etc/dovecot/ssl/tls.crt")
                    .with_access_mode(AccessMode::ReadOnly),
            )
            .with_mount(
                Mount::bind_mount(mock_path!("private_key.pem"), "/etc/dovecot/ssl/tls.key")
                    .with_access_mode(AccessMode::ReadOnly),
            )
            .with_mount(
                Mount::bind_mount(dovecot_config, "/etc/dovecot/dovecot.conf")
                    .with_access_mode(AccessMode::ReadOnly),
            )
            .with_mount(
                Mount::bind_mount(mock_path!("oauth2-keys"), "/etc/dovecot/oauth2-keys")
                    .with_access_mode(AccessMode::ReadOnly),
            )
            .with_mount(Mount::bind_mount(
                server_dir.to_string_lossy(),
                "/srv/vmail/user/mail"
            ))
            .with_env_var("USER_PASSWORD", &password)
            .start()
    );

    MailSetup {
        config: Arc::new(config_m::Account::new(
            config_m::Auth::Plain(config_m::PlainAuth::new(
                "user".to_string(),
                vec!["echo".to_string(), password],
            )),
            assert_ok!(container.get_host()).to_string(),
            assert_ok!(container.get_host_port_ipv4(IMAPS_PORT)),
            Some(PathBuf::from(CERTIFICATE_PATH)),
            vec!["INBOX".to_string(), "DRAFT".to_string()],
            client_base_path.clone(),
            client_base_path,
            Duration::from_secs(30),
            Vec::new(),
            config_m::SyncConfig::default(),
        )),
        tmp_dir,
        server_mail_storge: ServerMailStorage {
            dir: server_dir,
            container,
        },
    }
}
