use std::str;

use futures::StreamExt;
use imap_proto::{AttributeValue, Response};
use log::{debug, trace, warn};

use crate::imap::connection::{ResponseData, SendCommand};

pub async fn fetch(connection: &mut impl SendCommand, sequence_set: &str) -> Vec<RemoteMail> {
    // TODO: use imap_proto::Attribute enum?
    // TODO: use imap_proto::Attribute enum?
    let command = format!("FETCH {sequence_set} (RFC822)");
    debug!("{command}");
    let mut responses = connection.send(&command);
    // TODO: infer capacity from sequence_set
    let mut mails = Vec::with_capacity(1);
    while let Some(response) = responses.next().await {
        if let Response::Fetch(_, attibutes) = response.parsed() {
            debug_assert_eq!(attibutes.len(), 1); // same as attibutes list in command
            mails.push(RemoteMail { response });
        } else {
            warn!("ignoring unknown response to FETCH");
            trace!("{:?}", response.parsed());
        }
    }
    mails
}

pub struct RemoteMail {
    response: ResponseData, // need to hold reference to response buffer
}

impl RemoteMail {
    pub fn content(&self) -> &[u8] {
        if let Response::Fetch(_, attributes) = self.response.parsed() {
            for attribute in attributes {
                if let AttributeValue::Rfc822(Some(data)) = attribute {
                    return data.as_ref();
                }
            }
            panic!("no mail content found. probably wrong FETCH command attributes");
        }
        panic!("response should be FETCH response. check construnction code");
    }

    pub fn uid(&self) -> u32 {
        8
    }
}

// response.parsed() = Fetch(6090,[Flags(["\\Seen",],),Envelope(Envelope {date: Some([83,117,110,44,32,50,55,32,65,112,114,32,50,48,50,53,32,49,57,58,50,52,58,52,53,32,43,48,50,48,48,],),subject: Some([115,117,98,106,101,99,116,],),from: Some([Address {name: Some([70 105 114 115 116 110 97 109 101 32 76 97 115 116 110 97 109 101],),adl: None,mailbox: Some([109 97 105 108 98 111 120],),host: Some([104 111 115 116 46 116 100 108],),},],),sender: Some([Address {name: Some([70 105 114 115 116 110 97 109 101 32 76 97 115 116 110 97 109 101],),adl: None,mailbox: Some([109 97 105 108 98 111 120],),host: Some([104 111 115 116 46 116 100 108],),},],),reply_to: Some([Address {name: Some([70 105 114 115 116 110 97 109 101 32 76 97 115 116 110 97 109 101],),adl: None,mailbox: Some([109 97 105 108 98 111 120],),host: Some([104 111 115 116 46 116 100 108],),},],),to: Some([Address {name: Some([70 105 114 115 116 110 97 109 101 32 76 97 115 116 110 97 109 101],),adl: None,mailbox: Some([109 97 105 108 98 111 120],),host: Some([104 111 115 116 46 116 100 108],),},],),cc: None,bcc: None,in_reply_to: None,message_id: Some([60 77 83 71 73 68 64 104 111 115 116 46 116 100 108 62],),},),Rfc822(Some([39 82 101 116 117 114 110 45 80 97 116 104 58 32 60 109 97 105 108 98 111 120 64 104 111 115 116 46 116 100 108 62 10 10 68 101 108 105 118 101 114 101 100 45 84 111 58 32 109 97 105 108 98 111 120 64 104 111 115 116 46 116 100 108 10 10 82 101 99 101 105 118 101 100 58 32 102 114 111 109 32 108 111 99 97 108 104 111 115 116 32 40 117 110 107 110 111 119 110 32 91 73 80 118 54 58 50 97 48 50 58 56 48 55 49 58 50 98 56 52 58 56 51 99 48 58 56 98 53 54 58 51 97 55 48 58 100 102 102 58 57 51 48 48 93 41 10 10 9 40 65 117 116 104 101 110 116 105 99 97 116 101 100 32 115 101 110 100 101 114 58 32 109 97 105 108 98 111 120 64 104 111 115 116 46 116 100 108 41 10 10 9 98 121 32 109 120 46 104 111 115 116 46 116 100 108 32 40 115 101 114 118 101 114 105 109 112 108 41 32 119 105 116 104 32 69 83 77 84 80 83 65 32 105 100 32 70 70 70 70 70 70 70 70 70 70 70 10 10 9 102 111 114 32 60 109 97 105 108 98 111 120 64 104 111 115 116 46 116 100 108 62 59 32 83 117 110 44 32 50 55 32 65 112 114 32 50 48 50 53 32 49 57 58 50 52 58 52 53 32 43 48 50 48 48 32 40 67 69 83 84 41 10 10 77 105 109 101 45 86 101 114 115 105 111 110 58 32 49 46 48 10 10 67 111 110 116 101 110 116 45 84 114 97 110 115 102 101 114 45 69 110 99 111 100 105 110 103 58 32 113 117 111 116 101 100 45 112 114 105 110 116 97 98 108 101 10 10 67 111 110 116 101 110 116 45 84 121 112 101 58 32 116 101 120 116 47 112 108 97 105 110 59 32 99 104 97 114 115 101 116 61 85 84 70 45 56 10 10 68 97 116 101 58 32 83 117 110 44 32 50 55 32 65 112 114 32 50 48 50 53 32 49 57 58 50 52 58 52 53 32 43 48 50 48 48 10 10 77 101 115 115 97 103 101 45 73 100 58 32 60 109 101 115 115 97 103 101 105 100 64 104 111 115 116 46 116 100 108 62 10 10 83 117 98 106 101 99 116 58 32 115 117 98 106 101 99 116 10 10 70 114 111 109 58 32 34 70 105 114 115 116 110 97 109 101 32 76 97 115 116 110 97 109 101 34 32 60 109 97 105 108 98 111 120 64 104 111 115 116 46 116 108 100 62 10 10 84 111 58 32 60 109 97 105 108 98 111 120 64 104 111 115 116 46 116 100 108 62 10 10 88 45 77 97 105 108 101 114 58 32 99 108 105 101 110 116 32 118 101 114 115 105 111 110 10 10 10 10 98 111 100 121 10 10 39 10 226 128 139],),),(Rfc822Text(Some([98,111,100,121,13,10,],),),),],)
// response.parsed() = Done {tag: RequestId("0002",),status: Ok,code: None,information: Some("Fetch completed (0.002 + 0.000 + 0.001 secs).",),}
