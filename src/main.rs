mod config;
mod imap;
mod logging;
mod maildir;
mod sync;

use anyhow::Result;
use config::Config;
use imap::{Client, Connection};
use maildir::MaildirRepository;
use sync::Syncer;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    logging::init();

    let config = Config::load_from_file();

    let (connection, _) = Connection::connect_to(config.host(), *config.port()).await;
    let client = Client::new(connection);
    let mut session = client.login(config.user(), &config.password()).await?;
    let mailbox = "INBOX";

    let uid_validity = session.select(mailbox).await?;
    let maildir_repository =
        MaildirRepository::new(config.maildir(), mailbox, config.statedir(), *uid_validity);

    let _ = Syncer::new(session, maildir_repository);

    Ok(())
}
