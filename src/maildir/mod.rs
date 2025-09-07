#[expect(clippy::module_inception)]
mod maildir;
mod maildir_repository;

pub use maildir::Maildir;
pub use maildir_repository::LocalMailMetadata;
pub use maildir_repository::MaildirRepository;
