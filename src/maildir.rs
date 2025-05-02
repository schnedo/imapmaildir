use std::{
    fs::{self, DirBuilder},
    io::Error,
    os::unix::fs::DirBuilderExt as _,
    path::PathBuf,
    sync::Arc,
};

use log::{info, trace};
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

    pub fn store_new(&self, mail: RemoteMail) -> JoinHandle<Result<(), Error>> {
        let maildir_path = self.maildir_root.clone();
        spawn_blocking(move || {
            let filename = "new";
            let file_path = maildir_path.join(format!("tmp/{filename}"));
            trace!("writing to {file_path:?}");
            fs::write(file_path.as_path(), mail.content())
        })
    }
}
