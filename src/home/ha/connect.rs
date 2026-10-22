//! Connecting home to a Home Assistant, listing what it has, and
//! letting it go: what `/home ha` and a scan do with the hub.

use std::net::Ipv4Addr;
use std::sync::mpsc::Sender;
use std::thread;

use crate::home::driver::backend_of;
use crate::home::{ha, Device, Home, Request, State};

/// The address and token typed after `/home ha`: `[URL] TOKEN`, in that
/// order. Anything else is said back as a mistake rather than guessed
/// at, so a token is never tried as an address, shown back as one, or
/// an address sent as the token.
pub(in crate::home) fn ha_args(first: &str, second: Option<&str>) -> Result<(Option<String>, String), String> {
    const USAGE: &str = "/home ha TOKEN, or /home ha URL TOKEN";
    match second {
        None if !looks_like_token(first) && (looks_like_address(first) || first.contains('.')) => {
            Err(format!("give the token too: {USAGE}"))
        }
        None => Ok((None, first.to_string())),
        Some(second) if looks_like_token(first) && !looks_like_token(second) => {
            Err(format!("the address goes before the token: {USAGE}"))
        }
        Some(second) => Ok((Some(first.to_string()), second.to_string())),
    }
}

/// Whether a word is an HA access token: a JWT, three base64url parts
/// joined by dots, the first a JSON header (`eyJ` is `{"`).
pub(in crate::home) fn looks_like_token(word: &str) -> bool {
    let parts: Vec<&str> = word.split('.').collect();
    word.starts_with("eyJ")
        && parts.len() == 3
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
}

/// Whether a word typed after `/home ha` is an address: `http://…` or
/// `host:port` or an IPv4 address. HA's tokens are JWTs, with none of these.
fn looks_like_address(word: &str) -> bool {
    word.contains("://") || word.contains(':') || word.parse::<Ipv4Addr>().is_ok()
}

impl Home {
    /// `/home ha …`: what it is connected to, connecting, or forgetting.
    pub(in crate::home) fn ha_configure(&mut self, value: Option<&str>, next: Option<&str>) -> String {
        match value {
            None => match &self.config.ha {
                Some(hub) => format!("home lists the devices of Home Assistant at {}; /home ha forget to stop", hub.url),
                None => "connect Home Assistant with /home ha TOKEN, or /home ha URL TOKEN; \
                         make the token under your HA profile, Security. /home ha forget disconnects it"
                    .into(),
            },
            Some("forget" | "off") => self.forget_ha(),
            Some(first) => match ha_args(first, next) {
                Ok((typed, token)) => {
                    self.connect_ha(typed, token);
                    "connecting to Home Assistant…".into()
                }
                Err(e) => e,
            },
        }
    }

    /// Reaches a Home Assistant, at `typed` or wherever one announces
    /// itself, and lists what it has.
    fn connect_ha(&self, typed: Option<String>, token: String) {
        let me = self.me.clone();
        thread::spawn(move || {
            let result = (|| {
                let url = match typed {
                    Some(typed) => ha::url(&typed)?,
                    None => ha::find().ok_or("no Home Assistant announces itself here; give its address: \
                                              /home ha http://IP:8123 TOKEN")?,
                };
                let hub = ha::Hub { url, token };
                // Said only to the person at this machine: where it tried.
                let tried = |e: String| format!("{e}; tried {}", hub.url);
                let home = hub.check().map_err(tried)?;
                let found = hub.entities().map_err(tried)?;
                Ok((hub, home, found))
            })();
            let _ = me.send(Request::HaConnected(result));
        });
    }

    pub(in crate::home) fn ha_connected(&mut self, result: Result<(ha::Hub, String, ha::Listing), String>) {
        let (hub, home, ha::Listing { found, warning }) = match result {
            Ok(connected) => connected,
            Err(e) => return self.note(format!("home: {e}")),
        };
        // Every HA device goes through the one hub, so a new token or
        // address reaches them all. Home has one hub: another address is
        // this one moved, or a replacement for it.
        let url = hub.url.clone();
        let before = self.config.ha.replace(hub).map(|h| h.url).filter(|old| *old != url);
        if let Some(old) = before {
            self.note(format!("home: Home Assistant at {old} is replaced by the one at {url}"));
        }
        let before = self.found.len();
        self.on_listed(found);
        self.save();
        let new = self.found.len() - before;
        if let Some(warning) = warning {
            self.note(format!("home: {warning}"));
        }
        self.note(format!(
            "connected to {home} at {url}: {new} new device{} to add in /devices",
            if new == 1 { "" } else { "s" },
        ));
    }

    /// `/home ha forget`: drops the hub, its token, and every device from it.
    /// Every HA device is the one hub's.
    pub(in crate::home) fn forget_ha(&mut self) -> String {
        let Some(hub) = self.config.ha.take() else {
            return "home isn't connected to Home Assistant".into();
        };
        let gone: Vec<String> = self.config.devices.iter().filter(|d| ha::drives(d)).map(Device::id).collect();
        self.config.devices.retain(|d| !ha::drives(d));
        self.found.retain(|d| !ha::drives(d));
        for id in &gone {
            self.live.remove(id);
        }
        self.save();
        format!(
            "disconnected Home Assistant at {} and forgot its {} device{}",
            hub.url,
            gone.len(),
            if gone.len() == 1 { "" } else { "s" },
        )
    }
}

/// Lists a hub's entities, each with its state, for a scan. An added
/// device the listing leaves out is checked on its own, so none stays
/// "checking"; those checks run side by side, like any other device's.
/// A hub that can't be listed can't drive anything either, so its
/// devices are all offline at once rather than after a wait each.
pub(in crate::home) fn list_hub(hub: &ha::Hub, added: Vec<Device>, me: &Sender<Request>) {
    let found = match hub.entities() {
        Ok(listing) => listing.found,
        Err(e) => {
            for d in added {
                let _ = me.send(Request::Checked { id: d.id(), state: State::Offline, detail: e.clone() });
            }
            return;
        }
    };
    let listed = ha::listed(&found);
    let _ = me.send(Request::Listed(found));
    let checks: Vec<_> = added
        .into_iter()
        .filter(|d| !d.unit.as_ref().is_some_and(|u| listed.contains(u)))
        .filter_map(|d| {
            let backend = backend_of(&d)?;
            let me = me.clone();
            Some(thread::spawn(move || {
                let (state, detail) = backend.check(&d);
                let _ = me.send(Request::Checked { id: d.id(), state, detail });
            }))
        })
        .collect();
    for check in checks {
        let _ = check.join();
    }
}
