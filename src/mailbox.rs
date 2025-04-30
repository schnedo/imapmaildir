use std::path::Path;

use maildir::{Maildir, MaildirError};
use tokio::task::{spawn_blocking, JoinHandle};

use crate::imap::RemoteMail;

pub struct Mailbox<'a> {
    maildir_path: &'a Path,
}

impl<'a> Mailbox<'a> {
    pub fn new(maildir_path: &'a Path) -> Self {
        Self { maildir_path }
    }

    #[expect(clippy::unused_async)]
    pub async fn store_new(&self, mail: RemoteMail) -> JoinHandle<Result<String, MaildirError>> {
        let maildir = self.maildir_path.to_path_buf();
        spawn_blocking(move || {
            let maildir = Maildir::from(maildir);
            maildir.store_new(mail.content())
        })
    }
}
