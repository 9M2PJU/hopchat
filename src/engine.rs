//! Owns the `OfflineProtocol` instance on one thread and pumps it: network
//! frames in, UI commands in, protocol events and chat messages out.

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use offline_protocol::{
    account_storage_namespace, EstablishmentState, Event, FileStorePair, OfflineProtocol, ProtocolConfig,
    StaticStoreKey,
};
use offline_protocol_transport::{
    Transport, TransportStatus, TransportType, WifiDirectTransport,
};

use crate::net::{self, LocalIdentity, NetEvent};

pub const APP_ID: &str = "dev.hopchat";
const PROFILE: &str = "default";
const TICK: Duration = Duration::from_millis(30);

/// What the UI asks the engine to do.
pub enum Command {
    Send { to: String, text: String },
}

/// What the engine tells the UI.
pub enum Update {
    PeerSeen { address: String, nick: String },
    PeerUp { address: String },
    PeerDown { address: String },
    /// An MLS session with `address` is usable (new or restored).
    SessionReady { address: String },
    /// A send was accepted locally under `id`.
    Queued { to: String, id: String, text: String },
    Incoming { from: String, text: String, hops: u8 },
    Protocol(Box<Event>),
    Error(String),
}

pub enum Input {
    Net(NetEvent),
    Ui(Command),
}

pub struct Engine {
    protocol: OfflineProtocol,
    streams: HashMap<String, TcpStream>,
    updates: Sender<Update>,
}

/// Opens (or creates) this profile's identity and stores under `root`, and
/// returns the engine with the identity it presents to peers.
pub fn open(
    root: &Path,
    nick: &str,
    updates: Sender<Update>,
) -> Result<(Engine, LocalIdentity), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(root)?;
    let key = StaticStoreKey::new(load_or_create_key(&root.join("store.key"))?);
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

    let events = updates.clone();
    protocol.on_event(move |event| {
        if let Event::SecureSessionEstablished { peer_id, .. } = &event {
            let address = peer_id.clone();
            let _ = events.send(Update::SessionReady { address });
        }
        let _ = events.send(Update::Protocol(Box::new(event)));
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
        },
        me,
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
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
                let _ = self.updates.send(Update::PeerSeen { address, nick });
            }
            Input::Net(NetEvent::Up { address, stream }) => {
                self.streams.insert(address.clone(), stream);
                self.with_lan(|lan| lan.on_peer_connected(address.clone()));
                // Also sends our key package, which starts the MLS handshake.
                self.protocol.on_neighbor_discovered(&address);
                // A session restored from storage fires no "established"
                // event, so report it here.
                let restored = matches!(
                    self.protocol.get_establishment_state(&address),
                    Ok(EstablishmentState::SessionConfirmed)
                );
                let _ = self.updates.send(Update::PeerUp {
                    address: address.clone(),
                });
                if restored {
                    let _ = self.updates.send(Update::SessionReady { address });
                }
            }
            Input::Net(NetEvent::Down { address }) => self.drop_peer(&address),
            Input::Net(NetEvent::Frame { address, body }) => {
                if let Some(lan) = self.lan()
                    && let Err(e) = lan.on_data_received_from(body, address) {
                        self.error(format!("inbound frame refused: {e}"));
                    }
            }
            Input::Ui(Command::Send { to, text }) => {
                match self.protocol.send_message(&to, &text, None, None::<String>) {
                    Ok(id) => {
                        let _ = self.updates.send(Update::Queued {
                            to,
                            id: id.as_str(),
                            text,
                        });
                    }
                    Err(e) => self.error(format!("send failed: {e}")),
                }
            }
        }
    }

    fn drop_peer(&mut self, address: &str) {
        if let Some(stream) = self.streams.remove(address) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        self.with_lan(|lan| {
            lan.on_peer_disconnected(address);
        });
        self.protocol.on_neighbor_lost(address);
        let _ = self.updates.send(Update::PeerDown {
            address: address.to_string(),
        });
    }

    /// One turn of the engine: run timers, surface chat, flush the wire.
    fn pump(&mut self) {
        if let Err(e) = self.protocol.process() {
            self.error(format!("process: {e}"));
        }
        while let Some(message) = self.protocol.receive_message() {
            let _ = self.updates.send(Update::Incoming {
                from: message.sender.as_str().to_string(),
                text: message.content,
                hops: message.hop_count.value(),
            });
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

    fn error(&self, message: String) {
        let _ = self.updates.send(Update::Error(message));
    }
}
