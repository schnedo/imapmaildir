use client::Client;
use config::Config;

mod client;
mod config;

#[tokio::main]
async fn main() {
    let config = Config::load_from_file();
    Client::new(&config).await;
}
