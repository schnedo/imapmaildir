mod fixtures;

use assertables::*;
use imapmaildir::Client;
use imapmaildir::Syncer;
use rstest::*;

use crate::fixtures::*;

#[rstest]
#[tokio::test]
#[awt]
async fn test(#[future] no_changes_server: MockServer) {
    let config = no_changes_server.config();

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;
    let container = no_changes_server.container();
    assert_ok!(container.stop().await);
    let read_dir = assert_ok!(config.maildir_base_path().join("INBOX/cur").read_dir());
    assert_len_eq_x!(read_dir.collect::<Vec<_>>(), 3);
}
