mod codec;
mod connection;
mod tag_generator;

#[cfg(feature = "bench")]
pub(crate) use codec::ImapCodec;
pub use codec::ResponseData;
pub use connection::Connection;
