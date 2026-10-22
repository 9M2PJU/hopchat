//! Connecting home to a Home Assistant, listing what it has, and
//! letting it go: what `/home ha` and a scan do with the hub.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::mpsc::Sender;
use std::thread;

use serde_json::Value;

use crate::home::driver::{backend_of, Found, MAX_PARALLEL};
use crate::home::{ha, Device, Home, Request, State};

/// The address and token typed after `/home ha`: `[URL] TOKEN`, in that
/// order. Anything else is said back as a mistake rather than guessed
/// at, so a token is never tried as an address, shown back as one, or
/// an address sent as the token.
pub(in crate::home) fn ha_args(first: &str, second: Option<&str>) -> Result<(Option<String>, String), String> {
    const USAGE: &str = "/home ha URL TOKEN (/home ha find looks for the address)";
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
                None => "connect Home Assistant with /home ha URL TOKEN; /home ha find looks for its address; \
                         make the token under your HA profile, Security. /home ha forget disconnects it"
                    .into(),
            },
            Some("forget" | "off") => self.forget_ha(),
            // Two at once would race to be the one hub; the later answer
            // would quietly undo the earlier.
            Some(_) if self.connecting_ha.is_some() => "already connecting to Home Assistant; wait for it to answer".into(),
            Some("find") => {
                self.find_ha();
                "looking for a Home Assistant announcing itself…".into()
            }
            Some(first) => match ha_args(first, next) {
                Ok((Some(typed), token)) => {
                    self.connect_ha(typed, token);
                    "connecting to Home Assistant…".into()
                }
                // A token with no address: the token isn't sent anywhere
                // home only heard of, so this only says where HA is.
                Ok((None, _)) => {
                    self.find_ha();
                    "looking for a Home Assistant announcing itself; the token isn't sent until you give its address…".into()
                }
                Err(e) => e,
            },
        }
    }

    /// Looks for a Home Assistant announcing itself on the LAN and says
    /// where, for the person to connect to by typing it with the token.
    /// Anything on the LAN can announce itself as HA, so the token never
    /// goes to an address found this way.
    fn find_ha(&mut self) {
        let attempt = self.start_ha_attempt();
        let me = self.me.clone();
        thread::spawn(move || {
            let _ = me.send(Request::HaConnected { attempt, result: Err(found_only(ha::find())) });
        });
    }

    /// Reaches the Home Assistant at `typed` and lists what it has.
    fn connect_ha(&mut self, typed: String, token: String) {
        let attempt = self.start_ha_attempt();
        let me = self.me.clone();
        thread::spawn(move || {
            let result = (|| {
                let hub = ha::Hub { url: ha::url(&typed)?, token };
                // Said only to the person at this machine: where it tried.
                let tried = |e: String| format!("{e}; tried {}", hub.url);
                let home = hub.check().map_err(tried)?;
                let found = hub.entities().map_err(tried)?;
                Ok((hub, home, found))
            })();
            let _ = me.send(Request::HaConnected { attempt, result });
        });
    }

    /// Marks a `/home ha` attempt as the one on its way, and numbers it.
    fn start_ha_attempt(&mut self) -> u64 {
        self.ha_attempts += 1;
        self.connecting_ha = Some(self.ha_attempts);
        self.ha_attempts
    }

    /// What a `/home ha` connection came back with. Only the one connecting
    /// now is taken: one that answers after `/home ha forget` is dropped.
    pub(in crate::home) fn on_ha_connected(&mut self, attempt: u64, result: Result<(ha::Hub, String, ha::Listing), String>) {
        if self.connecting_ha == Some(attempt) {
            self.connecting_ha = None;
            self.ha_connected(result);
        }
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
        let before = self.config.ha.replace(hub);
        let same = before.as_ref() == self.config.ha.as_ref();
        let moved = before.map(|h| h.url).filter(|old| *old != url);
        if let Some(old) = &moved {
            self.note(format!("home: Home Assistant at {old} is replaced by the one at {url}"));
        }
        let gone = self.keep_what_the_hub_vouches_for(&found, moved.is_some(), same);
        self.drop_unlisted_offers(&ha::listed(&found));
        if !gone.is_empty() {
            let one = gone.len() == 1;
            self.note(format!(
                "home: forgot {}: the Home Assistant at {url} doesn't list {} as before; add {} again in /devices if {} the same",
                gone.join(", "),
                if one { "it" } else { "them" },
                if one { "it" } else { "them" },
                if one { "it is" } else { "they are" },
            ));
        }
        let offered: HashSet<String> = self.found.iter().filter(|d| ha::drives(d)).map(Device::id).collect();
        self.learn_all(found);
        self.save();
        let new = self.found.iter().filter(|d| ha::drives(d) && !offered.contains(&d.id())).count();
        if let Some(warning) = warning {
            self.note(format!("home: {warning}"));
        }
        self.note(format!(
            "connected to {home} at {url}: {new} new device{} to add in /devices",
            if new == 1 { "" } else { "s" },
        ));
    }

    /// Forgets each HA device, added or only offered, that the hub just
    /// listed doesn't vouch for, and says which added ones went. An entity
    /// id is only unique within one Home Assistant: another one's
    /// `light.kitchen` may be a different light, and the device people
    /// call "Kitchen" mustn't silently drive it.
    ///
    /// The `same` hub as before, address and token alike, is the one a
    /// scan has been listing all along, and a changed name is a rename in
    /// HA, kept up with as a scan would. Otherwise: when the hub `moved`
    /// to another address, a device stays only where the new one lists
    /// its entity id under the same HA name. At the same address with a
    /// new token, one the listing leaves out stays (it may be hidden in
    /// HA since), and only one listed under another name goes: another
    /// Home Assistant answers there now, or the entity was renamed, and
    /// either way it is added again knowingly. A device whose HA name was
    /// never kept can't be told apart that way, so it stays wherever its
    /// entity id is listed. Saving is left to the caller.
    fn keep_what_the_hub_vouches_for(&mut self, listed: &[Found], moved: bool, same: bool) -> Vec<String> {
        let names: HashMap<&str, Option<&str>> = listed
            .iter()
            .filter_map(|f| Some((f.unit.as_deref()?, f.data.get("name").and_then(Value::as_str))))
            .collect();
        let keep = |d: &Device| {
            if !ha::drives(d) {
                return true;
            }
            match d.unit.as_deref().and_then(|u| names.get(u)) {
                None => !moved,
                Some(_) if same => true,
                Some(listed) => match d.data.get("name").and_then(Value::as_str) {
                    None => true,
                    Some(named) => *listed == Some(named),
                },
            }
        };
        let (kept, gone): (Vec<Device>, Vec<Device>) = std::mem::take(&mut self.config.devices).into_iter().partition(keep);
        self.config.devices = kept;
        let (offered, dropped): (Vec<Device>, Vec<Device>) = std::mem::take(&mut self.found).into_iter().partition(keep);
        self.found = offered;
        for id in gone.iter().chain(&dropped).map(Device::id) {
            self.live.remove(&id);
        }
        gone.into_iter().map(|d| d.name).collect()
    }

    /// `/home ha forget`: drops the hub, its token, and every device from it.
    /// Every HA device is the one hub's.
    pub(in crate::home) fn forget_ha(&mut self) -> String {
        // A connection still on its way is dropped when it answers.
        self.connecting_ha = None;
        let Some(hub) = self.config.ha.take() else {
            return "home isn't connected to Home Assistant".into();
        };
        let gone: Vec<String> = self.config.devices.iter().filter(|d| ha::drives(d)).map(Device::id).collect();
        // Found ones, never added, have a line in the panel too.
        let offered: Vec<String> = self.found.iter().filter(|d| ha::drives(d)).map(Device::id).collect();
        self.config.devices.retain(|d| !ha::drives(d));
        self.found.retain(|d| !ha::drives(d));
        for id in gone.iter().chain(&offered) {
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

/// What `/home ha find`, or a token with no address, says: where a Home
/// Assistant announces itself, to connect to by typing it. The token is
/// never sent to an address home only heard of over the LAN.
pub(in crate::home) fn found_only(found: Option<String>) -> String {
    match found {
        Some(url) => format!("found Home Assistant at {url}; connect to it with /home ha {url} TOKEN"),
        None => "no Home Assistant announces itself here; give its address: /home ha http://IP:8123 TOKEN".into(),
    }
}

/// Lists a hub's entities, each with its state, for a scan. An added
/// device the listing leaves out is checked on its own, so none stays
/// "checking"; those checks run side by side, a few at a time. A hub
/// that can't be listed can't drive anything either, so its devices are
/// all offline at once rather than after a wait each. Everything sent
/// names the hub, so home can tell a scan that outlived it from a
/// current one.
pub(in crate::home) fn list_hub(hub: &ha::Hub, added: Vec<Device>, me: &Sender<Request>) {
    let checked = |id: String, state: State, detail: String| Request::HubChecked { hub: hub.clone(), id, state, detail };
    let listing = match hub.entities() {
        Ok(listing) => listing,
        Err(e) => {
            for d in added {
                let _ = me.send(checked(d.id(), State::Offline, e.clone()));
            }
            return;
        }
    };
    let listed = ha::listed(&listing.found);
    // Sent whole: a listing the registry couldn't filter says so, as
    // `/home ha` does, rather than offering hidden entities unannounced.
    let _ = me.send(Request::Listed { hub: hub.clone(), listing });
    let unlisted: Vec<Device> = added.into_iter().filter(|d| !d.unit.as_ref().is_some_and(|u| listed.contains(u))).collect();
    thread::scope(|s| {
        for batch in unlisted.chunks(MAX_PARALLEL) {
            let checks: Vec<_> = batch
                .iter()
                .filter_map(|d| {
                    let backend = backend_of(d)?;
                    let checked = &checked;
                    Some(s.spawn(move || {
                        let (state, detail) = backend.check(d);
                        let _ = me.send(checked(d.id(), state, detail));
                    }))
                })
                .collect();
            for check in checks {
                let _ = check.join();
            }
        }
    });
}
