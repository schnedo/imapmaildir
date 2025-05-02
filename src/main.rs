mod config;
mod imap;
mod logging;
mod maildir;

use anyhow::Result;
use config::Config;
use imap::{Client, Connection};
use maildir::Maildir;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    logging::init();

    let config = Config::load_from_file();

    let (connection, _) = Connection::connect_to(config.host(), *config.port()).await;
    let client = Client::new(connection);
    let mut session = client.login(config.user(), &config.password()).await?;
    session.select("INBOX").await?;
    let mails = session.fetch("6106").await;

    let mailbox = Maildir::new(config.maildir());

    let join_handles = mails.into_iter().map(|mail| mailbox.store_new(mail));
    for handle in join_handles {
        handle.await;
    }
    // session.idle().await;

    Ok(())
}
