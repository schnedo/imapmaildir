use futures::Stream;

use super::response_stream::Response;

pub trait ContinuationCommand {
    async fn send(&mut self, command: &str);
}

pub trait SendCommand {
    type Responses<'a>: Stream<Item = Response> + Unpin + ContinuationCommand
    where
        Self: 'a;

    #[must_use]
    fn send(&self, command: String) -> Self::Responses<'_>;
}
