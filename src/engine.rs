//! Owns the `OfflineProtocol` instance on one thread and pumps it: network
//! frames in, UI commands in, protocol events and chat messages out.

use std::collections::{HashMap, VecDeque};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use offline_protocol::{
    account_storage_namespace, Error as ProtocolError, EstablishmentState, Event, FileStorePair,
    MediaSendOptions, OfflineProtocol, ProtocolConfig, SendMessageOptions, StaticStoreKey,
};
use offline_protocol_core::{ContentType, ReplyContext, UserId};
use offline_protocol_transport::{
    Transport, TransportStatus, TransportType, WifiDirectTransport,
};

use crate::net::{self, LocalIdentity, NetEvent};

pub const APP_ID: &str = "dev.hopchat";
const PROFILE: &str = "default";
const TICK: Duration = Duration::from_millis(30);
/// The SDK's receive-side cap; larger files would be dropped by the peer.
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// A quoted reply carries at most this much of the original text.
const QUOTE_CHARS: usize = 120;

/// The message a new one answers.
#[derive(Clone)]
pub struct Quote {
    pub id: String,
    pub sender: String,
    pub text: String,
}

/// What the UI asks the engine to do.
pub enum Command {
    Send {
        to: String,
        text: String,
        reply: Option<Quote>,
    },
    /// Queue `path` for `to` under `file_id`. Pass the id of an interrupted
    /// transfer to resume it; otherwise mint one with [`new_file_id`].
    SendFile {
        to: String,
        path: PathBuf,
        file_id: String,
    },
    Typing {
        to: String,
        on: bool,
    },
    Read {
        to: String,
        ids: Vec<String>,
    },
}

/// What the engine tells the UI.
pub enum Update {
    PeerSeen { address: String, nick: String },
    PeerUp { address: String },
    PeerDown { address: String },
    /// An MLS session with `address` is usable (new or restored).
    SessionReady { address: String },
    /// A send was accepted locally under `id`.
    Queued {
        to: String,
        id: String,
        text: String,
        reply: Option<Quote>,
    },
    Incoming {
        from: String,
        id: String,
        text: String,
        hops: u8,
        reply: Option<Quote>,
    },
    /// A file is waiting for a session or a free transfer slot.
    FileQueued {
        file_id: String,
        name: String,
        size: u64,
    },
    /// Chunks of a transfer, either direction, have been acknowledged.
    FileProgress {
        file_id: String,
        done: u32,
        total: u32,
        /// Name and size, known for inbound transfers once chunk 0 lands.
        inbound: Option<(String, u64)>,
    },
    /// Every chunk of an outbound file was acknowledged.
    FileSent { file_id: String },
    FileFailed { file_id: String, reason: String },
    /// A whole file arrived and was saved to `path`.
    FileReceived {
        from: String,
        file_id: String,
        name: String,
        size: u64,
        path: PathBuf,
    },
    /// An inbound file was dropped before it completed.
    FileLost {
        from: String,
        file_id: String,
        name: String,
        reason: String,
    },
    /// A send of ours that was in flight when we last quit must be
    /// re-supplied from disk.
    ResendWanted { file_id: String, to: String },
    Typing { from: String, on: bool },
    Read { from: String, ids: Vec<String> },
    Protocol(Box<Event>),
    Error(String),
}

pub enum Input {
    Net(NetEvent),
    Ui(Command),
}

struct PendingFile {
    to: String,
    path: PathBuf,
    file_id: String,
}

pub struct Engine {
    protocol: OfflineProtocol,
    streams: HashMap<String, TcpStream>,
    updates: Sender<Update>,
    /// Protocol events, routed through the engine so file events can be
    /// enriched (and files saved) before the UI hears of them.
    events: Receiver<Event>,
    /// Files waiting for a session or a free slot, oldest first.
    outbox: VecDeque<PendingFile>,
    downloads: PathBuf,
}

/// A fresh transfer id: our own, so the UI can track a file from the moment
/// it is queued rather than only once the SDK accepts it.
pub fn new_file_id() -> String {
    let mut bytes = [0u8; 12];
    let _ = getrandom::fill(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("hc_{hex}")
}

/// Opens (or creates) this profile's identity and stores under `root`, and
/// returns the engine with the identity it presents to peers and the key
/// the profile's other local files are sealed with.
pub fn open(
    root: &Path,
    nick: &str,
    downloads: PathBuf,
    updates: Sender<Update>,
) -> Result<(Engine, LocalIdentity, [u8; 32]), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(root)?;
    let store_key = load_or_create_key(&root.join("store.key"))?;
    let key = StaticStoreKey::new(store_key);
    let namespace = account_storage_namespace(APP_ID, PROFILE);
    let stores = FileStorePair::open(root.join("keys"), root.join("state"), &namespace, &key)?;

    let config = ProtocolConfig::builder(APP_ID, PROFILE)
        .ble_enabled(false)
        .internet_enabled(false)
        .wifi_direct_enabled(true)
        .require_encryption(true)
        .build()?;
    let mut protocol = OfflineProtocol::new(config)?;
    protocol.initialize_mls(stores.secure_storage(), stores.state_storage())?;

    let address = protocol
        .local_address()
        .ok_or("identity was not established")?
        .to_string();
    let preamble = protocol
        .mls_manager()
        .ok_or("MLS is not initialized")?
        .read()
        .map_err(|_| "MLS lock poisoned")?
        .identity_assertion(&[])?;

    let lan = WifiDirectTransport::new(address.clone());
    lan.start()?;
    lan.on_status_changed(TransportStatus::Available);
    protocol
        .transport_manager_mut()
        .add_transport(TransportType::WiFiDirect, Box::new(lan));

    // The callback runs under the SDK's state lock, so it may only queue.
    let (event_tx, events) = mpsc::channel();
    protocol.on_event(move |event| {
        let _ = event_tx.send(event);
    });
    protocol.start()?;

    let me = LocalIdentity {
        address,
        nick: nick.to_string(),
        preamble,
    };
    Ok((
        Engine {
            protocol,
            streams: HashMap::new(),
            updates,
            events,
            outbox: VecDeque::new(),
            downloads,
        },
        me,
        store_key,
    ))
}

fn load_or_create_key(path: &Path) -> std::io::Result<[u8; 32]> {
    if let Ok(bytes) = std::fs::read(path)
        && let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return Ok(key);
        }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| std::io::Error::other(e.to_string()))?;
    write_private(path, &key)?;
    Ok(key)
}

#[cfg(unix)]
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}

#[cfg(not(unix))]
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

impl Engine {
    fn lan(&self) -> Option<Arc<dyn Transport>> {
        self.protocol
            .transport_manager()
            .get_transport(TransportType::WiFiDirect)
    }

    fn with_lan(&self, f: impl FnOnce(&WifiDirectTransport)) {
        if let Some(lan) = self.lan()
            && let Some(lan) = lan.as_any().downcast_ref::<WifiDirectTransport>() {
                f(lan);
            }
    }

    fn update(&self, update: Update) {
        let _ = self.updates.send(update);
    }

    pub fn run(mut self, inputs: Receiver<Input>) {
        loop {
            match inputs.recv_timeout(TICK) {
                Ok(input) => self.handle(input),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(input) = inputs.try_recv() {
                self.handle(input);
            }
            self.pump();
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Net(NetEvent::Seen { address, nick }) => {
                self.update(Update::PeerSeen { address, nick });
            }
            Input::Net(NetEvent::Up { address, stream }) => {
                self.streams.insert(address.clone(), stream);
                self.with_lan(|lan| lan.on_peer_connected(address.clone()));
                // Also sends our key package, which starts the MLS handshake.
                self.protocol.on_neighbor_discovered(&address);
                // A session restored from storage fires no "established"
                // event, so report it here.
                let restored = self.session_ready(&address);
                self.update(Update::PeerUp {
                    address: address.clone(),
                });
                if restored {
                    self.update(Update::SessionReady { address });
                }
            }
            Input::Net(NetEvent::Down { address }) => self.drop_peer(&address),
            Input::Net(NetEvent::Frame { address, body }) => {
                if let Some(lan) = self.lan()
                    && let Err(e) = lan.on_data_received_from(body, address) {
                        self.error(format!("inbound frame refused: {e}"));
                    }
            }
            Input::Ui(Command::Send { to, text, reply }) => self.send_text(to, text, reply),
            Input::Ui(Command::SendFile { to, path, file_id }) => {
                self.queue_file(to, path, file_id)
            }
            Input::Ui(Command::Typing { to, on }) => {
                // Advisory: only worth sending over a live session.
                if self.streams.contains_key(&to) && self.session_ready(&to) {
                    let _ = self.protocol.send_typing_indicator(&to, &to, on);
                }
            }
            Input::Ui(Command::Read { to, ids }) => {
                for ids in ids.chunks(256) {
                    let _ = self.protocol.send_read_receipt(&to, ids.to_vec());
                }
            }
        }
    }

    fn session_ready(&self, address: &str) -> bool {
        matches!(
            self.protocol.get_establishment_state(address),
            Ok(EstablishmentState::SessionConfirmed)
        )
    }

    fn send_text(&mut self, to: String, text: String, reply: Option<Quote>) {
        let options = SendMessageOptions {
            reply_to_msg: reply.as_ref().map(|q| q.id.clone()),
            reply_context: reply.as_ref().and_then(|q| {
                Some(ReplyContext {
                    sender: UserId::new(q.sender.clone()).ok()?,
                    text: q.text.chars().take(QUOTE_CHARS).collect(),
                    timestamp: None,
                    reply_media_label: None,
                    reply_content_type: None,
                })
            }),
            ..Default::default()
        };
        match self.protocol.send_message_with(&to, &text, options) {
            Ok(id) => self.update(Update::Queued {
                to,
                id: id.as_str(),
                text,
                reply,
            }),
            Err(e) => self.error(format!("send failed: {e}")),
        }
    }

    fn queue_file(&mut self, to: String, path: PathBuf, file_id: String) {
        let size = match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => meta.len(),
            Ok(_) => return self.file_failed(file_id, "not a regular file".into()),
            Err(e) => return self.file_failed(file_id, e.to_string()),
        };
        if size == 0 {
            return self.file_failed(file_id, "file is empty".into());
        }
        if size > MAX_FILE_BYTES {
            return self.file_failed(file_id, "larger than 100 MB".into());
        }
        let name = display_name(&path);
        self.update(Update::FileQueued {
            file_id: file_id.clone(),
            name,
            size,
        });
        self.outbox.push_back(PendingFile { to, path, file_id });
    }

    /// Hands queued files to the SDK as sessions and transfer slots allow.
    /// A file that cannot start yet keeps its place; the rest still try.
    fn start_files(&mut self) {
        let mut waiting = VecDeque::new();
        while let Some(file) = self.outbox.pop_front() {
            if !self.streams.contains_key(&file.to) || !self.session_ready(&file.to) {
                waiting.push_back(file);
                continue;
            }
            let bytes = match std::fs::read(&file.path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    self.file_failed(file.file_id, e.to_string());
                    continue;
                }
            };
            let options = MediaSendOptions {
                file_id: Some(file.file_id.clone()),
                ..Default::default()
            };
            let name = display_name(&file.path);
            let kind = content_type_of(&file.path);
            match self
                .protocol
                .send_media_with(&file.to, bytes, name, kind, options)
            {
                Ok(_) => {}
                Err(ProtocolError::SessionNotReady(_) | ProtocolError::MediaTransferLimit(_)) => {
                    waiting.push_back(file)
                }
                Err(e) => self.file_failed(file.file_id, e.to_string()),
            }
        }
        self.outbox = waiting;
    }

    fn file_failed(&self, file_id: String, reason: String) {
        self.update(Update::FileFailed { file_id, reason });
    }

    fn drop_peer(&mut self, address: &str) {
        if let Some(stream) = self.streams.remove(address) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        self.with_lan(|lan| {
            lan.on_peer_disconnected(address);
        });
        self.protocol.on_neighbor_lost(address);
        self.update(Update::PeerDown {
            address: address.to_string(),
        });
    }

    /// One turn of the engine: run timers, surface chat, flush the wire.
    fn pump(&mut self) {
        if !self.outbox.is_empty() {
            self.start_files();
        }
        if let Err(e) = self.protocol.process() {
            self.error(format!("process: {e}"));
        }
        while let Some(message) = self.protocol.receive_message() {
            let reply = message.reply_context.as_ref().map(|r| Quote {
                id: message
                    .reply_to_msg
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or_default(),
                sender: r.sender.as_str().to_string(),
                text: r.text.clone(),
            });
            self.update(Update::Incoming {
                from: message.sender.as_str().to_string(),
                id: message.id.as_str(),
                text: message.content,
                hops: message.hop_count.value(),
                reply,
            });
        }
        while let Ok(event) = self.events.try_recv() {
            self.on_event(event);
        }

        let Some(lan) = self.lan() else { return };
        let mut broken = Vec::new();
        loop {
            match lan.get_next_message() {
                Ok(Some((peer, body))) => match self.streams.get_mut(&peer) {
                    Some(stream) => {
                        if net::write_frame(stream, &body).is_err() {
                            broken.push(peer);
                        }
                    }
                    None => broken.push(peer),
                },
                Ok(None) => break,
                Err(e) => {
                    self.error(format!("outbound frame: {e}"));
                    break;
                }
            }
        }
        broken.dedup();
        for peer in broken {
            if self.streams.contains_key(&peer) {
                self.drop_peer(&peer);
            }
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::SecureSessionEstablished { ref peer_id, .. } => {
                let address = peer_id.clone();
                self.update(Update::SessionReady { address });
            }
            Event::FileProgress {
                ref file_id,
                chunks_sent,
                total_chunks,
                ..
            } => {
                let inbound = self
                    .protocol
                    .file_transfer_manager()
                    .get_progress(file_id)
                    .map(|p| (p.file_name, p.file_size));
                self.update(Update::FileProgress {
                    file_id: file_id.clone(),
                    done: chunks_sent,
                    total: total_chunks,
                    inbound,
                });
                return;
            }
            Event::MediaSent { file_id, .. } => {
                return self.update(Update::FileSent { file_id });
            }
            Event::MediaSendFailed {
                file_id, reason, ..
            } => return self.file_failed(file_id, reason),
            Event::MediaResendRequired {
                file_id, recipient, ..
            } => {
                return self.update(Update::ResendWanted {
                    file_id,
                    to: recipient,
                });
            }
            Event::FileReceived {
                file_id,
                file_name,
                file_size,
                sender,
                file_data,
                ..
            } => {
                match self.save(&file_name, &file_data) {
                    Ok(path) => self.update(Update::FileReceived {
                        from: sender,
                        file_id,
                        name: file_name,
                        size: file_size,
                        path,
                    }),
                    Err(e) => self.update(Update::FileLost {
                        from: sender,
                        file_id,
                        name: file_name,
                        reason: format!("could not save: {e}"),
                    }),
                }
                return;
            }
            Event::FileReceiveFailed {
                file_id,
                file_name,
                sender,
                reason,
            } => {
                return self.update(Update::FileLost {
                    from: sender,
                    file_id,
                    name: file_name,
                    reason,
                });
            }
            Event::TypingIndicatorReceived {
                ref sender,
                is_typing,
                ..
            } => self.update(Update::Typing {
                from: sender.clone(),
                on: is_typing,
            }),
            Event::ReadReceiptReceived {
                ref sender,
                ref message_ids,
                ..
            } => self.update(Update::Read {
                from: sender.clone(),
                ids: message_ids.clone(),
            }),
            _ => {}
        }
        self.update(Update::Protocol(Box::new(event)));
    }

    /// Writes a received file into the downloads folder under a name that
    /// cannot escape it and does not overwrite anything already there.
    fn save(&self, name: &str, base64_data: &str) -> std::io::Result<PathBuf> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(base64_data)
            .map_err(std::io::Error::other)?;
        std::fs::create_dir_all(&self.downloads)?;
        let name = safe_file_name(name);
        let (stem, ext) = match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
            _ => (name.clone(), String::new()),
        };
        for n in 0..1000 {
            let candidate = if n == 0 {
                self.downloads.join(&name)
            } else {
                self.downloads.join(format!("{stem} ({n}){ext}"))
            };
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    file.write_all(&bytes)?;
                    return Ok(candidate);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("too many files with that name"))
    }

    fn error(&self, message: String) {
        self.update(Update::Error(message));
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into())
}

/// A peer-chosen file name reduced to one harmless path component.
pub fn safe_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .take(120)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

fn content_type_of(path: &Path) -> ContentType {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "bmp" | "svg" => ContentType::Image,
        "mp4" | "mov" | "mkv" | "webm" | "avi" => ContentType::Video,
        "mp3" | "m4a" | "wav" | "flac" | "ogg" | "opus" | "aac" => ContentType::Audio,
        _ => ContentType::File,
    }
}

#[cfg(test)]
mod tests {
    use super::safe_file_name;

    #[test]
    fn file_names_cannot_escape_downloads() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("..\\..\\boot.ini"), "boot.ini");
        assert_eq!(safe_file_name(".bashrc"), "bashrc");
        assert_eq!(safe_file_name(".."), "file");
        assert_eq!(safe_file_name("a\u{1b}[31mb.txt"), "a[31mb.txt");
        assert_eq!(safe_file_name("photo.jpg"), "photo.jpg");
    }
}
