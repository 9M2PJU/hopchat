//! Me: a conversation with nobody else in it, for notes to yourself and
//! for talking to your own devices and model.
//!
//! Nothing written here goes on the network. The engine hands it
//! straight back to the UI as a group message under the id [`ME`], and to
//! home, so `@home` and `@ai` answer here as they do in the lobby: always,
//! and only for the person at this machine. It is kept in history like
//! any other conversation.

/// Its conversation id. Peers are `off1…`, MLS group ids are hex and the
/// lobby is `lobby`, so none of them can be this.
pub const ME: &str = "me";
/// What it is called in the sidebar.
pub const NAME: &str = "Me";

/// A fresh message id.
pub fn new_id() -> String {
    let mut bytes = [0u8; 12];
    let _ = getrandom::fill(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("me_{hex}")
}
