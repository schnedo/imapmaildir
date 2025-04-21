use derive_builder::Builder;

#[derive(Builder, Debug)]
pub(super) struct Mailbox {
    name: String,
    #[builder(default)]
    readonly: bool,
    flags: Vec<String>,
    exists: u32,
    recent: u32,
    #[builder(setter(strip_option), default)]
    unseen: Option<u32>,
    #[builder(default)]
    permanent_flags: Vec<String>,
    #[builder(setter(strip_option), default)]
    uid: Option<Uid>,
}

#[derive(Builder, Debug, Clone)]
pub(super) struct Uid {
    validity: u32,
    next: u32,
}
