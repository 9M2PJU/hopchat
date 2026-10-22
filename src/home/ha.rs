//! Home Assistant: one hub on the LAN that drives every brand it has an
//! integration for. Home talks to its REST API to drive things, and to its
//! WebSocket API once per listing, to read the entity registry.
//!
//! Connecting takes a long-lived access token (`/home ha [URL] TOKEN`),
//! made under the HA user's profile, Security tab. The hub is kept in the
//! settings so a scan can list its entities again, and each device added
//! from it keeps the URL and token too, so it can be driven on its own.
//! Each entity is a backend of its category: an HA light is driven through
//! the same `light_set` as a Wiz bulb.
//!
//! `http://` and `https://` both work. Over https, HA's certificate has to
//! be one this machine trusts.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

use super::driver::{backend_of, Backend, Category, Found};
use super::light::{scene_key, Caps, LightDriver, Pilot};
use super::switch::SwitchDriver;
use super::tv::TvDriver;
use super::{Device, State};

/// Where HA listens unless told otherwise.
const PORT: u16 = 8123;
const WAIT: Duration = Duration::from_secs(5);
/// How long to look for an HA announcing itself.
const FIND: Duration = Duration::from_secs(3);
const SERVICE: &str = "_home-assistant._tcp.local.";
/// Integrations home drives itself: their entities would be the same
/// device twice.
const NATIVE: &[&str] = &["wiz", "webostv"];
/// The most of an answer to read. A big home's `/api/states` runs to
/// megabytes, past the HTTP client's own 10 MB default.
const MAX_ANSWER: u64 = 64 * 1024 * 1024;
/// `LightEntityFeature::FLASH`.
const LIGHT_FLASH: u64 = 8;
/// `MediaPlayerEntityFeature::VOLUME_SET`.
const VOLUME_SET: u64 = 4;
/// What one `volume_up` moves by, as a percentage, when an integration
/// doesn't say otherwise.
const VOLUME_STEP: u8 = 10;

/// A Home Assistant to talk to.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Hub {
    pub url: String,
    pub token: String,
}

/// Never prints the token.
impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hub").field("url", &self.url).field("token", &"<redacted>").finish()
    }
}

/// What a Home Assistant lists, and a warning when it couldn't be filtered.
pub struct Listing {
    pub found: Vec<Found>,
    pub warning: Option<String>,
}

/// The `http://host:port` or `https://host:port` someone typed, with the
/// scheme and port filled in when left out.
pub fn url(typed: &str) -> Result<String, String> {
    let typed = typed.trim().trim_end_matches('/');
    let (scheme, rest) = match typed.split_once("://") {
        Some((scheme @ ("http" | "https"), rest)) => (scheme, rest),
        Some((other, _)) => return Err(format!("{other}:// isn't how to reach Home Assistant; use http:// or https://")),
        None => ("http", typed),
    };
    if rest.is_empty() || rest.contains('/') {
        return Err(format!("{typed} isn't an address like http://homeassistant.local:8123"));
    }
    // Unbracketed, an IPv6 address's last group would read as a port.
    if !rest.starts_with('[') && rest.matches(':').count() > 1 {
        return Err(format!("put an IPv6 address in brackets, like http://[{rest}]:8123"));
    }
    let has_port = rest.rsplit_once(':').is_some_and(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    Ok(if has_port { format!("{scheme}://{rest}") } else { format!("{scheme}://{rest}:{PORT}") })
}

/// The URL of a Home Assistant announcing itself on the LAN, if one does
/// within a few seconds.
pub fn find() -> Option<String> {
    let daemon = mdns_sd::ServiceDaemon::new().ok()?;
    let browse = daemon.browse(SERVICE).ok()?;
    let until = Instant::now() + FIND;
    let mut found = None;
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match browse.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(service)) => {
                // What HA itself says to use, else the address it answered from.
                let said = ["internal_url", "base_url"]
                    .iter()
                    .filter_map(|k| service.get_property_val_str(k))
                    .find_map(|u| url(u).ok());
                let seen = || {
                    let ip = service.get_addresses_v4().into_iter().next()?;
                    Some(format!("http://{ip}:{}", service.get_port()))
                };
                found = said.or_else(seen);
                if found.is_some() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    found
}

impl Hub {
    /// The hub a device added from it keeps in its data.
    fn of(d: &Device) -> Result<Hub, String> {
        let field = |k: &str| d.data.get(k).and_then(Value::as_str).map(str::to_string);
        match (field("url"), field("token")) {
            (Some(url), Some(token)) => Ok(Hub { url, token }),
            _ => Err(format!("{} lost its Home Assistant; connect again with /home ha", d.name)),
        }
    }

    /// What a device keeps to reach this hub.
    pub fn stamp(&self, data: &mut Value) {
        if !data.is_object() {
            *data = json!({});
        }
        data["url"] = json!(self.url);
        data["token"] = json!(self.token);
    }

    /// `host:port`, without the scheme.
    fn host(&self) -> &str {
        self.url.split_once("://").map_or(self.url.as_str(), |(_, host)| host)
    }

    fn secure(&self) -> bool {
        self.url.starts_with("https://")
    }

    /// One client for every call, so its connections to HA are kept and
    /// reused rather than opened per request.
    fn agent() -> &'static ureq::Agent {
        static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
        AGENT.get_or_init(|| {
            let tls = ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build();
            ureq::Agent::config_builder()
                .timeout_global(Some(WAIT))
                .tls_config(tls)
                // An HTTP error from HA says why in its body; keep it.
                .http_status_as_error(false)
                .build()
                .new_agent()
        })
    }

    /// The JSON HA answered, or why not.
    fn answer(&self, sent: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value, String> {
        let mut response = sent.map_err(|e| match e {
            ureq::Error::Timeout(_) => "Home Assistant took too long to answer".to_string(),
            e => format!("can't reach Home Assistant at {}: {e}", self.url),
        })?;
        let code = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_ANSWER)
            .read_to_string()
            .map_err(|e| format!("couldn't read Home Assistant's answer: {e}"))?;
        outcome(code, &body)
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let sent = Self::agent()
            .get(format!("{}{path}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .call();
        self.answer(sent)
    }

    /// Calls a service, such as `light.turn_on`, on one entity, and returns
    /// the entity's new state if HA's answer has it. HA answers with every
    /// state the call changed; one that changes later, as a polled or
    /// cloud device may, isn't in it.
    fn call(&self, service: &str, entity: &str, mut body: Value) -> Result<Option<Value>, String> {
        let (domain, service) = service.split_once('.').unwrap_or((service, ""));
        body["entity_id"] = json!(entity);
        let sent = Self::agent()
            .post(format!("{}/api/services/{domain}/{service}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .send_json(&body);
        Ok(changed(&self.answer(sent)?, entity))
    }

    /// One entity's state and attributes.
    fn state(&self, entity: &str) -> Result<Value, String> {
        self.get(&format!("/api/states/{entity}")).map_err(|e| {
            if e.ends_with("HTTP 404") || e.contains("Entity not found") { format!("Home Assistant has no {entity}") } else { e }
        })
    }

    /// The name of the home it runs, as a check that the URL and token work
    /// and that what answers is a Home Assistant.
    pub fn check(&self) -> Result<String, String> {
        home_name(&self.get("/api/config")?).ok_or_else(|| format!("{} answers, but not as Home Assistant", self.url))
    }

    /// The address devices from it are listed at in the panel.
    fn ip(&self) -> Ipv4Addr {
        self.host()
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| {
                addrs.find_map(|a| match a {
                    SocketAddr::V4(v4) => Some(*v4.ip()),
                    SocketAddr::V6(_) => None,
                })
            })
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    /// Every entity home can drive, as found devices, each with its state.
    pub fn entities(&self) -> Result<Listing, String> {
        let states = self.get("/api/states")?;
        // Without the registry, nothing is filtered out; still worth listing.
        let (registry, warning) = match self.registry() {
            Ok(registry) => (registry, None),
            Err(e) => (
                HashMap::new(),
                Some(format!(
                    "couldn't read Home Assistant's entity registry ({e}), so its hidden and settings \
                     entities, and its own copies of Wiz and LG devices, are listed too"
                )),
            ),
        };
        let ip = self.ip();
        let found = states
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|state| {
                let entity = state.get("entity_id")?.as_str()?;
                if registry.get(entity).is_some_and(|entry| !wanted(entry)) {
                    return None;
                }
                let (backend, mut data) = describe(entity, state)?;
                self.stamp(&mut data);
                let checked = Some(checked(state, state_of(backend)));
                Some(Found { backend, ip, mac: None, unit: Some(entity.to_string()), data, checked })
            })
            .collect();
        Ok(Listing { found, warning })
    }

    /// The entity registry, by entity id: what the REST API doesn't say,
    /// such as which integration made an entity and whether it is hidden.
    fn registry(&self) -> Result<HashMap<String, Value>, String> {
        let address = self.host();
        let stream = address
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.find_map(|a| TcpStream::connect_timeout(&a, WAIT).ok()))
            .ok_or_else(|| format!("can't reach Home Assistant at {}", self.url))?;
        stream.set_read_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
        let scheme = if self.secure() { "wss" } else { "ws" };
        let ws_url = format!("{scheme}://{address}/api/websocket");
        let (mut ws, _) = tungstenite::client_tls(ws_url.as_str(), stream).map_err(|e| e.to_string())?;
        read(&mut ws)?; // auth_required
        let auth = json!({"type": "auth", "access_token": self.token});
        ws.send(Message::text(auth.to_string())).map_err(|e| e.to_string())?;
        if read(&mut ws)?.get("type").and_then(Value::as_str) != Some("auth_ok") {
            return Err("Home Assistant refused the token".into());
        }
        let list = json!({"id": 1, "type": "config/entity_registry/list"});
        ws.send(Message::text(list.to_string())).map_err(|e| e.to_string())?;
        let answer = loop {
            let message = read(&mut ws)?;
            if message.get("id").and_then(Value::as_u64) == Some(1) {
                break message;
            }
        };
        let _ = ws.close(None);
        let entries = answer.get("result").and_then(Value::as_array).ok_or("Home Assistant sent no registry")?;
        Ok(entries
            .iter()
            .filter_map(|e| Some((e.get("entity_id")?.as_str()?.to_string(), e.clone())))
            .collect())
    }
}

/// The home's name from `/api/config`, if that is what HA answers there.
fn home_name(config: &Value) -> Option<String> {
    let name = config.get("location_name").and_then(Value::as_str);
    if name.is_none() && config.get("version").is_none() {
        return None;
    }
    Some(name.unwrap_or("Home").to_string())
}

/// What HA's answer means: its JSON, or why not, in its own words when it
/// gives them, as in "Entity … does not support action media_player.volume_set".
/// A success that isn't JSON is something else answering at that address.
fn outcome(code: u16, body: &str) -> Result<Value, String> {
    let json = serde_json::from_str::<Value>(body);
    match code {
        200..=299 => json.map_err(|_| "that address answers, but not as Home Assistant".into()),
        401 => Err("Home Assistant refused the token; make a new one under your HA profile, Security".into()),
        _ => Err(match json.ok().as_ref().and_then(|b| b.get("message")).and_then(Value::as_str) {
            Some(message) => format!("Home Assistant: {message}"),
            None => format!("Home Assistant answered HTTP {code}"),
        }),
    }
}

/// `entity`'s state among those a service call changed.
fn changed(states: &Value, entity: &str) -> Option<Value> {
    states.as_array()?.iter().find(|s| s.get("entity_id").and_then(Value::as_str) == Some(entity)).cloned()
}

/// Whether a device is one of Home Assistant's, and so keeps its URL and token.
pub fn drives(d: &Device) -> bool {
    backend_of(d).is_some_and(|b| b.listed_by_hub())
}

/// The entities a listing has, by backend and entity id.
pub fn listed(listing: &[Found]) -> HashSet<(&'static str, String)> {
    listing.iter().filter_map(|f| Some((f.backend, f.unit.clone()?))).collect()
}

/// The next JSON message on a WebSocket.
fn read(ws: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>) -> Result<Value, String> {
    loop {
        match ws.read().map_err(|e| e.to_string())? {
            Message::Text(t) => return serde_json::from_str(&t).map_err(|e| e.to_string()),
            Message::Close(_) => return Err("Home Assistant closed the connection".into()),
            _ => {}
        }
    }
}

/// Whether a registry entry is something a person would ask for: not
/// hidden or disabled, not a settings or diagnostic entity, and not from
/// an integration home drives itself.
fn wanted(entry: &Value) -> bool {
    let set = |k: &str| entry.get(k).is_some_and(|v| !v.is_null());
    let platform = entry.get("platform").and_then(Value::as_str).unwrap_or_default();
    !set("hidden_by") && !set("disabled_by") && !set("entity_category") && !NATIVE.contains(&platform)
}

fn features(state: &Value) -> u64 {
    state["attributes"].get("supported_features").and_then(Value::as_u64).unwrap_or(0)
}

/// Which backend drives an entity, and what it keeps about it; `None` for
/// one home has no category for.
fn describe(entity: &str, state: &Value) -> Option<(&'static str, Value)> {
    let attrs = state.get("attributes").cloned().unwrap_or_default();
    let name = attrs.get("friendly_name").and_then(Value::as_str).unwrap_or(entity);
    let (domain, _) = entity.split_once('.')?;
    match domain {
        "light" => {
            let modes: Vec<&str> = attrs
                .get("supported_color_modes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let colour = modes.iter().any(|m| matches!(*m, "hs" | "rgb" | "rgbw" | "rgbww" | "xy"));
            let white = modes.contains(&"color_temp");
            // Every mode but "onoff" has a brightness.
            let dims = modes.iter().any(|m| *m != "onoff");
            // HA lists "off" among the effects, as the way to stop one.
            let effects: Vec<&str> = attrs
                .get("effect_list")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let stops = effects.iter().any(|e| is_off(e));
            let effects: Vec<&str> = effects.into_iter().filter(|e| !is_off(e)).collect();
            let flash = features(state) & LIGHT_FLASH != 0;
            Some((
                "ha_light",
                json!({
                    "name": name, "colour": colour, "white": white, "dims": dims,
                    "effects": effects, "stops": stops, "flash": flash,
                }),
            ))
        }
        // A speaker or receiver is a media player too, but not a TV.
        "media_player" if attrs.get("device_class").and_then(Value::as_str) == Some("tv") => {
            Some(("ha_tv", json!({"name": name})))
        }
        "switch" => Some(("ha_switch", json!({"name": name}))),
        _ => None,
    }
}

/// How each backend puts an entity's state into words.
fn state_of(backend: &str) -> fn(&Value) -> String {
    match backend {
        "ha_light" => describe_light,
        "ha_tv" => describe_tv,
        _ => describe_switch,
    }
}

/// Whether an effect is the absence of one.
fn is_off(effect: &str) -> bool {
    matches!(scene_key(effect).as_str(), "off" | "none")
}

fn flag(d: &Device, key: &str) -> bool {
    d.data.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn entity(d: &Device) -> Result<&str, String> {
    d.unit.as_deref().ok_or_else(|| format!("{} names no Home Assistant entity", d.name))
}

/// What an entity is doing, for the Devices panel.
fn checked(state: &Value, describe: fn(&Value) -> String) -> (State, String) {
    if state.get("state").and_then(Value::as_str) == Some("unavailable") {
        (State::Offline, "unavailable in Home Assistant".into())
    } else {
        (State::Ready, describe(state))
    }
}

/// What every HA backend says about the same things.
fn check_entity(d: &Device, describe: fn(&Value) -> String) -> (State, String) {
    match Hub::of(d).and_then(|hub| hub.state(entity(d)?)) {
        Ok(state) => checked(&state, describe),
        Err(e) => (State::Offline, e),
    }
}

/// The name HA gave an entity.
fn called(d: &Device) -> Option<String> {
    d.data.get("name").and_then(Value::as_str).map(str::to_string)
}

// ----- lights ---------------------------------------------------------------

/// Lights Home Assistant drives, as a home backend.
pub struct HaLight;

impl Backend for HaLight {
    fn id(&self) -> &'static str {
        "ha_light"
    }

    fn category(&self) -> Category {
        Category::Light
    }

    fn label(&self) -> &'static str {
        "Home Assistant lights"
    }

    fn listed_by_hub(&self) -> bool {
        true
    }

    fn name(&self, d: &Device) -> Option<String> {
        called(d)
    }

    fn about(&self, d: &Device) -> String {
        // Listed before "dims" was kept: assume it dims, as it used to.
        let dims = d.data.get("dims").and_then(Value::as_bool).unwrap_or(true);
        if dims { self.caps(d).describe().into() } else { "on or off only".into() }
    }

    fn found_detail(&self, d: &Device) -> String {
        format!("Home Assistant · {}", self.about(d))
    }

    fn check(&self, d: &Device) -> (State, String) {
        check_entity(d, describe_light)
    }

    /// Flashes it where HA can, else switches it the other way and back.
    fn test(&self, d: &mut Device) -> Result<String, String> {
        let hub = Hub::of(d)?;
        let entity = entity(d)?;
        let on = hub.state(entity)?.get("state").and_then(Value::as_str) == Some("on");
        if on && flag(d, "flash") {
            hub.call("light.turn_on", entity, json!({"flash": "short"}))?;
            return Ok(format!("{} flashed", d.name));
        }
        let (first, then) = if on { ("light.turn_off", "light.turn_on") } else { ("light.turn_on", "light.turn_off") };
        hub.call(first, entity, json!({}))?;
        thread::sleep(Duration::from_secs(1));
        hub.call(then, entity, json!({}))?;
        Ok(format!("{} blinked", d.name))
    }

    fn test_help(&self) -> &'static str {
        " blinks it so you can find it"
    }

    fn as_light(&self) -> Option<&dyn LightDriver> {
        Some(self)
    }
}

impl LightDriver for HaLight {
    fn caps(&self, d: &Device) -> Caps {
        Caps { colour: flag(d, "colour"), white: flag(d, "white") }
    }

    fn scenes(&self, d: &Device) -> Vec<String> {
        let effects = d.data.get("effects").and_then(Value::as_array);
        effects.into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
    }

    fn set(&self, d: &Device, pilot: &Pilot) -> Result<Option<String>, String> {
        let Some((service, body)) = light_call(pilot, &self.scenes(d), flag(d, "stops"))
            .map_err(|scene| format!("{} has no scene called {scene}", d.name))?
        else {
            return Ok(None);
        };
        let now = Hub::of(d)?.call(service, entity(d)?, body)?;
        Ok(now.as_ref().map(describe_light))
    }

    fn status(&self, d: &Device) -> Result<String, String> {
        let hub = Hub::of(d)?;
        Ok(describe_light(&hub.state(entity(d)?)?))
    }
}

/// The service call that makes a light do what `pilot` asks, given the
/// effects it has and whether "off" stops one; `None` when it asks for
/// nothing, and the scene asked for when the light has no such effect.
/// Asked to switch off, it switches off, whatever else came with it.
fn light_call(pilot: &Pilot, effects: &[String], stops: bool) -> Result<Option<(&'static str, Value)>, String> {
    let changes = pilot.brightness.is_some() || pilot.kelvin.is_some() || pilot.rgb.is_some() || pilot.scene.is_some();
    if pilot.on == Some(false) {
        return Ok(Some(("light.turn_off", json!({}))));
    }
    if pilot.on.is_none() && !changes {
        return Ok(None);
    }
    let mut body = json!({});
    if let Some(b) = pilot.brightness {
        body["brightness_pct"] = json!(b.clamp(1, 100));
    }
    if let Some(k) = pilot.kelvin {
        body["color_temp_kelvin"] = json!(k);
    }
    if let Some((r, g, b)) = pilot.rgb {
        body["rgb_color"] = json!([r, g, b]);
    }
    if let Some(scene) = &pilot.scene {
        let wanted = scene_key(scene);
        // HA wants the effect as the light spells it.
        let effect = effects.iter().find(|e| scene_key(e) == wanted).ok_or_else(|| scene.clone())?;
        body["effect"] = json!(effect);
    } else if (pilot.kelvin.is_some() || pilot.rgb.is_some()) && stops {
        // Or the colour asked for hides behind whatever effect is running.
        body["effect"] = json!("off");
    }
    Ok(Some(("light.turn_on", body)))
}

/// A light's state as "on · 40% · 2700K".
fn describe_light(state: &Value) -> String {
    let on = state.get("state").and_then(Value::as_str) == Some("on");
    if !on {
        return "off".into();
    }
    let attrs = &state["attributes"];
    let mut parts = vec!["on".to_string()];
    if let Some(b) = attrs.get("brightness").and_then(Value::as_f64) {
        parts.push(format!("{}%", (b * 100.0 / 255.0).round() as u8));
    }
    let effect = attrs.get("effect").and_then(Value::as_str).filter(|e| !e.is_empty() && !is_off(e));
    let mode = attrs.get("color_mode").and_then(Value::as_str).unwrap_or_default();
    if let Some(effect) = effect {
        parts.push(effect.to_string());
    } else if mode == "color_temp" {
        if let Some(k) = attrs.get("color_temp_kelvin").and_then(Value::as_u64) {
            parts.push(format!("{k}K"));
        }
    } else if let Some([r, g, b]) = attrs.get("rgb_color").and_then(Value::as_array).map(|c| {
        let c: Vec<u8> = c.iter().filter_map(Value::as_u64).map(|v| v as u8).collect();
        [c.first().copied().unwrap_or(0), c.get(1).copied().unwrap_or(0), c.get(2).copied().unwrap_or(0)]
    }) {
        parts.push(format!("#{r:02x}{g:02x}{b:02x}"));
    }
    parts.join(" · ")
}

// ----- TVs ------------------------------------------------------------------

/// TVs Home Assistant drives, as media players.
pub struct HaTv;

impl Backend for HaTv {
    fn id(&self) -> &'static str {
        "ha_tv"
    }

    fn category(&self) -> Category {
        Category::Tv
    }

    fn label(&self) -> &'static str {
        "Home Assistant TVs"
    }

    fn listed_by_hub(&self) -> bool {
        true
    }

    fn name(&self, d: &Device) -> Option<String> {
        called(d)
    }

    fn about(&self, _d: &Device) -> String {
        "through Home Assistant".into()
    }

    fn found_detail(&self, _d: &Device) -> String {
        "Home Assistant · TV".into()
    }

    fn check(&self, d: &Device) -> (State, String) {
        check_entity(d, describe_tv)
    }

    /// Nothing an HA media player does is a harmless hello, so this only
    /// says what it is doing.
    fn test(&self, d: &mut Device) -> Result<String, String> {
        self.status(d).map(|state| format!("{} is {state}", d.name))
    }

    fn test_help(&self) -> &'static str {
        " shows what it is doing"
    }

    fn as_tv(&self) -> Option<&dyn TvDriver> {
        Some(self)
    }
}

/// How to move a TV's volume by `change` percentage points.
#[derive(Debug, PartialEq)]
enum Volume {
    /// To this level, 0 to 1, in one call.
    Set(f64),
    /// By this many of the integration's own steps, for one that can't be
    /// set to a level.
    Step(u8),
}

/// The same `change` means the same on every TV: points of volume, not
/// whatever one `volume_up` happens to move by.
fn volume_change(state: &Value, change: i8) -> Volume {
    match state["attributes"].get("volume_level").and_then(Value::as_f64) {
        Some(now) if features(state) & VOLUME_SET != 0 => {
            Volume::Set((((now * 100.0).round() + f64::from(change)) / 100.0).clamp(0.0, 1.0))
        }
        _ => Volume::Step(change.unsigned_abs().div_ceil(VOLUME_STEP).max(1)),
    }
}

impl TvDriver for HaTv {
    fn power(&self, d: &mut Device, on: bool) -> Result<String, String> {
        let service = if on { "media_player.turn_on" } else { "media_player.turn_off" };
        Hub::of(d)?.call(service, entity(d)?, json!({}))?;
        Ok(format!("{} is turning {}", d.name, if on { "on" } else { "off" }))
    }

    fn volume(&self, d: &mut Device, level: Option<u8>, change: Option<i8>, mute: Option<bool>) -> Result<String, String> {
        let hub = Hub::of(d)?;
        let entity = entity(d)?;
        if let Some(mute) = mute {
            hub.call("media_player.volume_mute", entity, json!({"is_volume_muted": mute}))?;
        }
        let set = |level: f64| hub.call("media_player.volume_set", entity, json!({"volume_level": level}));
        if let Some(level) = level {
            set(f64::from(level.min(100)) / 100.0)?;
        } else if let Some(change) = change.filter(|c| *c != 0) {
            match volume_change(&hub.state(entity)?, change) {
                Volume::Set(level) => {
                    set(level)?;
                }
                Volume::Step(steps) => {
                    let service = if change > 0 { "media_player.volume_up" } else { "media_player.volume_down" };
                    for _ in 0..steps {
                        hub.call(service, entity, json!({}))?;
                    }
                }
            }
        }
        self.status(d).map(|state| format!("{} is {state}", d.name))
    }

    fn status(&self, d: &mut Device) -> Result<String, String> {
        Ok(describe_tv(&Hub::of(d)?.state(entity(d)?)?))
    }

    /// An app and an input are both a source to HA.
    fn open_app(&self, d: &mut Device, app: &str) -> Result<String, String> {
        self.source(d, app)
    }

    fn input(&self, d: &mut Device, input: &str) -> Result<String, String> {
        self.source(d, input)
    }

    fn media(&self, d: &mut Device, action: &str) -> Result<String, String> {
        let service = match action {
            "play" => "media_player.media_play",
            "pause" => "media_player.media_pause",
            "stop" => "media_player.media_stop",
            _ => return Err(format!("{} can't {action} through Home Assistant", d.name)),
        };
        Hub::of(d)?.call(service, entity(d)?, json!({}))?;
        Ok(format!("{} did {action}", d.name))
    }
}

impl HaTv {
    /// Switches to the source whose name `wanted` is, or is part of.
    fn source(&self, d: &mut Device, wanted: &str) -> Result<String, String> {
        let hub = Hub::of(d)?;
        let entity = entity(d)?;
        let state = hub.state(entity)?;
        let sources: Vec<&str> = state["attributes"]
            .get("source_list")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let Some(source) = pick_source(&sources, wanted) else {
            return Err(if sources.is_empty() {
                format!("{} has no sources to switch to", d.name)
            } else {
                format!("{} has no {wanted}; it has {}", d.name, sources.join(", "))
            });
        };
        hub.call("media_player.select_source", entity, json!({"source": source}))?;
        Ok(format!("{} switched to {source}", d.name))
    }
}

/// The source `wanted` means: the same name, else the one it is part of,
/// ignoring case, spaces and punctuation ("hdmi 1" is "HDMI_1").
fn pick_source<'a>(sources: &[&'a str], wanted: &str) -> Option<&'a str> {
    let key = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let wanted = key(wanted);
    if wanted.is_empty() {
        return None;
    }
    sources
        .iter()
        .find(|s| key(s) == wanted)
        .or_else(|| sources.iter().find(|s| key(s).contains(&wanted)))
        .copied()
}

/// A TV's state as "on · Netflix · volume 20".
fn describe_tv(state: &Value) -> String {
    let power = state.get("state").and_then(Value::as_str).unwrap_or("unknown");
    if matches!(power, "off" | "standby" | "unavailable") {
        return power.into();
    }
    let attrs = &state["attributes"];
    let mut parts = vec![power.to_string()];
    let showing = ["media_title", "app_name", "source"]
        .iter()
        .find_map(|k| attrs.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()));
    if let Some(showing) = showing {
        parts.push(showing.to_string());
    }
    if attrs.get("is_volume_muted").and_then(Value::as_bool) == Some(true) {
        parts.push("muted".into());
    } else if let Some(v) = attrs.get("volume_level").and_then(Value::as_f64) {
        parts.push(format!("volume {}", (v * 100.0).round() as u8));
    }
    parts.join(" · ")
}

// ----- switches -------------------------------------------------------------

/// Plugs and switches Home Assistant drives.
pub struct HaSwitch;

impl Backend for HaSwitch {
    fn id(&self) -> &'static str {
        "ha_switch"
    }

    fn category(&self) -> Category {
        Category::Switch
    }

    fn label(&self) -> &'static str {
        "Home Assistant switches"
    }

    fn listed_by_hub(&self) -> bool {
        true
    }

    fn name(&self, d: &Device) -> Option<String> {
        called(d)
    }

    fn about(&self, _d: &Device) -> String {
        "on or off".into()
    }

    fn found_detail(&self, _d: &Device) -> String {
        "Home Assistant · switch".into()
    }

    fn check(&self, d: &Device) -> (State, String) {
        check_entity(d, describe_switch)
    }

    /// Flipping an unknown plug to find it could switch off the fridge, so
    /// this only says whether it is on.
    fn test(&self, d: &mut Device) -> Result<String, String> {
        SwitchDriver::status(self, d).map(|state| format!("{} is {state}", d.name))
    }

    fn test_help(&self) -> &'static str {
        " shows whether it is on"
    }

    fn as_switch(&self) -> Option<&dyn SwitchDriver> {
        Some(self)
    }
}

impl SwitchDriver for HaSwitch {
    fn set(&self, d: &Device, on: bool) -> Result<Option<String>, String> {
        let service = if on { "switch.turn_on" } else { "switch.turn_off" };
        let now = Hub::of(d)?.call(service, entity(d)?, json!({}))?;
        Ok(now.as_ref().map(describe_switch))
    }

    fn status(&self, d: &Device) -> Result<String, String> {
        Ok(describe_switch(&Hub::of(d)?.state(entity(d)?)?))
    }
}

fn describe_switch(state: &Value) -> String {
    state.get("state").and_then(Value::as_str).unwrap_or("unknown").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_as_typed() {
        assert_eq!(url("homeassistant.local").unwrap(), "http://homeassistant.local:8123");
        assert_eq!(url("http://10.0.0.4:8123/").unwrap(), "http://10.0.0.4:8123");
        assert_eq!(url("10.0.0.4:80").unwrap(), "http://10.0.0.4:80");
        assert_eq!(url("https://ha.example.com").unwrap(), "https://ha.example.com:8123");
        assert_eq!(url("https://ha.example.com:443").unwrap(), "https://ha.example.com:443");
        assert_eq!(url("[fd00::4]").unwrap(), "http://[fd00::4]:8123");
        assert_eq!(url("http://[fd00::4]:8123").unwrap(), "http://[fd00::4]:8123");
        assert!(url("fd00::4").unwrap_err().contains("brackets"));
        assert!(url("ftp://10.0.0.4").is_err());
        assert!(url("http://10.0.0.4:8123/lovelace").is_err());
    }

    #[test]
    fn hosts_without_their_scheme() {
        let hub = |url: &str| Hub { url: url.into(), token: "t".into() };
        assert_eq!(hub("http://10.0.0.4:8123").host(), "10.0.0.4:8123");
        assert_eq!(hub("https://ha.lan:8123").host(), "ha.lan:8123");
        assert!(hub("https://ha.lan:8123").secure() && !hub("http://ha.lan:8123").secure());
    }

    #[test]
    fn a_hub_never_prints_its_token() {
        let hub = Hub { url: "http://10.0.0.4:8123".into(), token: "eyJsecret".into() };
        let printed = format!("{hub:?}");
        assert!(!printed.contains("eyJsecret") && printed.contains("10.0.0.4"), "{printed}");
    }

    #[test]
    fn registry_leaves_out_what_nobody_asks_for() {
        assert!(wanted(&json!({"platform": "hue", "hidden_by": null, "entity_category": null})));
        assert!(!wanted(&json!({"platform": "hue", "hidden_by": "user"})));
        assert!(!wanted(&json!({"platform": "hue", "disabled_by": "integration"})));
        assert!(!wanted(&json!({"platform": "hue", "entity_category": "config"})));
        assert!(!wanted(&json!({"platform": "wiz"})), "a Wiz bulb is driven by home directly");
    }

    #[test]
    fn lights_say_what_they_can_do() {
        let state = json!({"entity_id": "light.strip", "state": "on", "attributes": {
            "friendly_name": "Strip", "supported_color_modes": ["color_temp", "hs"],
            "effect_list": ["Rainbow", "Night-Light", "off"], "supported_features": 44,
        }});
        let (backend, data) = describe("light.strip", &state).unwrap();
        assert_eq!(backend, "ha_light");
        assert_eq!(
            data,
            json!({"name": "Strip", "colour": true, "white": true, "dims": true,
                   "effects": ["Rainbow", "Night-Light"], "stops": true, "flash": true})
        );
        let dimmer = json!({"state": "off", "attributes": {"supported_color_modes": ["brightness"]}});
        let (_, data) = describe("light.hall", &dimmer).unwrap();
        assert_eq!((data["name"].as_str(), data["colour"].as_bool(), data["white"].as_bool()), (Some("light.hall"), Some(false), Some(false)));
        assert_eq!((data["dims"].as_bool(), data["stops"].as_bool(), data["flash"].as_bool()), (Some(true), Some(false), Some(false)));
        assert!(describe("sensor.temperature", &state).is_none());
    }

    #[test]
    fn an_on_off_light_says_it_only_switches() {
        let state = json!({"state": "off", "attributes": {"supported_color_modes": ["onoff"]}});
        let (backend, data) = describe("light.porch", &state).unwrap();
        let d = Device::new(backend, "Porch".into(), Ipv4Addr::UNSPECIFIED, None, data);
        assert_eq!(HaLight.about(&d), "on or off only");
        let older = Device::new("ha_light", "Hall".into(), Ipv4Addr::UNSPECIFIED, None, json!({}));
        assert_eq!(HaLight.about(&older), "dimmable", "listed before dims was kept");
    }

    #[test]
    fn listings_carry_each_entitys_state() {
        let off = json!({"state": "off", "attributes": {}});
        assert_eq!(checked(&off, state_of("ha_light")), (State::Ready, "off".into()));
        let gone = json!({"state": "unavailable", "attributes": {}});
        assert_eq!(checked(&gone, state_of("ha_switch")).0, State::Offline);
        let tv = json!({"state": "playing", "attributes": {"source": "Netflix"}});
        assert_eq!(checked(&tv, state_of("ha_tv")).1, "playing · Netflix");
    }

    #[test]
    fn light_states_read_like_wiz() {
        let warm = json!({"state": "on", "attributes": {"brightness": 102, "color_mode": "color_temp", "color_temp_kelvin": 2700}});
        assert_eq!(describe_light(&warm), "on · 40% · 2700K");
        let red = json!({"state": "on", "attributes": {"brightness": 255, "color_mode": "hs", "rgb_color": [255, 0, 0]}});
        assert_eq!(describe_light(&red), "on · 100% · #ff0000");
        let effect = json!({"state": "on", "attributes": {"color_mode": "hs", "effect": "Rainbow"}});
        assert_eq!(describe_light(&effect), "on · Rainbow");
        let stopped = json!({"state": "on", "attributes": {"color_mode": "color_temp", "effect": "off", "color_temp_kelvin": 3000}});
        assert_eq!(describe_light(&stopped), "on · 3000K");
        assert_eq!(describe_light(&json!({"state": "off", "attributes": {}})), "off");
    }

    #[test]
    fn only_tvs_are_tvs() {
        let tv = json!({"attributes": {"friendly_name": "Lounge TV", "device_class": "tv"}});
        assert_eq!(describe("media_player.lounge", &tv), Some(("ha_tv", json!({"name": "Lounge TV"}))));
        let speaker = json!({"attributes": {"device_class": "speaker"}});
        assert!(describe("media_player.kitchen", &speaker).is_none());
        let plug = json!({"attributes": {"friendly_name": "Kettle"}});
        assert_eq!(describe("switch.kettle", &plug), Some(("ha_switch", json!({"name": "Kettle"}))));
    }

    #[test]
    fn answers_say_why_not() {
        assert_eq!(outcome(200, "[]"), Ok(json!([])));
        assert!(outcome(401, "401: Unauthorized").unwrap_err().contains("refused the token"));
        let refused = r#"{"message": "Entity media_player.tv does not support action media_player.volume_set"}"#;
        assert_eq!(outcome(400, refused).unwrap_err(), "Home Assistant: Entity media_player.tv does not support action media_player.volume_set");
        assert_eq!(outcome(500, "").unwrap_err(), "Home Assistant answered HTTP 500");
    }

    #[test]
    fn something_else_answering_isnt_home_assistant() {
        assert!(outcome(200, "<html>Sign in</html>").unwrap_err().contains("not as Home Assistant"));
        assert_eq!(home_name(&json!({"location_name": "Flat", "version": "2026.10.1"})), Some("Flat".into()));
        assert_eq!(home_name(&json!({"version": "2026.10.1"})), Some("Home".into()));
        assert_eq!(home_name(&json!({"status": "ok"})), None);
        assert_eq!(home_name(&Value::Null), None);
    }

    #[test]
    fn a_call_reports_the_state_it_changed() {
        let states = json!([{"entity_id": "light.other", "state": "off"}, {"entity_id": "switch.kettle", "state": "on"}]);
        assert_eq!(changed(&states, "switch.kettle").map(|s| describe_switch(&s)), Some("on".into()));
        assert_eq!(changed(&states, "switch.fridge"), None, "not changed yet: ask instead");
        assert_eq!(changed(&Value::Null, "switch.kettle"), None);
    }

    #[test]
    fn light_calls() {
        let effects = vec!["Rainbow".to_string(), "Night-Light".to_string()];
        let call = |pilot: Pilot, stops: bool| light_call(&pilot, &effects, stops);
        assert_eq!(call(Pilot::default(), false), Ok(None), "nothing asked, nothing sent");
        assert_eq!(call(Pilot { on: Some(false), ..Default::default() }, false), Ok(Some(("light.turn_off", json!({})))));
        assert_eq!(
            call(Pilot { on: Some(false), brightness: Some(30), ..Default::default() }, false),
            Ok(Some(("light.turn_off", json!({})))),
            "off means off, as it does on a Wiz",
        );
        assert_eq!(call(Pilot { on: Some(true), ..Default::default() }, false), Ok(Some(("light.turn_on", json!({})))));
        assert_eq!(
            call(Pilot { scene: Some("night light".into()), ..Default::default() }, true),
            Ok(Some(("light.turn_on", json!({"effect": "Night-Light"})))),
            "the effect as the light spells it",
        );
        assert_eq!(call(Pilot { scene: Some("fire".into()), ..Default::default() }, true), Err("fire".into()));
        assert_eq!(
            call(Pilot { rgb: Some((255, 0, 0)), brightness: Some(0), ..Default::default() }, true),
            Ok(Some(("light.turn_on", json!({"rgb_color": [255, 0, 0], "brightness_pct": 1, "effect": "off"})))),
            "a colour stops a running effect",
        );
        assert_eq!(
            call(Pilot { kelvin: Some(2700), ..Default::default() }, false),
            Ok(Some(("light.turn_on", json!({"color_temp_kelvin": 2700})))),
            "no \"off\" to send when the light has none",
        );
    }

    #[test]
    fn volume_moves_by_points_on_every_tv() {
        let tv = |level: f64, features: u64| json!({"attributes": {"volume_level": level, "supported_features": features}});
        assert_eq!(volume_change(&tv(0.2, VOLUME_SET), -3), Volume::Set(0.17));
        assert_eq!(volume_change(&tv(0.98, VOLUME_SET | 8), 5), Volume::Set(1.0));
        assert_eq!(volume_change(&tv(0.01, VOLUME_SET), -5), Volume::Set(0.0));
        // Without a level to set, HA's own steps, a tenth each.
        assert_eq!(volume_change(&tv(0.2, 0), -3), Volume::Step(1));
        assert_eq!(volume_change(&tv(0.2, 0), 15), Volume::Step(2));
        assert_eq!(volume_change(&json!({"attributes": {"supported_features": VOLUME_SET}}), 1), Volume::Step(1));
    }

    #[test]
    fn which_devices_are_home_assistants() {
        let d = |backend: &str| Device::new(backend, "x".into(), Ipv4Addr::UNSPECIFIED, None, Value::Null);
        assert!(drives(&d("ha_light")) && drives(&d("ha_tv")) && drives(&d("ha_switch")));
        assert!(!drives(&d("wiz")) && !drives(&d("webos")) && !drives(&d("ha_something_newer")));
    }

    #[test]
    fn sources_by_what_people_say() {
        let sources = ["HDMI_1", "HDMI 2", "Netflix", "YouTube TV"];
        assert_eq!(pick_source(&sources, "hdmi 1"), Some("HDMI_1"));
        assert_eq!(pick_source(&sources, "netflix"), Some("Netflix"));
        assert_eq!(pick_source(&sources, "youtube"), Some("YouTube TV"));
        assert_eq!(pick_source(&sources, "disney+"), None);
        assert_eq!(pick_source(&sources, "+"), None);
    }

    #[test]
    fn tv_states() {
        let on = json!({"state": "playing", "attributes": {"source": "Netflix", "volume_level": 0.2, "is_volume_muted": false}});
        assert_eq!(describe_tv(&on), "playing · Netflix · volume 20");
        let muted = json!({"state": "on", "attributes": {"app_name": "YouTube", "is_volume_muted": true}});
        assert_eq!(describe_tv(&muted), "on · YouTube · muted");
        assert_eq!(describe_tv(&json!({"state": "off", "attributes": {}})), "off");
    }

    /// Lists and drives a real Home Assistant. `HA_URL=… HA_TOKEN=… cargo
    /// test -- --ignored live`; HA's `demo:` integration has everything it
    /// drives.
    #[test]
    #[ignore = "needs a Home Assistant with the demo integration"]
    fn live() {
        let hub = Hub { url: url(&std::env::var("HA_URL").unwrap()).unwrap(), token: std::env::var("HA_TOKEN").unwrap() };
        println!("home: {}", hub.check().unwrap());
        let listing = hub.entities().unwrap();
        assert_eq!(listing.warning, None, "the registry was read");
        let found = listing.found;
        let device = |entity: &str| {
            let f = found.iter().find(|f| f.unit.as_deref() == Some(entity)).unwrap_or_else(|| panic!("{entity} not listed"));
            assert!(f.checked.is_some(), "{entity} is listed with its state");
            Device { unit: f.unit.clone(), ..Device::new(f.backend, entity.into(), f.ip, None, f.data.clone()) }
        };
        let units: Vec<&str> = found.iter().filter_map(|f| f.unit.as_deref()).collect();
        println!("listed: {units:?}");
        assert!(!units.contains(&"media_player.walkman"), "only TVs are TVs");

        let mut bed = device("light.bed_light");
        assert_eq!(HaLight.caps(&bed), Caps { colour: true, white: true });
        HaLight.set(&bed, &Pilot { brightness: Some(40), kelvin: Some(2700), ..Default::default() }).unwrap();
        assert_eq!(HaLight.status(&bed).unwrap(), "on · 40% · 2700K");
        HaLight.set(&bed, &Pilot { rgb: Some((255, 0, 0)), ..Default::default() }).unwrap();
        assert!(HaLight.status(&bed).unwrap().ends_with("#ff0000"), "{}", HaLight.status(&bed).unwrap());
        HaLight.set(&bed, &Pilot { on: Some(false), ..Default::default() }).unwrap();
        assert_eq!(HaLight.check(&bed), (State::Ready, "off".into()));
        HaLight.test(&mut bed).unwrap();
        assert_eq!(HaLight.status(&bed).unwrap(), "off", "testing an off light leaves it off");

        let mut tv = device("media_player.lounge_room");
        assert_eq!(HaTv.open_app(&mut tv, "YouTube").unwrap(), "media_player.lounge_room switched to youtube");
        assert!(HaTv.open_app(&mut tv, "netflix").unwrap_err().contains("it has dvd, youtube"));
        // The demo TV has no volume control, and says so through HA.
        let volume = HaTv.volume(&mut tv, Some(30), None, None).unwrap_err();
        assert!(volume.contains("does not support action media_player.volume_set"), "{volume}");
        assert!(HaTv.status(&mut tv).unwrap().contains("Chapter 1"));
        HaTv.power(&mut tv, false).unwrap();
        assert_eq!(HaTv.status(&mut tv).unwrap(), "off");
        HaTv.power(&mut tv, true).unwrap();

        let ac = device("switch.ac");
        SwitchDriver::set(&HaSwitch, &ac, true).unwrap();
        assert_eq!(SwitchDriver::status(&HaSwitch, &ac).unwrap(), "on");
        SwitchDriver::set(&HaSwitch, &ac, false).unwrap();
        assert_eq!(SwitchDriver::status(&HaSwitch, &ac).unwrap(), "off");

        let wrong = Hub { token: "nope".into(), ..hub.clone() };
        assert!(wrong.check().unwrap_err().contains("refused the token"));
        assert_eq!(hub.state("light.nope").unwrap_err(), "Home Assistant has no light.nope");
    }
}
