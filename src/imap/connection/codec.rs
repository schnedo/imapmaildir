// Copyright (c) 2017 Dirkjan Ochtman
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

// TODO: Check correct license handling
// original code copied from
// https://github.com/djc/tokio-imap/blob/7cf1ee962ee1981bd3cb5f543a27eedece7e5edc/tokio-imap/src/codec.rs
// just using the crate lead to issues with consuming the response stream. probably due to version
// mismatches of futures crate

use std::io;
use std::mem;

use bytes::{BufMut, Bytes, BytesMut};
use nom::{self, Needed};
use tokio_util::codec::{Decoder, Encoder};

use imap_proto::types::{Request, RequestId, Response};

#[derive(Default)]
pub struct ImapCodec {
    decode_need_message_bytes: usize,
}

impl Decoder for ImapCodec {
    type Item = ResponseData;
    type Error = io::Error;
    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, io::Error> {
        if self.decode_need_message_bytes > buf.len() {
            return Ok(None);
        }
        let (response, rsp_len) = match imap_proto::Response::from_bytes(buf) {
            Ok((remaining, response)) => {
                // This SHOULD be acceptable/safe: BytesMut storage memory is
                // allocated on the heap and should not move. It will not be
                // freed as long as we keep a reference alive, which we do
                // by retaining a reference to the split buffer, below.
                let response =
                    unsafe { mem::transmute::<Response<'_>, Response<'static>>(response) };
                (response, buf.len() - remaining.len())
            }
            Err(nom::Err::Incomplete(Needed::Size(min))) => {
                self.decode_need_message_bytes = min.get();
                return Ok(None);
            }
            Err(nom::Err::Incomplete(_)) => {
                return Ok(None);
            }
            Err(nom::Err::Error(nom::error::Error { code, .. }) |
nom::Err::Failure(nom::error::Error { code, .. })) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("{code:?} during parsing of {buf:?}"),
                ));
            }
        };
        let raw = buf.split_to(rsp_len).freeze();
        self.decode_need_message_bytes = 0;
        Ok(Some(ResponseData { raw, response }))
    }
}

impl<'a> Encoder<&'a Request<'a>> for ImapCodec {
    type Error = io::Error;
    fn encode(&mut self, msg: &Request, dst: &mut BytesMut) -> Result<(), io::Error> {
        if !msg.0.is_empty() {
            dst.put(&*msg.0);
            dst.put_u8(b' ');
        }
        dst.put_slice(&msg.1);
        dst.put_slice(b"\r\n");
        Ok(())
    }
}

#[derive(Debug)]
pub struct ResponseData {
    #[expect(dead_code)] // Contains data that `response` borrows
    raw: Bytes,
    // This reference is really scoped to the lifetime of the `raw`
    // member, but unfortunately Rust does not allow that yet. It
    // is transmuted to `'static` by the `Decoder`, instead, and
    // references returned to callers of `ResponseData` are limited
    // to the lifetime of the `ResponseData` struct.
    //
    // `raw` is never mutated during the lifetime of `ResponseData`,
    // and `Response` does not not implement any specific drop glue.
    response: Response<'static>,
}

impl ResponseData {
    pub fn request_id(&self) -> Option<&RequestId> {
        match self.response {
            Response::Done { ref tag, .. } => Some(tag),
            _ => None,
        }
    }

    #[expect(clippy::needless_lifetimes)]
    pub fn parsed<'a>(&'a self) -> &'a Response<'a> {
        &self.response
    }
}

// my own code
#[cfg(test)]
impl ResponseData {
    pub fn new(response: Response<'static>) -> Self {
        Self {
            raw: Bytes::new(),
            response,
        }
    }
}
