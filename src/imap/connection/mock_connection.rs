use std::{borrow::Cow, vec::IntoIter};

use futures::{
    stream::{iter, Iter},
    StreamExt,
};
use imap_proto::{RequestId, Response, Status};

use super::{codec::ResponseData, SendCommand};

pub struct MockConnection {
    responses: Box<dyn Iterator<Item = ResponseData>>,
}

impl MockConnection {
    pub fn new(responses: impl IntoIterator<Item = Response<'static>> + 'static) -> Self {
        let responses = Box::new(responses.into_iter().map(ResponseData::new));
        Self { responses }
    }
}

// TODO: Response does not implement copy or clone, so cannot just return input.
// Maybe match on command and generate data on the fly?
// Or use IntoIter?
// Or generate Stream in new to avoid moving here behind mutable self reference?
impl SendCommand for MockConnection {
    type Responses<'a> = Iter<IntoIter<ResponseData>>;

    fn send<'a>(&'a mut self, _command: &'a str) -> Self::Responses<'a> {
        let buf: Vec<_> = self.responses.by_ref().collect();
        iter(buf)
    }
}

#[tokio::test]
async fn should_just_return_input() {
    let responses = [
        Response::Data {
            status: Status::Ok,
            code: None,
            information: None,
        },
        Response::Done {
            tag: RequestId("0000".to_owned()),
            status: Status::Ok,
            code: None,
            information: Some(Cow::Borrowed("information")),
        },
    ];
    let mut mock_connection = MockConnection::new(responses);

    let mut responses = mock_connection.send("whatever");

    let next_response = responses.next().await;
    assert!(next_response.is_some());
    let next_response = next_response.unwrap();
    let next_response = next_response.parsed();
    assert!(matches!(
        next_response,
        Response::Data {
            status: Status::Ok,
            code: None,
            information: None
        }
    ));
    let next_response = responses.next().await;
    assert!(next_response.is_some());
    let next_response = next_response.unwrap();
    let next_response = next_response.parsed();
    assert!(matches!(
        next_response,
        Response::Done {
            tag: RequestId(_),
            status: Status::Ok,
            code: None,
            information: Some(Cow::Borrowed("information")),
        },
    ));
    let next_response = responses.next().await;
    assert!(next_response.is_none());
}
