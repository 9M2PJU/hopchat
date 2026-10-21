//! Conversations that survive a restart, one sealed file per peer.
//!
//! Each file is `nonce || XChaCha20-Poly1305(json)` under a key derived from
//! the profile's store key, so history is as private at rest as the SDK's
//! own stores.

use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::engine::write_private;

/// Older messages are dropped past this, per conversation.
const KEEP: usize = 5_000;
const NONCE: usize = 24;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Status {
    Queued,
    Waiting(String),
    Retrying(u32),
    Sent,
    Delivered { hops: u8, ms: u64 },
    Read,
    Failed(String),
}

impl Status {
    /// Whether nothing more will happen to the message.
    pub fn settled(&self) -> bool {
        matches!(self, Status::Read | Status::Failed(_))
    }

    fn rank(&self) -> u8 {
        match self {
            Status::Queued | Status::Waiting(_) | Status::Retrying(_) => 0,
            Status::Sent => 1,
            Status::Delivered { .. } => 2,
            Status::Read => 3,
            Status::Failed(_) => 4,
        }
    }

    /// Moves forward, never back: a late "sent" must not undo "delivered".
    pub fn advance(&mut self, next: Status) {
        let retry = self.rank() == 0 && next.rank() == 0;
        if next.rank() > self.rank() || retry {
            *self = next;
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub enum Transfer {
    Queued,
    Moving { done: u32, total: u32 },
    Done,
    Failed(String),
    /// Interrupted by a restart; resumable while the source file exists.
    Interrupted,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum Body {
    Text(String),
    File {
        file_id: String,
        name: String,
        size: u64,
        /// The source for a file we sent, the saved copy for one we got.
        path: PathBuf,
        transfer: Transfer,
    },
    /// Something happened that belongs in the conversation but is no one's
    /// message: a lost file, a key change.
    Notice(String),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Quoted {
    pub id: String,
    pub mine: bool,
    pub text: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    /// The SDK's message id, or the transfer id for a file.
    pub id: String,
    pub mine: bool,
    pub at_ms: i64,
    pub body: Body,
    pub status: Status,
    #[serde(default)]
    pub hops: u8,
    #[serde(default)]
    pub reply: Option<Quoted>,
    /// For theirs: whether we told them we read it.
    #[serde(default)]
    pub read: bool,
    /// In a group: who wrote it, when it is not ours.
    #[serde(default)]
    pub sender: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Conversation {
    pub address: String,
    pub nick: String,
    pub entries: Vec<Entry>,
    /// A group conversation: `address` is the group id, `nick` its name.
    #[serde(default)]
    pub group: bool,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(skip)]
    pub dirty: bool,
}

pub struct Store {
    dir: PathBuf,
    cipher: XChaCha20Poly1305,
}

impl Store {
    pub fn open(root: &Path, store_key: &[u8; 32]) -> std::io::Result<Self> {
        let dir = root.join("history");
        std::fs::create_dir_all(&dir)?;
        let key = Sha256::new()
            .chain_update(b"hopchat history v1")
            .chain_update(store_key)
            .finalize();
        let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(std::io::Error::other)?;
        Ok(Self { dir, cipher })
    }

    /// Every conversation that could be read. Unreadable files are skipped,
    /// not fatal: losing one transcript must not lock you out of the rest.
    pub fn load(&self) -> Vec<Conversation> {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<Conversation> = dir
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "sealed"))
            .filter_map(|e| self.read(&e.path()))
            .collect();
        for convo in &mut out {
            for entry in &mut convo.entries {
                if let Body::File { transfer, .. } = &mut entry.body
                    && matches!(transfer, Transfer::Queued | Transfer::Moving { .. })
                {
                    *transfer = if entry.mine {
                        Transfer::Interrupted
                    } else {
                        Transfer::Failed("interrupted".into())
                    };
                }
            }
        }
        out
    }

    fn read(&self, path: &Path) -> Option<Conversation> {
        let bytes = std::fs::read(path).ok()?;
        if bytes.len() < NONCE {
            return None;
        }
        let (nonce, sealed) = bytes.split_at(NONCE);
        let plain = self.cipher.decrypt(XNonce::from_slice(nonce), sealed).ok()?;
        serde_json::from_slice(&plain).ok()
    }

    pub fn save(&self, convo: &Conversation) -> std::io::Result<()> {
        let skip = convo.entries.len().saturating_sub(KEEP);
        let trimmed = Conversation {
            address: convo.address.clone(),
            nick: convo.nick.clone(),
            entries: convo.entries[skip..].to_vec(),
            group: convo.group,
            members: convo.members.clone(),
            dirty: false,
        };
        let plain = serde_json::to_vec(&trimmed).map_err(std::io::Error::other)?;
        let mut nonce = [0u8; NONCE];
        getrandom::fill(&mut nonce).map_err(|e| std::io::Error::other(e.to_string()))?;
        let sealed = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), plain.as_slice())
            .map_err(|_| std::io::Error::other("seal failed"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&sealed);
        // Write beside, then rename: a crash mid-write keeps the old copy.
        let path = self.path_for(&convo.address);
        let tmp = path.with_extension("tmp");
        write_private(&tmp, &out)?;
        std::fs::rename(tmp, path)
    }

    pub fn forget(&self, address: &str) {
        let _ = std::fs::remove_file(self.path_for(address));
    }

    fn path_for(&self, address: &str) -> PathBuf {
        let digest = Sha256::digest(address.as_bytes());
        let name: String = digest[..12].iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{name}.sealed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_sealed() {
        let root = std::env::temp_dir().join(format!("hopchat-test-{}", std::process::id()));
        let store = Store::open(&root, &[7u8; 32]).unwrap();
        let convo = Conversation {
            address: "off1peer".into(),
            nick: "bob".into(),
            entries: vec![Entry {
                id: "m1".into(),
                mine: true,
                at_ms: 1,
                body: Body::Text("secret words".into()),
                status: Status::Sent,
                hops: 0,
                reply: None,
                read: false,
                sender: None,
            }],
            group: false,
            members: Vec::new(),
            dirty: true,
        };
        store.save(&convo).unwrap();
        let raw = std::fs::read(store.path_for("off1peer")).unwrap();
        assert!(!raw.windows(6).any(|w| w == b"secret"));
        let loaded = store.load();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].nick, "bob");

        let other = Store::open(&root, &[8u8; 32]).unwrap();
        assert!(other.load().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn status_only_moves_forward() {
        let mut s = Status::Delivered { hops: 1, ms: 3 };
        s.advance(Status::Sent);
        assert!(matches!(s, Status::Delivered { .. }));
        s.advance(Status::Read);
        assert_eq!(s, Status::Read);
        let mut q = Status::Queued;
        q.advance(Status::Retrying(2));
        assert_eq!(q, Status::Retrying(2));
    }
}
