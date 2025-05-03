#[expect(clippy::module_inception)]
mod maildir;

pub use maildir::Maildir;
