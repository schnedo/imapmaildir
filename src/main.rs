use client::Client;
use config::Config;

mod client;
mod config;

#[tokio::main]
async fn main() {
    let config = Config::load_from_file();
    let client = Client::connect(config.host(), config.port()).await;
    let _ = client.login(config.user(), &config.password()).await;
}
