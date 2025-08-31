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
use imap::{Authenticator, Connection, ImapRepository};
use maildir::MaildirRepository;
use nuke::nuke;
use sync::Repository;
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
        let mailbox = config
            .mailboxes()
            .first()
            .expect("there should be one mailbox set");

        let imap_repository = ImapRepository::try_connect::<Connection>(
            config.host(),
            config.port(),
            config.user(),
            &config.password(),
            mailbox,
        )
        .await
        .expect("connecting imap repository should not fail");
        let uid_validity = imap_repository.validity();
        let maildir_repository = MaildirRepository::new(
            config.account(),
            mailbox,
            config.maildir(),
            config.statedir(),
            uid_validity,
        );

        let mut syncer = Syncer::new(imap_repository, maildir_repository);

        syncer.init_remote_to_local().await;

        Ok(())
    }
}
