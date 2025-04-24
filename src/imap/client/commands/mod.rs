mod select;

pub use select::select;
pub use select::SelectError;

pub trait Command {
    fn raw(&self) -> String;
}
