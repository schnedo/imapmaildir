use super::{response_stream::Response, SendCommand};

pub struct MockConnection<T: Iterator<Item = Response>> {
    responses: T,
}

impl<T: Iterator<Item = Response>> MockConnection<T> {
    pub fn new() -> Self {}
}

impl<T: Iterator<Item = Response>> SendCommand for MockConnection<T> {
    fn send<'a>(&'a mut self, command: &'a str) -> super::response_stream::ResponseStream<'a> {
        todo!()
    }
}

#[test]
fn should_just_return_input() {
    assert!(false);
}
