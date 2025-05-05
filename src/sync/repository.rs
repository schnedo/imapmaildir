use crate::imap::UidValidity;

pub trait Repository {
    fn validity(&self) -> &UidValidity;
}
