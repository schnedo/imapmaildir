mod client;
mod config;
mod connected_to_journal;

use std::io::Write as _;

use anyhow::Result;
use client::Client;
use config::Config;
use connected_to_journal::connected_to_journal;
use env_logger::Env;

#[tokio::main]
async fn main() -> Result<()> {
    if connected_to_journal() {
        dbg!("journal");
        env_logger::Builder::from_env(Env::default().default_filter_or("debug"))
            .format(|buf, record| {
                writeln!(
                    buf,
                    "<{}>{}: {}",
                    match record.level() {
                        log::Level::Error => 3,
                        log::Level::Warn => 4,
                        log::Level::Info => 6,
                        log::Level::Debug => 7,
                        log::Level::Trace => 7,
                    },
                    record.target(),
                    record.args()
                )
            })
            .init()
    } else {
        env_logger::Builder::from_env(Env::default().default_filter_or("trace")).init();
    }

    let config = Config::load_from_file();
    let client = Client::connect(config.host(), config.port()).await;
    let mut session = client.login(config.user(), &config.password()).await?;
    session.select("INBOX").await?;
    session.select("FOOOAA").await?;

    Ok(())
}
