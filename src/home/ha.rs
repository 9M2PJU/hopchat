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
//! Only plain `http://` for now: HA on a LAN usually answers there, and
//! hopchat's HTTP client is built without TLS.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

use super::driver::{Backend, Category, Found};
use super::light::{scene_key, Caps, LightDriver, Pilot};
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

/// A Home Assistant to talk to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hub {
    pub url: String,
    pub token: String,
}

/// The `http://host:port` someone typed, with the scheme and port filled
/// in when left out.
pub fn url(typed: &str) -> Result<String, String> {
    let typed = typed.trim().trim_end_matches('/');
    if typed.starts_with("https://") {
        return Err("use Home Assistant's http:// address; hopchat can't speak https to it yet".into());
    }
    let rest = typed.strip_prefix("http://").unwrap_or(typed);
    if rest.is_empty() || rest.contains('/') {
        return Err(format!("{typed} isn't an address like http://homeassistant.local:8123"));
    }
    let has_port = rest.rsplit_once(':').is_some_and(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    Ok(if has_port { format!("http://{rest}") } else { format!("http://{rest}:{PORT}") })
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

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(WAIT))
            .http_status_as_error(true)
            .build()
            .new_agent()
    }

    fn error(&self, e: ureq::Error) -> String {
        match e {
            ureq::Error::StatusCode(401) => {
                "Home Assistant refused the token; make a new one under your HA profile, Security".into()
            }
            ureq::Error::StatusCode(code) => format!("Home Assistant answered HTTP {code}"),
            ureq::Error::Timeout(_) => "Home Assistant took too long to answer".into(),
            _ => format!("can't reach Home Assistant at {}", self.url),
        }
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let mut response = Self::agent()
            .get(format!("{}{path}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .call()
            .map_err(|e| self.error(e))?;
        response.body_mut().read_json().map_err(|e| e.to_string())
    }

    /// Calls a service, such as `light.turn_on`, on one entity.
    fn call(&self, service: &str, entity: &str, mut body: Value) -> Result<(), String> {
        let (domain, service) = service.split_once('.').unwrap_or((service, ""));
        body["entity_id"] = json!(entity);
        Self::agent()
            .post(format!("{}/api/services/{domain}/{service}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .send_json(&body)
            .map_err(|e| self.error(e))?;
        Ok(())
    }

    /// One entity's state and attributes.
    fn state(&self, entity: &str) -> Result<Value, String> {
        self.get(&format!("/api/states/{entity}")).map_err(|e| {
            if e.ends_with("HTTP 404") { format!("Home Assistant has no {entity}") } else { e }
        })
    }

    /// The name of the home it runs, as a check that the URL and token work.
    pub fn check(&self) -> Result<String, String> {
        let config = self.get("/api/config")?;
        Ok(config.get("location_name").and_then(Value::as_str).unwrap_or("Home").to_string())
    }

    /// The address devices from it are listed at in the panel.
    fn ip(&self) -> Ipv4Addr {
        let host = self.url.trim_start_matches("http://");
        host.to_socket_addrs()
            .ok()
            .and_then(|mut addrs| {
                addrs.find_map(|a| match a {
                    SocketAddr::V4(v4) => Some(*v4.ip()),
                    SocketAddr::V6(_) => None,
                })
            })
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    /// Every entity home can drive, as found devices.
    pub fn entities(&self) -> Result<Vec<Found>, String> {
        let states = self.get("/api/states")?;
        // Without the registry, nothing is filtered out; still worth listing.
        let registry = self.registry().unwrap_or_default();
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
                Some(Found { backend, ip, mac: None, unit: Some(entity.to_string()), data })
            })
            .collect();
        Ok(found)
    }

    /// The entity registry, by entity id: what the REST API doesn't say,
    /// such as which integration made an entity and whether it is hidden.
    fn registry(&self) -> Result<HashMap<String, Value>, String> {
        let address = self.url.trim_start_matches("http://");
        let stream = address
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.find_map(|a| TcpStream::connect_timeout(&a, WAIT).ok()))
            .ok_or_else(|| format!("can't reach Home Assistant at {}", self.url))?;
        stream.set_read_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
        let ws_url = format!("ws://{address}/api/websocket");
        let (mut ws, _) =
            tungstenite::client(ws_url.as_str(), MaybeTlsStream::Plain(stream)).map_err(|e| e.to_string())?;
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
            let effects = attrs.get("effect_list").cloned().unwrap_or(json!([]));
            Some(("ha_light", json!({"name": name, "colour": colour, "white": white, "effects": effects})))
        }
        _ => None,
    }
}

fn flag(d: &Device, key: &str) -> bool {
    d.data.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn entity(d: &Device) -> Result<&str, String> {
    d.unit.as_deref().ok_or_else(|| format!("{} names no Home Assistant entity", d.name))
}

/// What every HA backend says about the same things.
fn check_entity(d: &Device, describe: fn(&Value) -> String) -> (State, String) {
    let state = Hub::of(d).and_then(|hub| hub.state(entity(d)?));
    match state {
        Ok(s) if s.get("state").and_then(Value::as_str) == Some("unavailable") => {
            (State::Offline, "unavailable in Home Assistant".into())
        }
        Ok(s) => (State::Ready, describe(&s)),
        Err(e) => (State::Offline, e),
    }
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

    /// Listed by home from the hub it connected to, not found by looking.
    fn discover(&self, _each: &dyn Fn(Found)) {}

    fn probe(&self, _ip: Ipv4Addr) -> Option<Found> {
        None
    }

    fn name(&self, d: &Device) -> Option<String> {
        d.data.get("name").and_then(Value::as_str).map(str::to_string)
    }

    fn about(&self, d: &Device) -> String {
        self.caps(d).describe().into()
    }

    fn found_detail(&self, d: &Device) -> String {
        format!("Home Assistant · {}", self.about(d))
    }

    fn check(&self, d: &Device) -> (State, String) {
        check_entity(d, describe_light)
    }

    fn test(&self, d: &mut Device) -> Result<String, String> {
        let hub = Hub::of(d)?;
        let entity = entity(d)?;
        let on = hub.state(entity)?.get("state").and_then(Value::as_str) == Some("on");
        if on {
            hub.call("light.turn_on", entity, json!({"flash": "short"}))?;
        } else {
            hub.call("light.turn_on", entity, json!({}))?;
            thread::sleep(Duration::from_secs(1));
            hub.call("light.turn_off", entity, json!({}))?;
        }
        Ok(format!("{} flashed", d.name))
    }

    fn test_help(&self) -> &'static str {
        " flashes it so you can find it"
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

    fn set(&self, d: &Device, pilot: &Pilot) -> Result<(), String> {
        let hub = Hub::of(d)?;
        let entity = entity(d)?;
        let changes = pilot.brightness.is_some() || pilot.kelvin.is_some() || pilot.rgb.is_some() || pilot.scene.is_some();
        if pilot.on == Some(false) && !changes {
            return hub.call("light.turn_off", entity, json!({}));
        }
        if pilot.on.is_none() && !changes {
            return Ok(());
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
            let Some(effect) = self.scenes(d).into_iter().find(|e| scene_key(e) == wanted) else {
                return Err(format!("{} has no scene called {scene}", d.name));
            };
            body["effect"] = json!(effect);
        }
        hub.call("light.turn_on", entity, body)
    }

    fn status(&self, d: &Device) -> Result<String, String> {
        let hub = Hub::of(d)?;
        Ok(describe_light(&hub.state(entity(d)?)?))
    }
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
    let effect = attrs.get("effect").and_then(Value::as_str).filter(|e| !e.is_empty() && *e != "none");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_as_typed() {
        assert_eq!(url("homeassistant.local").unwrap(), "http://homeassistant.local:8123");
        assert_eq!(url("http://10.0.0.4:8123/").unwrap(), "http://10.0.0.4:8123");
        assert_eq!(url("10.0.0.4:80").unwrap(), "http://10.0.0.4:80");
        assert!(url("https://ha.example.com").is_err());
        assert!(url("http://10.0.0.4:8123/lovelace").is_err());
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
            "effect_list": ["Rainbow", "Night-Light"],
        }});
        let (backend, data) = describe("light.strip", &state).unwrap();
        assert_eq!(backend, "ha_light");
        assert_eq!(data, json!({"name": "Strip", "colour": true, "white": true, "effects": ["Rainbow", "Night-Light"]}));
        let dimmer = json!({"state": "off", "attributes": {"supported_color_modes": ["brightness"]}});
        let (_, data) = describe("light.hall", &dimmer).unwrap();
        assert_eq!((data["name"].as_str(), data["colour"].as_bool(), data["white"].as_bool()), (Some("light.hall"), Some(false), Some(false)));
        assert!(describe("sensor.temperature", &state).is_none());
    }

    #[test]
    fn light_states_read_like_wiz() {
        let warm = json!({"state": "on", "attributes": {"brightness": 102, "color_mode": "color_temp", "color_temp_kelvin": 2700}});
        assert_eq!(describe_light(&warm), "on · 40% · 2700K");
        let red = json!({"state": "on", "attributes": {"brightness": 255, "color_mode": "hs", "rgb_color": [255, 0, 0]}});
        assert_eq!(describe_light(&red), "on · 100% · #ff0000");
        let effect = json!({"state": "on", "attributes": {"color_mode": "hs", "effect": "Rainbow"}});
        assert_eq!(describe_light(&effect), "on · Rainbow");
        assert_eq!(describe_light(&json!({"state": "off", "attributes": {}})), "off");
    }
}
