mod codec;
#[expect(clippy::module_inception)]
mod connection;
mod response_stream;
mod send_command;
mod tag_generator;

pub use codec::ImapCodec;
pub use codec::ResponseData;
pub use connection::Connection;
pub use send_command::ContinuationCommand;
pub use send_command::SendCommand;
pub use tag_generator::TagGenerator;
#[cfg(test)]
pub mod mock_connection;
