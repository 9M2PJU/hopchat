//! The lobby: one room everyone on the network is in, with nothing to
//! create and nobody to invite.
//!
//! It is not an MLS group, which would need someone to create it and
//! admit everyone else. A lobby message goes out instead as a hidden
//! one-to-one message, end-to-end encrypted like any other, to each peer
//! with a secure session right now. It carries its own id, the same for
//! every recipient, so replies point at the same message everywhere.
//! Whoever isn't around when it is sent never gets it: like a room, not a
//! mailbox.
//!
//! The engine hands lobby messages to the UI as group messages under the
//! id [`LOBBY`], so everything that works in a group (`@home`, `@ai`,
//! replies) works here too.

use serde::{Deserialize, Serialize};

/// The lobby's conversation id. Peers are `off1…` and MLS group ids are
/// hex, so neither can be this.
pub const LOBBY: &str = "lobby";
/// What the lobby is called in the sidebar.
pub const NAME: &str = "Lobby";
/// What a hidden lobby message starts with; nobody types this.
const MARK: &str = "\u{1}hopchat-lobby\u{1}";

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Wire {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

impl Wire {
    pub fn encode(&self) -> String {
        format!("{MARK}{}", serde_json::to_string(self).unwrap_or_default())
    }

    /// `Some` for any lobby message, even one too garbled to read, so it
    /// is never shown as a one-to-one message.
    pub fn decode(text: &str) -> Option<Option<Wire>> {
        let body = text.strip_prefix(MARK)?;
        Some(serde_json::from_str(body).ok())
    }
}

/// A fresh lobby message id.
pub fn new_id() -> String {
    let mut bytes = [0u8; 12];
    let _ = getrandom::fill(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("lb_{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trips_and_hides_garble() {
        let wire = Wire { id: new_id(), text: "hi all".into(), reply_to: Some("lb_1".into()) };
        assert_eq!(Wire::decode(&wire.encode()), Some(Some(wire)));
        let bare = Wire { id: "lb_2".into(), text: "x".into(), reply_to: None };
        assert!(!bare.encode().contains("reply_to"));
        assert_eq!(Wire::decode(&format!("{MARK}{{nonsense")), Some(None));
        assert_eq!(Wire::decode("an ordinary message"), None);
    }
}
