use anyhow::Result;
use client::Client;
use config::Config;

mod client;
mod config;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load_from_file();
    let client = Client::connect(config.host(), config.port()).await;
    let mut session = client.login(config.user(), &config.password()).await?;
    session.select("INBOX").await?;
    session.select("FOOOAA").await?;

    Ok(())
}
