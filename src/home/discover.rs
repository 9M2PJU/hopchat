//! Finding devices on the local network: Wiz bulbs answer a UDP broadcast;
//! LG TVs are found by sweeping this machine's /24 for the webOS API port,
//! since many sets (and many routers) never answer SSDP.

use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::{webos, wiz, Kind};

const WIZ_WAIT: Duration = Duration::from_secs(3);
const PORT_WAIT: Duration = Duration::from_millis(400);
const SWEEPERS: usize = 64;

/// A device that answered, before anyone has added it.
pub struct Found {
    pub kind: Kind,
    pub ip: Ipv4Addr,
    pub mac: Option<String>,
    /// The Wiz module name, for a light.
    pub module: Option<String>,
}

/// Every light and TV that answers, reported through `each` as it is found.
pub fn scan(each: impl Fn(Found) + Send + Clone + 'static) {
    let lights = {
        let each = each.clone();
        thread::spawn(move || {
            for (ip, mac) in wiz::discover(WIZ_WAIT) {
                let module = wiz::Bulb { ip }.system().ok().map(|s| s.module);
                each(Found { kind: Kind::Light, ip, mac, module });
            }
        })
    };
    for ip in tv_candidates() {
        if webos::Tv::probe(ip) {
            let mac = webos::mac_of(ip).map(|m| webos::format_mac(&m));
            each(Found { kind: Kind::Tv, ip, mac, module: None });
        }
    }
    let _ = lights.join();
}

/// Addresses on this machine's /24 with a webOS API port open.
fn tv_candidates() -> Vec<Ipv4Addr> {
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
            let tx = tx.clone();
            thread::spawn(move || {
                for ip in part {
                    let open = [3001u16, 3000].iter().any(|port| {
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
