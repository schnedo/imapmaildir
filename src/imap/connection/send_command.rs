use futures::Stream;

use super::response_stream::Response;

pub trait SendCommand {
    type Responses<'a>: Stream<Item = Response> + Unpin
    where
        Self: 'a;

    fn send<'a>(&'a mut self, command: &'a str) -> Self::Responses<'a>;
}
