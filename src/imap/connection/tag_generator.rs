use std::num::Wrapping;

pub struct TagGenerator {
    last_tag: Wrapping<u16>,
}

impl TagGenerator {
    pub fn next(&mut self) -> String {
        self.last_tag += 1;
        format!("{:04x}", self.last_tag)
    }
}

impl Default for TagGenerator {
    fn default() -> Self {
        Self {
            last_tag: Wrapping(u16::MAX),
        }
    }
}
