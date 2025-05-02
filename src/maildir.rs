use std::{
    fs::{self, DirBuilder},
    io::Error,
    os::unix::fs::DirBuilderExt as _,
    path::PathBuf,
    process,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use log::{info, trace};
use rustix::system::uname;
use tokio::task::{spawn_blocking, JoinHandle};

use crate::imap::RemoteMail;

pub struct Maildir {
    maildir_root: Arc<PathBuf>,
}

impl Maildir {
    pub fn new(mut maildir_path: PathBuf) -> Self {
        info!("using mailbox in {maildir_path:#?}");
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);

        maildir_path.push("tmp");
        builder
            .create(maildir_path.as_path())
            .expect("creation of tmp subdir should succeed");
        maildir_path.pop();
        maildir_path.push("new");
        builder
            .create(maildir_path.as_path())
            .expect("creation of new subdir should succeed");
        maildir_path.pop();
        maildir_path.push("cur");
        builder
            .create(maildir_path.as_path())
            .expect("creation of cur subdir should succeed");
        maildir_path.pop();

        Self {
            maildir_root: Arc::new(maildir_path),
        }
    }

    // Algorithm
    // Technically the program should chdir into maildir_root to prevent issues if the path of
    // maildir_root changes. Setting current_dir is a process wide operation though and will mess
    // up relative file operations in the spawn_blocking threads.
    pub fn store_new(&self, mail: RemoteMail) -> JoinHandle<Result<(), Error>> {
        let maildir_path = self.maildir_root.clone();
        spawn_blocking(move || {
            let filename = Self::generate_filename(&mail);
            let file_path = maildir_path.join(format!("tmp/{filename}"));
            trace!("writing to {file_path:?}");
            fs::write(file_path.as_path(), mail.content())
                .expect("writing new mail to tmp should succeed");
            fs::rename(file_path, maildir_path.join(format!("new/{filename}")))
        })
    }

    fn generate_filename(mail: &RemoteMail) -> String {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("should be able to get unix time");
        let secs = time.as_secs();
        let nanos = time.subsec_nanos();
        let hostname = uname();
        let hostname = hostname.nodename().to_string_lossy();
        let pid = process::id();
        let uid = mail.uid();
        format!("{secs}.U={uid},P={pid},N={nanos}.{hostname}:2,")
    }
}
