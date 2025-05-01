use std::{path::PathBuf, sync::Arc};

use log::info;
use tokio::task::{spawn_blocking, JoinHandle};

use crate::imap::RemoteMail;

pub struct Maildir {
    maildir_path: Arc<PathBuf>,
}

impl Maildir {
    pub fn new(maildir_path: Arc<PathBuf>) -> Self {
        if maildir_path
            .try_exists()
            .expect("maildir_path should be accessible")
        {
            info!("using mailbox in {maildir_path:#?}");
        } else {
            info!("creating new maildir in {maildir_path:#?}");
        }

        Self { maildir_path }
    }

    #[expect(clippy::unused_async)]
    pub async fn store_new(&self, _mail: RemoteMail) -> JoinHandle<()> {
        let maildir_path = self.maildir_path.clone();
        spawn_blocking(move || todo!("store_new {:#?}", maildir_path))
    }
}
