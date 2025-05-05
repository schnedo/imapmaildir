#[expect(clippy::module_inception)]
mod maildir;
mod maildir_repository;
mod state;

pub use maildir::Maildir;
pub use state::State;
