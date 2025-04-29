use std::{
    borrow::Cow,
    mem::transmute,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use futures::{Stream, StreamExt};
use imap_proto::{RequestId, Response, Status};

use super::{codec::ResponseData, ContinuationCommand, SendCommand};

type ListOfResponseList = Box<dyn Iterator<Item = Box<dyn Iterator<Item = ResponseData>>>>;
pub struct MockConnection {
    responses: ListOfResponseList,
}

impl MockConnection {
    pub fn new(
        responses: impl IntoIterator<Item = impl IntoIterator<Item = Response<'static>> + 'static>
            + 'static,
    ) -> Self {
        let responses = Box::new(responses.into_iter().map(|inner| {
            let inner_iter = inner.into_iter().map(ResponseData::new);
            Box::new(inner_iter) as Box<dyn Iterator<Item = ResponseData>>
        }));
        Self { responses }
    }
}

impl SendCommand for MockConnection {
    type Responses<'a> = MockResponses;

    fn send<'a>(&'a mut self, _command: &'a str) -> Self::Responses<'a> {
        let buf: Vec<_> = self.responses.by_ref().collect();
        MockResponses::new(Box::new(buf.into_iter()))
    }
}

pub struct MockResponses {
    responses: ListOfResponseList,
    n_lists_done: usize,
    n_continuation_received: usize,
    current_responses: Box<dyn Iterator<Item = ResponseData>>,
    waker: Option<&'static Waker>,
}

impl MockResponses {
    fn new(mut responses: ListOfResponseList) -> Self {
        Self {
            current_responses: responses.next().expect("responses should have some data"),
            responses,
            n_lists_done: 0,
            n_continuation_received: 0,
            waker: None,
        }
    }
}

impl ContinuationCommand for MockResponses {
    async fn send(&mut self, command: &str) {
        self.n_continuation_received += 1;
        if let Some(waker) = self.waker {
            waker.wake_by_ref();
        }
    }
}

impl Stream for MockResponses {
    type Item = ResponseData;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(response_data) = this.current_responses.next() {
                return Poll::Ready(Some(response_data));
            }
            if let Some(next_responses) = this.responses.next() {
                this.current_responses = next_responses;
                this.n_lists_done += 1;
                if this.n_continuation_received < this.n_lists_done {
                    let waker_ref = unsafe { transmute::<&'_ Waker, &'static Waker>(cx.waker()) };
                    this.waker = Some(waker_ref);
                    return Poll::Pending;
                }
            } else {
                return Poll::Ready(None);
            }
        }
    }
}

#[tokio::test]
async fn should_just_return_input() {
    let responses = [[
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
    ]];
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
