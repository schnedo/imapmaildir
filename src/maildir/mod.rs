mod local_mail;
mod maildir;
mod maildir_repository;
mod state;

pub use local_mail::LocalMail;
pub use local_mail::LocalMailMetadata;
pub use maildir::Maildir;
pub use maildir_repository::LocalChanges;
pub use maildir_repository::MaildirRepository;
