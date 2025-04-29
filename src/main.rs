mod config;
mod imap;
mod logging;

use anyhow::Result;
use config::Config;
use imap::{Client, Connection};
use maildir::Maildir;

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();

    let config = Config::load_from_file();

    // let (connection, _) = Connection::connect_to(config.host(), *config.port()).await;
    // let client = Client::new(connection);
    // let mut session = client.login(config.user(), &config.password()).await?;
    // session.select("INBOX").await?;
    // session.idle().await;

    let maildir_path = config.maildir().clone();
    let maildir = Maildir::from(maildir_path);
    maildir
        .create_dirs()
        .expect("should be able to create maildir dirs");

    Ok(())
}
