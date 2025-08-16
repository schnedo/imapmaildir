use futures::Stream;

use super::response_stream::Response;

pub trait ContinuationCommand {
    async fn send(&mut self, command: &str);
}

pub trait SendCommand {
    type Responses<'a>: Stream<Item = Response> + Unpin + ContinuationCommand
    where
        Self: 'a;

    fn send<'a>(&'a mut self, command: String) -> Self::Responses<'a>;
}
