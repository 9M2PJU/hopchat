//! Finding devices on the local network: every backend looks in its own
//! way, all at once, and this holds what they share.

use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::driver::{Found, BACKENDS};

const PORT_WAIT: Duration = Duration::from_millis(400);
const SWEEPERS: usize = 64;

/// Every device any backend finds, reported through `each` as it is found.
pub fn scan(each: impl Fn(Found) + Send + Clone + 'static) {
    let scans: Vec<_> = BACKENDS
        .iter()
        .map(|backend| {
            let each = each.clone();
            thread::spawn(move || backend.discover(&each))
        })
        .collect();
    for scan in scans {
        let _ = scan.join();
    }
}

/// Addresses on this machine's /24 with any of `ports` open, for devices
/// that answer no broadcast.
pub fn sweep(ports: &[u16]) -> Vec<Ipv4Addr> {
    let Some(me) = local_ipv4() else { return Vec::new() };
    let [a, b, c, _] = me.octets();
    let hosts: Vec<Ipv4Addr> = (1..=254)
        .map(|d| Ipv4Addr::new(a, b, c, d))
        .filter(|ip| *ip != me)
        .collect();
    let (tx, rx) = mpsc::channel();
    let chunk = hosts.len().div_ceil(SWEEPERS);
    let workers: Vec<_> = hosts
        .chunks(chunk)
        .map(|part| {
            let part = part.to_vec();
            let ports = ports.to_vec();
            let tx = tx.clone();
            thread::spawn(move || {
                for ip in part {
                    let open = ports.iter().any(|port| {
                        TcpStream::connect_timeout(&SocketAddr::from((ip, *port)), PORT_WAIT).is_ok()
                    });
                    if open {
                        let _ = tx.send(ip);
                    }
                }
            })
        })
        .collect();
    drop(tx);
    for worker in workers {
        let _ = worker.join();
    }
    let mut found: Vec<Ipv4Addr> = rx.into_iter().collect();
    found.sort();
    found
}

/// The address this machine uses on the LAN. Connecting a UDP socket sends
/// nothing; it only makes the OS pick the outgoing interface.
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(addr) if !addr.ip().is_loopback() => Some(*addr.ip()),
        _ => None,
    }
}
