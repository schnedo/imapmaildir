use futures::StreamExt;

use crate::imap::connection::SendCommand;

pub async fn idle(connection: &mut impl SendCommand) {
    let command = "IDLE";
    let mut responses = connection.send(&command);
    while let Some(response) = responses.next().await {
        dbg!(response);
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use imap_proto::{AttributeValue, MailboxDatum, Response};

    #[tokio::test]
    async fn foo() {
        let foo = [
            Response::Continue {
                code: None,
                information: Some(Cow::Borrowed("idling")),
            },
            Response::MailboxData(MailboxDatum::Exists(6081)),
            Response::Fetch(
                6081,
                vec![AttributeValue::Flags(vec![Cow::Borrowed("Junk")])],
            ),
        ];
    }
}
