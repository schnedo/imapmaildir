mod codec;
#[allow(clippy::module_inception)]
mod connection;
mod response_stream;
mod send_command;
mod tag_generator;

pub use connection::Connection;
pub use send_command::ContinuationCommand;
pub use send_command::SendCommand;
#[cfg(test)]
pub mod mock_connection;
