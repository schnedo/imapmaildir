use std::{
    fmt::{Display, Formatter, Result},
    mem::transmute,
};

use futures::StreamExt;
use imap_proto::{AttributeValue, Response, Status};
use log::{debug, trace, warn};

use crate::imap::connection::{ResponseData, SendCommand};

// simplified form of real imap sequence set.
// this struct currently only takes a single number or a range instead of full blown vector of
// numbers/ranges
#[derive(Debug)]
pub struct SequenceSet {
    from: u32,
    to: Option<u32>,
}

impl SequenceSet {
    pub fn single(from: u32) -> Self {
        Self { from, to: None }
    }
    pub fn range(from: u32, to: u32) -> Self {
        Self { from, to: Some(to) }
    }
}

impl Display for SequenceSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        if let Some(to) = self.to {
            write!(f, "{}:{}", self.from, to)
        } else {
            write!(f, "{}", self.from)
        }
    }
}

pub async fn fetch(
    connection: &mut impl SendCommand,
    sequence_set: &SequenceSet,
) -> Vec<RemoteMail> {
    let command = format!("FETCH {sequence_set} (UID, FLAGS, RFC822)");
    debug!("{command}");
    let mut responses = connection.send(&command);
    // TODO: infer capacity from sequence_set
    let mut mails = Vec::with_capacity(1);
    while let Some(response) = responses.next().await {
        match response.parsed() {
            Response::Fetch(_, attributes) => {
                if let [AttributeValue::Uid(uid), AttributeValue::Flags(flags), AttributeValue::Rfc822(Some(content))] =
                    attributes.as_slice()
                {
                    trace!("{flags:?}");
                    let mut seen = None;
                    let mut answered = None;
                    let mut flagged = None;
                    let mut deleted = None;
                    let mut draft = None;
                    let mut recent = None;
                    for flag in flags {
                        match flag.as_ref() {
                            "\\Seen" => seen = Some(()),
                            "\\Answered" => flagged = Some(()),
                            "\\Flagged" => answered = Some(()),
                            "\\Deleted" => deleted = Some(()),
                            "\\Draft" => draft = Some(()),
                            "\\Recent" => recent = Some(()),
                            _ => debug!("ignoring unhandled flag {flag}"),
                        }
                    }
                    mails.push(RemoteMail {
                        uid: *uid,
                        seen: seen.is_some(),
                        answered: answered.is_some(),
                        flagged: flagged.is_some(),
                        deleted: deleted.is_some(),
                        draft: draft.is_some(),
                        recent: recent.is_some(),
                        content: unsafe { transmute::<&[u8], &[u8]>(content.as_ref()) },
                        response,
                    });
                } else {
                    panic!("wrong format of FETCH response. check order of attributes in command");
                }
            }
            Response::Done {
                status: Status::Ok, ..
            } => {}
            Response::Done { information, .. } => {
                if let Some(information) = information {
                    panic!("{information}");
                } else {
                    panic!("bad FETCH");
                }
            }
            _ => {
                warn!("ignoring unknown response to FETCH");
                trace!("{:?}", response.parsed());
            }
        }
    }
    mails
}

#[expect(clippy::struct_excessive_bools)]
pub struct RemoteMail {
    #[expect(dead_code)] // need to hold reference to response buffer for other fields
    response: ResponseData,
    uid: u32,
    seen: bool,
    answered: bool,
    flagged: bool,
    deleted: bool,
    draft: bool,
    recent: bool,
    content: &'static [u8],
}

impl RemoteMail {
    pub fn content(&self) -> &[u8] {
        self.content
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn seen(&self) -> bool {
        self.seen
    }
    pub fn answered(&self) -> bool {
        self.answered
    }
    pub fn flagged(&self) -> bool {
        self.flagged
    }
    pub fn deleted(&self) -> bool {
        self.deleted
    }
    pub fn draft(&self) -> bool {
        self.draft
    }
    pub fn recent(&self) -> bool {
        self.recent
    }
}

// response.parsed() = Fetch(6090,[Flags(["\\Seen",],),Envelope(Envelope {date: Some([83,117,110,44,32,50,55,32,65,112,114,32,50,48,50,53,32,49,57,58,50,52,58,52,53,32,43,48,50,48,48,],),subject: Some([115,117,98,106,101,99,116,],),from: Some([Address {name: Some([70 105 114 115 116 110 97 109 101 32 76 97 115 116 110 97 109 101],),adl: None,mailbox: Some([109 97 105 108 98 111 120],),host: Some([104 111 115 116 46 116 100 108],),},],),sender: Some([Address {name: Some([70 105 114 115 116 110 97 109 101 32 76 97 115 116 110 97 109 101],),adl: None,mailbox: Some([109 97 105 108 98 111 120],),host: Some([104 111 115 116 46 116 100 108],),},],),reply_to: Some([Address {name: Some([70 105 114 115 116 110 97 109 101 32 76 97 115 116 110 97 109 101],),adl: None,mailbox: Some([109 97 105 108 98 111 120],),host: Some([104 111 115 116 46 116 100 108],),},],),to: Some([Address {name: Some([70 105 114 115 116 110 97 109 101 32 76 97 115 116 110 97 109 101],),adl: None,mailbox: Some([109 97 105 108 98 111 120],),host: Some([104 111 115 116 46 116 100 108],),},],),cc: None,bcc: None,in_reply_to: None,message_id: Some([60 77 83 71 73 68 64 104 111 115 116 46 116 100 108 62],),},),Rfc822(Some([39 82 101 116 117 114 110 45 80 97 116 104 58 32 60 109 97 105 108 98 111 120 64 104 111 115 116 46 116 100 108 62 10 10 68 101 108 105 118 101 114 101 100 45 84 111 58 32 109 97 105 108 98 111 120 64 104 111 115 116 46 116 100 108 10 10 82 101 99 101 105 118 101 100 58 32 102 114 111 109 32 108 111 99 97 108 104 111 115 116 32 40 117 110 107 110 111 119 110 32 91 73 80 118 54 58 50 97 48 50 58 56 48 55 49 58 50 98 56 52 58 56 51 99 48 58 56 98 53 54 58 51 97 55 48 58 100 102 102 58 57 51 48 48 93 41 10 10 9 40 65 117 116 104 101 110 116 105 99 97 116 101 100 32 115 101 110 100 101 114 58 32 109 97 105 108 98 111 120 64 104 111 115 116 46 116 100 108 41 10 10 9 98 121 32 109 120 46 104 111 115 116 46 116 100 108 32 40 115 101 114 118 101 114 105 109 112 108 41 32 119 105 116 104 32 69 83 77 84 80 83 65 32 105 100 32 70 70 70 70 70 70 70 70 70 70 70 10 10 9 102 111 114 32 60 109 97 105 108 98 111 120 64 104 111 115 116 46 116 100 108 62 59 32 83 117 110 44 32 50 55 32 65 112 114 32 50 48 50 53 32 49 57 58 50 52 58 52 53 32 43 48 50 48 48 32 40 67 69 83 84 41 10 10 77 105 109 101 45 86 101 114 115 105 111 110 58 32 49 46 48 10 10 67 111 110 116 101 110 116 45 84 114 97 110 115 102 101 114 45 69 110 99 111 100 105 110 103 58 32 113 117 111 116 101 100 45 112 114 105 110 116 97 98 108 101 10 10 67 111 110 116 101 110 116 45 84 121 112 101 58 32 116 101 120 116 47 112 108 97 105 110 59 32 99 104 97 114 115 101 116 61 85 84 70 45 56 10 10 68 97 116 101 58 32 83 117 110 44 32 50 55 32 65 112 114 32 50 48 50 53 32 49 57 58 50 52 58 52 53 32 43 48 50 48 48 10 10 77 101 115 115 97 103 101 45 73 100 58 32 60 109 101 115 115 97 103 101 105 100 64 104 111 115 116 46 116 100 108 62 10 10 83 117 98 106 101 99 116 58 32 115 117 98 106 101 99 116 10 10 70 114 111 109 58 32 34 70 105 114 115 116 110 97 109 101 32 76 97 115 116 110 97 109 101 34 32 60 109 97 105 108 98 111 120 64 104 111 115 116 46 116 108 100 62 10 10 84 111 58 32 60 109 97 105 108 98 111 120 64 104 111 115 116 46 116 100 108 62 10 10 88 45 77 97 105 108 101 114 58 32 99 108 105 101 110 116 32 118 101 114 115 105 111 110 10 10 10 10 98 111 100 121 10 10 39 10 226 128 139],),),(Rfc822Text(Some([98,111,100,121,13,10,],),),),],)
// response.parsed() = Done {tag: RequestId("0002",),status: Ok,code: None,information: Some("Fetch completed (0.002 + 0.000 + 0.001 secs).",),}
