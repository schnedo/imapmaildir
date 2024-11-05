use config::Config;
use imap::connect;
use native_tls::TlsConnector;

mod config;

fn main() {
    let config = Config::load_from_file();

    let tls = TlsConnector::new().expect("native tls should be available");
    let client = connect((config.host(), config.port), config.host(), &tls)
        .expect("client should connect with host and port over native tls");
    let session = client
        .login(config.user(), config.password())
        .expect("session can be established with user and password");
}
