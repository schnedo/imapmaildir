use std::{
    fs::{self, read_dir, DirBuilder, OpenOptions},
    io::{Error, Write},
    os::unix::fs::{DirBuilderExt as _, MetadataExt},
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use log::{info, trace};
use rustix::system::uname;
use thiserror::Error;
use tokio::task::{spawn_blocking, JoinHandle};

use crate::{
    imap::RemoteMail,
    sync::{Flag, MailMetadata},
};

pub struct Maildir {
    new: Arc<PathBuf>,
    cur: Arc<PathBuf>,
    tmp: Arc<PathBuf>,
}

impl Maildir {
    pub fn new(maildir_path: &Path) -> Self {
        info!("using mailbox in {maildir_path:#?}");
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);

        let tmp = maildir_path.join("tmp");
        builder
            .create(tmp.as_path())
            .expect("creation of tmp subdir should succeed");
        let new = maildir_path.join("new");
        builder
            .create(new.as_path())
            .expect("creation of new subdir should succeed");
        let cur = maildir_path.join("cur");
        builder
            .create(cur.as_path())
            .expect("creation of cur subdir should succeed");

        Self {
            new: Arc::new(new),
            cur: Arc::new(cur),
            tmp: Arc::new(tmp),
        }
    }

    // Algorithm
    // Technically the program should chdir into maildir_root to prevent issues if the path of
    // maildir_root changes. Setting current_dir is a process wide operation though and will mess
    // up relative file operations in the spawn_blocking threads.
    pub fn store_new(&self, mail: RemoteMail) -> JoinHandle<Result<(), Error>> {
        let new = self.new.clone();
        let tmp = self.tmp.clone();
        spawn_blocking(move || {
            let filename = Self::generate_filename();
            let file_path = tmp.join(&filename);

            trace!("writing to {file_path:?}");
            let Ok(mut file) = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file_path)
            else {
                todo!("handle tmp file creation errors");
            };

            file.write_all(mail.content())
                .expect("writing new mail to tmp should succeed");
            file.sync_all()
                .expect("writing new tmp mail to disc should succeed");

            let uid = mail.uid();
            let meta = file
                .metadata()
                .expect("reading tmp file metadata should succeed");
            let size = meta.size();
            let mut flags = String::with_capacity(6);
            for flag in mail.flags() {
                if let Ok(char_flag) = flag.try_into() {
                    flags.push(char_flag);
                }
            }
            fs::rename(
                file_path,
                new.join(format!("{filename},S={size},U={uid}:2,{flags}")),
            )
        })
    }

    fn generate_filename() -> String {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("should be able to get unix time");
        let secs = time.as_secs();
        let nanos = time.subsec_nanos();
        let hostname = uname();
        let hostname = hostname.nodename().to_string_lossy();
        let pid = process::id();
        format!("{secs}.P{pid}N{nanos}.{hostname}")
    }

    pub fn list_cur(&self) -> impl Iterator<Item = MailMetadata> {
        read_dir(self.cur.as_path())
            .expect("cur should be readable")
            .map(|entry| {
                let filename = entry
                    .expect("entry of cur should be readable")
                    .file_name()
                    .into_string()
                    .expect("converting filename from OsString to String should be possible");
                let (filename, flags) = filename.rsplit_once(',').expect("flags should be present");
                let flags = flags.chars().map(Flag::from).collect();
                let uid_field = filename
                    .rsplit_once(':')
                    .expect("filename should contain :")
                    .0
                    .rsplit_once('=')
                    .expect("filename should contain =");
                assert_eq!(uid_field.0, "U");
                let uid = uid_field
                    .0
                    .parse::<u32>()
                    .expect("uid field should be u32")
                    .into();

                MailMetadata::new(uid, flags)
            })
    }

    pub fn is_empty(&self) -> bool {
        self.cur.is_empty() && self.new.is_empty() && self.tmp.is_empty()
    }
}

impl From<char> for Flag {
    fn from(value: char) -> Self {
        match value {
            'D' => Flag::Draft,
            'F' => Flag::Flagged,
            'R' => Flag::Answered,
            'S' => Flag::Seen,
            'T' => Flag::Deleted,
            _ => panic!("unknown flag"),
        }
    }
}

#[derive(Error, Debug)]
#[error("Unknown Maildir flag")]
struct UnknownMaildirFlagError {}

impl TryFrom<&Flag> for char {
    type Error = UnknownMaildirFlagError;

    fn try_from(value: &Flag) -> Result<Self, Self::Error> {
        match value {
            Flag::Seen => Ok('S'),
            Flag::Answered => Ok('R'),
            Flag::Flagged => Ok('F'),
            Flag::Deleted => Ok('T'),
            Flag::Draft => Ok('D'),
            Flag::Recent => Err(UnknownMaildirFlagError {}),
        }
    }
}

trait IsEmpty {
    fn is_empty(&self) -> bool;
}

impl IsEmpty for PathBuf {
    fn is_empty(&self) -> bool {
        self.read_dir()
            .expect("dir should be readable")
            .next()
            .is_none()
    }
}
