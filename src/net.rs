//! The LAN bridge: finds peers with a UDP multicast beacon and with DNS-SD
//! (see `dnssd`), and carries frames over one TCP stream per peer.
//!
//! The SDK performs no I/O, so this module is the "platform" half of its
//! peer-stream (`wifi_direct`) slot. Every stream opens with the identity
//! assertion and is framed by `offline_protocol_transport::stream_framing`;
//! a peer is only announced under the address its preamble proved.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use offline_protocol_mls::verify_identity_assertion;
use offline_protocol_transport::stream_framing::{frame, PeerStreamReader, StreamEvent};
use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};

use crate::dnssd;

const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);
const DISCOVERY_PORT: u16 = 47_999;
const BEACON_MAGIC: &str = "hopchat/1";
const BEACON_INTERVAL: Duration = Duration::from_secs(2);
/// TCP keepalive, matching the SDK's peer-stream managers: a phone that was
/// suspended leaves a half-open stream, and a new one from it is refused
/// until this one is noticed dead.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(15);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const KEEPALIVE_RETRIES: u32 = 3;
/// How long a stream may take to prove who it is before it is dropped.
const PREAMBLE_WAIT: Duration = Duration::from_secs(10);
/// How long one write may stall before the peer counts as gone, so a peer
/// that stops reading cannot block the engine.
const WRITE_WAIT: Duration = Duration::from_secs(5);

/// What the network tells the engine.
pub enum NetEvent {
    /// A beacon was heard. The nick is unauthenticated and for display only.
    Seen { address: String, nick: String },
    /// A stream proved `address`; frames for that peer go to `stream`.
    Up { address: String, stream: TcpStream },
    /// One message body from a proved peer.
    Frame { address: String, body: Vec<u8> },
    /// The stream to `address` closed.
    Down { address: String },
}

/// Identity this device presents on every stream.
#[derive(Clone)]
pub struct LocalIdentity {
    pub address: String,
    pub nick: String,
    pub preamble: Vec<u8>,
}

/// Addresses with a live, announced stream. Shared so a second stream to the
/// same peer can be refused instead of announced twice.
pub type Announced = Arc<Mutex<HashSet<String>>>;

/// Addresses with a dial in flight or a dialed stream still open. Shared by
/// the beacon and DNS-SD so one peer is never dialed twice at once: two
/// streams racing their preambles can each be kept by a different side,
/// and then both get closed.
pub type Dialing = Arc<Mutex<HashSet<String>>>;

pub fn start(me: LocalIdentity, events: Sender<NetEvent>) -> std::io::Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    let tcp_port = listener.local_addr()?.port();
    let announced: Announced = Arc::default();
    let dialing: Dialing = Arc::default();

    {
        let (me, events, announced) = (me.clone(), events.clone(), announced.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                spawn_stream(stream, None, me.clone(), events.clone(), announced.clone());
            }
        });
    }

    // Optional: without an mDNS responder the beacon still finds hopchats.
    let _ = dnssd::start(
        me.clone(),
        tcp_port,
        events.clone(),
        announced.clone(),
        dialing.clone(),
    );

    let socket = discovery_socket()?;
    {
        let socket = socket.try_clone()?;
        let beacon = format!("{BEACON_MAGIC} {} {tcp_port} {}", me.address, me.nick);
        let target = SocketAddrV4::new(MULTICAST_GROUP, DISCOVERY_PORT);
        thread::spawn(move || {
            loop {
                let _ = socket.send_to(beacon.as_bytes(), target);
                thread::sleep(BEACON_INTERVAL);
            }
        });
    }

    thread::spawn(move || {
        let mut buf = [0u8; 512];
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf) else {
                continue;
            };
            let Some((address, port, nick)) = parse_beacon(&buf[..n]) else {
                continue;
            };
            if address == me.address {
                continue;
            }
            let _ = events.send(NetEvent::Seen {
                address: address.clone(),
                nick,
            });
            // One side dials so a pair never races into two streams: the
            // lower address. The other side just waits to be called.
            if me.address < address
                && !announced.lock().unwrap().contains(&address)
                && !dialing.lock().unwrap().contains(&address)
            {
                let peer = SocketAddr::new(from.ip(), port);
                let (me, events, announced, dialing) = (
                    me.clone(),
                    events.clone(),
                    announced.clone(),
                    dialing.clone(),
                );
                thread::spawn(move || dial(&[peer], address, me, events, announced, dialing));
            }
        }
    });

    Ok(tcp_port)
}

fn discovery_socket() -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Several hopchat processes on one machine all listen on the same port.
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT).into())?;
    socket.join_multicast_v4(&MULTICAST_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_multicast_loop_v4(true)?;
    Ok(socket.into())
}

fn parse_beacon(bytes: &[u8]) -> Option<(String, u16, String)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut parts = text.splitn(4, ' ');
    if parts.next()? != BEACON_MAGIC {
        return None;
    }
    let address = parts.next()?.to_string();
    let port = parts.next()?.parse().ok()?;
    let nick = parts.next().unwrap_or("?").chars().take(24).collect();
    Some((address, port, nick))
}

/// Connects to the first of `peers` that answers and runs the stream until
/// it closes, expecting it to prove `address`. Does nothing if `address` is
/// already being dialed.
pub fn dial(
    peers: &[SocketAddr],
    address: String,
    me: LocalIdentity,
    events: Sender<NetEvent>,
    announced: Announced,
    dialing: Dialing,
) {
    if !dialing.lock().unwrap().insert(address.clone()) {
        return;
    }
    for peer in peers {
        if let Ok(stream) = TcpStream::connect_timeout(peer, Duration::from_secs(3)) {
            run_stream(stream, Some(address.clone()), me, events, announced);
            break;
        }
    }
    dialing.lock().unwrap().remove(&address);
}

fn spawn_stream(
    stream: TcpStream,
    expecting: Option<String>,
    me: LocalIdentity,
    events: Sender<NetEvent>,
    announced: Announced,
) {
    thread::spawn(move || run_stream(stream, expecting, me, events, announced));
}

/// Runs one stream: sends our preamble, then reads until it closes.
///
/// `expecting` is the address a beacon or DNS-SD record claimed, for a stream
/// we dialed; the reader refuses a preamble that proves anything else.
fn run_stream(
    mut stream: TcpStream,
    expecting: Option<String>,
    me: LocalIdentity,
    events: Sender<NetEvent>,
    announced: Announced,
) {
    let _ = stream.set_nodelay(true);
    let keepalive = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL)
        .with_retries(KEEPALIVE_RETRIES);
    let _ = SockRef::from(&stream).set_tcp_keepalive(&keepalive);
    let _ = stream.set_read_timeout(Some(PREAMBLE_WAIT));
    let _ = stream.set_write_timeout(Some(WRITE_WAIT));
    let Ok(preamble) = frame(&me.preamble) else {
        return;
    };
    if stream.write_all(&preamble).is_err() {
        return;
    }

    let verify = Box::new(|b: &[u8]| verify_identity_assertion(b).ok().map(|a| a.to_string()));
    let mut reader = PeerStreamReader::new(verify);
    if let Some(address) = expecting {
        reader = reader.expecting(address);
    }

    let mut peer: Option<String> = None;
    let mut buf = vec![0u8; 64 * 1024];
    'read: loop {
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for event in reader.feed(&buf[..n]) {
            match event {
                StreamEvent::Announced(address) => {
                    let Ok(writer) = stream.try_clone() else {
                        break 'read;
                    };
                    let fresh = address != me.address
                        && announced.lock().unwrap().insert(address.clone());
                    if !fresh {
                        break 'read;
                    }
                    // Proven: from here keepalive notices a dead peer.
                    let _ = stream.set_read_timeout(None);
                    peer = Some(address.clone());
                    let _ = events.send(NetEvent::Up {
                        address,
                        stream: writer,
                    });
                }
                StreamEvent::Message { from, body } => {
                    let _ = events.send(NetEvent::Frame {
                        address: from,
                        body,
                    });
                }
                StreamEvent::Closed(_) => break 'read,
            }
        }
    }

    let _ = stream.shutdown(std::net::Shutdown::Both);
    if let Some(address) = peer {
        announced.lock().unwrap().remove(&address);
        let _ = events.send(NetEvent::Down { address });
    }
}

/// Writes one body to a peer's stream as one frame.
pub fn write_frame(stream: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    let framed = frame(body).map_err(|e| std::io::Error::other(e.to_string()))?;
    stream.write_all(&framed)
}
