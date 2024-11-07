mod basics;

use basics::{response_done, ResponseDone};

pub fn parse_response(line: &str) -> ResponseDone {
    response_done(line).expect("response should be parseable").1
}
