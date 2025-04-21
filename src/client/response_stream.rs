use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::{Stream, TryStreamExt};
use tokio_util::codec::Decoder;

use super::{codec::ImapCodec, Transport};

pub struct ResponseStream<'a> {
    transport: &'a mut Transport,
    done: bool,
    tag: String,
}

impl<'a> ResponseStream<'a> {
    pub fn new(transport: &'a mut Transport, tag: String) -> Self {
        Self {
            transport,
            done: false,
            tag,
        }
    }
}

impl Stream for ResponseStream<'_> {
    type Item = <ImapCodec as Decoder>::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        let next_poll = self.transport.try_poll_next_unpin(cx);
        match next_poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(data))) => {
                if let Some(tag) = data.request_id() {
                    self.done = true;
                    assert_eq!(
                        tag.0, self.tag,
                        "Response tag did not match request tag. This should never happen and indicates that something is seriously wrong."
                    );
                }
                if data.request_id().is_some() {
                    self.done = true;
                }
                Poll::Ready(Some(data))
            }
            Poll::Ready(Some(Err(_))) => todo!("handle connection errors"),
        }
    }
}
