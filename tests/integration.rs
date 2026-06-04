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
    let client_mail = mail_setup.client_mail();
    let client_mailbox = client_mail.mailbox("INBOX");
    let client_mails = client_mailbox.mails();
    assert_len_eq_x!(client_mails.collect::<Vec<_>>(), 3);
    let server_mailbox = mail_setup.server_mail().mailbox("INBOX");
    let server_mails = server_mailbox.mails();
    assert_len_eq_x!(server_mails.collect::<Vec<_>>(), 3);
}
