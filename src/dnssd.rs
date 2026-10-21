//! DNS-SD discovery, the way every Offline Protocol SDK app finds peers on a
//! LAN, so hopchat and the SDK's iOS, Android and Python apps meet.
//!
//! The record is the one `docs/spec/stream-framing.md` fixes: service type
//! `_offlineprotocol._tcp`, TXT `txtvers=1` then `addr=<off1…>`. Instance
//! and host names are `op-` and a digest of the address, as the other
//! platforms publish. It runs next to the multicast beacon, not instead of
//! it, so older hopchats still find this one.
//!
//! A record is only a hint: the stream it leads to must prove `addr` in its
//! preamble before anything is announced.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use sha2::{Digest, Sha256};

use crate::net::{self, Announced, LocalIdentity, NetEvent};

const SERVICE_TYPE: &str = "_offlineprotocol._tcp.local.";
const TXT_VERSION: &str = "1";
const ADDRESS_PREFIX: &str = "off1";
/// How often known records are checked for a peer that needs dialing.
const REDIAL_INTERVAL: Duration = Duration::from_secs(2);

/// Publishes this device and dials the peers the LAN publishes. Errors only
/// if the mDNS responder can't start; the beacon works without it.
pub fn start(
    me: LocalIdentity,
    tcp_port: u16,
    events: Sender<NetEvent>,
    announced: Announced,
) -> Result<(), mdns_sd::Error> {
    let daemon = ServiceDaemon::new()?;
    // The listener is IPv4 only, so a record must not offer IPv6 to dial.
    daemon.disable_interface(IfKind::IPv6)?;

    let label = format!("op-{}", digest(&me.address));
    let txt = [("txtvers", TXT_VERSION), ("addr", me.address.as_str())];
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &label,
        &format!("{label}.local."),
        "",
        tcp_port,
        &txt[..],
    )?
    .enable_addr_auto();
    daemon.register(info)?;
    let browse = daemon.browse(SERVICE_TYPE)?;

    thread::spawn(move || {
        // Owned here so the responder lives as long as the browser.
        let _daemon = daemon;
        let dialing: Arc<Mutex<HashSet<String>>> = Arc::default();
        // Instance full name to the address it claims and where it listens.
        let mut records: HashMap<String, (String, Vec<SocketAddr>)> = HashMap::new();
        loop {
            match browse.recv_timeout(REDIAL_INTERVAL) {
                Ok(ServiceEvent::ServiceResolved(service)) => {
                    let found = peer_from_record(
                        |key| service.get_property_val_str(key),
                        service.port,
                        service.addresses.iter().map(|ip| ip.to_ip_addr()),
                    );
                    match found {
                        Some((address, _)) if address == me.address => {}
                        Some(record) => {
                            records.insert(service.fullname.clone(), record);
                        }
                        None => {
                            records.remove(&service.fullname);
                        }
                    }
                }
                Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                    records.remove(&fullname);
                }
                Ok(_) => continue,
                Err(mdns_sd::RecvTimeoutError::Timeout) => {}
                Err(mdns_sd::RecvTimeoutError::Disconnected) => return,
            }
            // The same rule as the beacon: the lower address dials.
            for (address, peers) in records.values() {
                if me.address.as_str() >= address.as_str()
                    || announced.lock().unwrap().contains(address)
                    || !dialing.lock().unwrap().insert(address.clone())
                {
                    continue;
                }
                let (address, peers) = (address.clone(), peers.clone());
                let (me, events, announced, dialing) = (
                    me.clone(),
                    events.clone(),
                    announced.clone(),
                    dialing.clone(),
                );
                thread::spawn(move || {
                    net::dial(&peers, address.clone(), me, events, announced);
                    dialing.lock().unwrap().remove(&address);
                });
            }
        }
    });
    Ok(())
}

/// The peer a resolved record names: its claimed address and the IPv4
/// sockets to try. `None` for a record this browser must not dial: a service
/// instance (it carries `sid`), or one without a well-formed `addr`.
fn peer_from_record<'a>(
    property: impl Fn(&str) -> Option<&'a str>,
    port: u16,
    ips: impl Iterator<Item = IpAddr>,
) -> Option<(String, Vec<SocketAddr>)> {
    if property("sid").is_some() || port == 0 {
        return None;
    }
    let address = property("addr")?;
    if !address.starts_with(ADDRESS_PREFIX) {
        return None;
    }
    let mut peers: Vec<SocketAddr> = ips
        .filter(|ip| ip.is_ipv4() && !ip.is_loopback())
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    peers.sort();
    if peers.is_empty() {
        return None;
    }
    Some((address.to_string(), peers))
}

/// Sixteen hex digits of SHA-256, as the other platforms name their records.
fn digest(address: &str) -> String {
    Sha256::digest(address.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record<'a>(txt: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> {
        move |key| txt.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    #[test]
    fn digest_matches_the_python_bridge() {
        // hashlib.sha256(b"off1abc").hexdigest()[:16]
        assert_eq!(digest("off1abc"), "3c345e5efcffd763");
    }

    #[test]
    fn a_peer_record_yields_its_ipv4_sockets() {
        let txt = [("txtvers", "1"), ("addr", "off1peer")];
        let ips = ["192.168.1.7", "fe80::1", "127.0.0.1"].map(|s| s.parse().unwrap());
        let (address, peers) = peer_from_record(record(&txt), 7878, ips.into_iter()).unwrap();
        assert_eq!(address, "off1peer");
        assert_eq!(peers, vec!["192.168.1.7:7878".parse().unwrap()]);
    }

    #[test]
    fn service_instances_and_bad_addresses_are_ignored() {
        let ip = || std::iter::once("10.0.0.2".parse().unwrap());
        let service = [("txtvers", "1"), ("sid", "chat"), ("addr", "off1peer")];
        assert!(peer_from_record(record(&service), 1, ip()).is_none());
        let foreign = [("txtvers", "1"), ("addr", "someone")];
        assert!(peer_from_record(record(&foreign), 1, ip()).is_none());
        let bare = [("txtvers", "1")];
        assert!(peer_from_record(record(&bare), 1, ip()).is_none());
        let ok = [("txtvers", "1"), ("addr", "off1peer")];
        assert!(peer_from_record(record(&ok), 0, ip()).is_none());
    }
}
