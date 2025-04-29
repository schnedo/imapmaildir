mod config;
mod imap;
mod logging;

use anyhow::Result;
use config::Config;
use imap::{Client, Connection};

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();

    let config = Config::load_from_file();
    let (connection, _) = Connection::connect_to(config.host(), *config.port()).await;
    let client = Client::new(connection);
    let mut session = client.login(config.user(), &config.password()).await?;
    session.select("INBOX").await?;
    session.idle().await;

    Ok(())
}
