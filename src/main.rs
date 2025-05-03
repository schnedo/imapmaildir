mod config;
mod imap;
mod logging;
mod maildir;

use anyhow::Result;
use config::Config;
use imap::{Client, Connection, SequenceSet};
use log::debug;
use maildir::{Maildir, State};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    logging::init();

    let config = Config::load_from_file();

    let (connection, _) = Connection::connect_to(config.host(), *config.port()).await;
    let client = Client::new(connection);
    let mut session = client.login(config.user(), &config.password()).await?;
    let mailbox = "INBOX";
    let uid_validity = session.select(mailbox).await?;
    let mails = session.fetch(&SequenceSet::single(6106)).await;

    let maildir = config.maildir().join(mailbox);
    let state_dir = config.statedir();

    let state = if let Ok(state) = State::load(state_dir, mailbox) {
        debug!("existing state file for {mailbox} found");
        if *state.uid_validity() != uid_validity {
            todo!("handle uid_validity change");
        }
        state
    } else {
        debug!("creating new state file for {mailbox}");
        State::create_new(state_dir, mailbox, uid_validity)
    };
    let mailbox = Maildir::new(maildir, state);

    let join_handles = mails.into_iter().map(|mail| mailbox.store_new(mail));
    for handle in join_handles {
        handle
            .await
            .expect("mail store task should not panic")
            .expect("writing mail to disc should succeed");
    }
    // session.idle().await;

    Ok(())
}
