#![expect(dead_code, unused_variables, unused_imports)]
mod config;
mod imap;
mod logging;
mod maildir;
mod nuke;
mod sync;

use anyhow::Result;
use clap::Parser;
use config::Config;
use imap::{Client, Connection};
use maildir::MaildirRepository;
use nuke::nuke;
use sync::Syncer;

#[derive(Parser, Debug)]
#[command(version, about, long_about=None)]
struct Args {
    /// `rm -rf` the configured account (WARNING: includes all mails)
    #[arg(long)]
    nuke: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging::init();

    let config = Config::load_from_file();

    if args.nuke {
        nuke(&config);
        Ok(())
    } else {
        let (connection, _) = Connection::connect_to(config.host(), config.port()).await;
        let client = Client::new(connection);
        let mut session = client.login(config.user(), &config.password()).await?;
        let mailbox = config
            .mailboxes()
            .first()
            .expect("there should be one mailbox set");

        let uid_validity = session.select(mailbox).await?;
        let maildir_repository = MaildirRepository::new(
            config.account(),
            mailbox,
            config.maildir(),
            config.statedir(),
            *uid_validity,
        );

        let mut syncer = Syncer::new(session, maildir_repository);

        syncer.init_a_to_b().await;

        Ok(())
    }
}
