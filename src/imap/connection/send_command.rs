use futures::Stream;

use super::response_stream::Response;

pub trait SendCommand {
    fn send<'a>(&'a mut self, command: &'a str) -> impl Stream<Item = Response> + Unpin;
}
