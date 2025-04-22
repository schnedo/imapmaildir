mod config;
mod imap;
mod logging;

use anyhow::Result;
use config::Config;
use imap::Client;

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();

    let config = Config::load_from_file();
    let client = Client::connect(config.host(), config.port()).await;
    let mut session = client.login(config.user(), &config.password()).await?;
    session.select("INBOX").await?;
    session.select("FOOOAA").await?;

    Ok(())
}
