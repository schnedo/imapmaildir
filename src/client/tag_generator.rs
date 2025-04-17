use std::num::Wrapping;

pub(super) struct TagGenerator {
    last_tag: Wrapping<u16>,
}

impl TagGenerator {
    pub fn default() -> Self {
        TagGenerator {
            last_tag: Wrapping(u16::MAX),
        }
    }
    pub fn next(mut self) -> String {
        self.last_tag += 1;
        format!("{:04x}", self.last_tag)
    }
}
