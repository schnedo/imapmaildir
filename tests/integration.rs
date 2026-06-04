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
    let client_mails = assert_ok!(config.maildir_base_path().join("INBOX/cur").read_dir());
    assert_len_eq_x!(client_mails.collect::<Vec<_>>(), 3);
    let mailbox = mail_setup.server_mail().mailbox("INBOX");
    let server_mails = mailbox.mails();
    assert_len_eq_x!(server_mails.collect::<Vec<_>>(), 3);
}
