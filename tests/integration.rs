mod fixtures;

use std::path::PathBuf;
use std::str::FromStr as _;

use assertables::*;
use imapmaildir::Client;
use imapmaildir::Syncer;
use imapmaildir::config::Account;
use imapmaildir::config::Auth;
use imapmaildir::config::PlainAuth;
use rstest::*;
use tempfile::tempdir;

use crate::fixtures::*;

#[rstest]
#[tokio::test]
#[awt]
async fn test(#[future] server: MockServer) {
    let tmp = assert_ok!(tempdir());
    let host = server.hostname().await;
    let port = server.port().await;
    let config = Account::new(
        Auth::Plain(PlainAuth::new(
            "user".to_string(),
            vec!["echo".to_string(), server.password().to_string()],
        )),
        host,
        port,
        Some(assert_ok!(PathBuf::from_str(&format!(
            "{}/tests/mock/certificate.crt",
            env!("CARGO_MANIFEST_DIR")
        )))),
        vec!["INBOX".to_string(), "DRAFT".to_string()],
        tmp.path().to_path_buf(),
        tmp.path().to_path_buf(),
    );

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "DRAFT",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;
    let read_dir = assert_ok!(tmp.path().join("DRAFT").join("cur").read_dir());
    assert_len_eq_x!(read_dir.collect::<Vec<_>>(), 1);
}
