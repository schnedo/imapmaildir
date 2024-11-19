mod spec;

use nom::Finish;
pub use spec::Capability;
use spec::{greeting, ResponseTextCode};

// Todo: distinguish ok, preauth and bye
#[derive(Debug)]
pub struct Greeting<'a> {
    capabilities: Option<Vec<Capability<'a>>>,
}
pub fn parse_greeting(input: &str) -> Result<Greeting, ()> {
    if let Ok((_, response)) = greeting(input).finish() {
        let capabilities = if let Some(ResponseTextCode::Capability(capabilities)) = response.code {
            Some(capabilities)
        } else {
            None
        };
        Ok(Greeting { capabilities })
    } else {
        Err(())
    }
}
