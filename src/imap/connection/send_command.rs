use super::response_stream::ResponseStream;

pub trait SendCommand {
    fn send<'a>(&'a mut self, command: &'a str) -> ResponseStream<'a>;
}
