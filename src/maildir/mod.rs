#[expect(clippy::module_inception)]
mod maildir;
mod state;

pub use maildir::Maildir;
pub use state::State;
