use imapmaildir::{Client, Tls, config::Account};

use crate::cli::Error;

pub fn list(config: &Account) -> Result<(), Error> {
    let tls = Tls::new(config.connection()).map_err(Error::Tls)?;
    let rt = compio::runtime::Runtime::new().map_err(Error::Runtime)?;

    rt.block_on(async {
        let mut client = Client::login(config.connection(), &tls, config.auth())
            .await
            .map_err(Error::Login)?;
        for mailbox in client.list().await.map_err(Error::List)? {
            println!("{mailbox}");
        }

        Ok(())
    })
}
