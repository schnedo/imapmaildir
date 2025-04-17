use std::{borrow::Cow, num::Wrapping};

pub(super) struct TagGenerator {
    last_tag: Wrapping<u16>,
}

impl TagGenerator {
    pub fn default() -> Self {
        TagGenerator {
            last_tag: Wrapping(u16::MAX),
        }
    }
    pub fn next(&mut self) -> Cow<'static, [u8]> {
        self.last_tag += 1;
        Cow::Owned(format!("{:04x}", self.last_tag).into_bytes())
    }
}
