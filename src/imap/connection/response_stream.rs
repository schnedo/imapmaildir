use std::{
    borrow::Cow,
    pin::Pin,
    task::{ready, Context, Poll},
};

use futures::{SinkExt as _, Stream, TryStreamExt as _};
use imap_proto::Request;
use tokio_util::codec::Decoder;

use super::{
    codec::ImapCodec, connection::ImapStream, send_command::ContinuationCommand,
    tag_generator::TagGenerator,
};

enum ResponseStreamState {
    Start,
    Sending,
    Receiving,
    Done,
}

pub struct ResponseStream<'a> {
    imap_stream: &'a mut ImapStream,
    state: ResponseStreamState,
    tag_generator: &'a mut TagGenerator,
    tag: String,
    command: String,
}

impl<'a> ResponseStream<'a> {
    pub fn new(
        imap_stream: &'a mut ImapStream,
        tag_generator: &'a mut TagGenerator,
        command: String,
    ) -> Self {
        Self {
            imap_stream,
            state: ResponseStreamState::Start,
            tag_generator,
            tag: String::with_capacity(0),
            command,
        }
    }

    fn start_sending(&mut self) {
        let tag = self.tag_generator.next();
        let request = Request(
            Cow::Borrowed(tag.as_bytes()),
            Cow::Borrowed(self.command.as_bytes()),
        );
        self.imap_stream
            .start_send_unpin(&request)
            .expect("imap sink should be able to receive data");
        self.tag = tag;
        self.state = ResponseStreamState::Sending;
    }
}

pub type Response = <ImapCodec as Decoder>::Item;

impl Stream for ResponseStream<'_> {
    type Item = Response;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.state {
                ResponseStreamState::Start => {
                    ready!(self.imap_stream.poll_ready_unpin(cx))
                        .expect("imap sink should be ready for receiving data");
                    self.start_sending();
                }
                ResponseStreamState::Sending => {
                    ready!(self.imap_stream.poll_flush_unpin(cx))
                        .expect("imap sink should be able to flush data");
                    self.state = ResponseStreamState::Receiving;
                }
                ResponseStreamState::Receiving => {
                    match ready!(self.imap_stream.try_poll_next_unpin(cx)) {
                        None => return Poll::Ready(None),
                        Some(Ok(data)) => {
                            if let Some(tag) = data.request_id() {
                                self.state = ResponseStreamState::Done;
                                assert_eq!(
                                    tag.0,
                                    self.tag,
                                    "Response tag did not match request tag. This should never happen and indicates that something is seriously wrong.",
                                );
                            }
                            return Poll::Ready(Some(data));
                        }
                        Some(Err(e)) => panic!("{}", e),
                    }
                }
                ResponseStreamState::Done => return Poll::Ready(None),
            }
        }
    }
}

impl ContinuationCommand for ResponseStream<'_> {
    async fn send(&mut self, command: &str) {
        let request = Request(Cow::Borrowed(&[]), Cow::Borrowed(command.as_bytes()));
        self.imap_stream
            .send(&request)
            .await
            .expect("sending of continuation data should succeed");
    }
}
