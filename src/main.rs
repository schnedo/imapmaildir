mod config;
mod imap;
mod logging;
mod maildir;
mod sync;

use anyhow::Result;
use config::Config;
use imap::{Client, Connection};
use sync::Syncer;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    logging::init();

    let config = Config::load_from_file();

    let (connection, _) = Connection::connect_to(config.host(), *config.port()).await;
    let client = Client::new(connection);
    let session = client.login(config.user(), &config.password()).await?;
    let mailbox = "INBOX";

    let mut syncer = Syncer::connect(session, config.maildir(), config.statedir(), mailbox).await;

    let join_handles = syncer.fetch_6106().await;
    for handle in join_handles {
        handle
            .await
            .expect("mail store task should not panic")
            .expect("writing mail to disc should succeed");
    }
    // session.idle().await;

    Ok(())
}
