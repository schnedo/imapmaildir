use std::{cell::Cell, num::Wrapping};

#[derive(Debug)]
pub struct TagGenerator {
    last_tag: Cell<Wrapping<u16>>,
}

impl TagGenerator {
    pub fn next(&self) -> String {
        self.last_tag.update(|x| x + Wrapping(1u16));
        format!("{:04x}", self.last_tag.get())
    }
}

impl Default for TagGenerator {
    fn default() -> Self {
        Self {
            last_tag: Cell::new(Wrapping(u16::MAX)),
        }
    }
}
