mod fixtures;

use assertables::*;
use imapmaildir::Client;
use imapmaildir::Syncer;
use rstest::*;

use crate::fixtures::*;

#[rstest]
#[tokio::test]
#[awt]
async fn test(#[future] mail_setup: MailSetup) {
    let config = mail_setup.config();

    let client = Client::login(config.connection(), config.auth()).await;
    Syncer::sync(
        "INBOX",
        config.maildir_base_path(),
        config.state_dir(),
        client,
    )
    .await;
    let container = mail_setup.container();
    assert_ok!(container.stop().await);
    let read_dir = assert_ok!(config.maildir_base_path().join("INBOX/cur").read_dir());
    assert_len_eq_x!(read_dir.collect::<Vec<_>>(), 3);
}
