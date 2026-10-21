//! Wiz bulbs: JSON datagrams on UDP 38899, no pairing, LAN only.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const PORT: u16 = 38899;
const WAIT: Duration = Duration::from_millis(1500);

/// What a bulb should change to; anything left `None` stays as it is.
#[derive(Default, Debug, PartialEq)]
pub struct Pilot {
    pub on: Option<bool>,
    /// 10–100 %.
    pub brightness: Option<u8>,
    /// 2200–6500 K.
    pub kelvin: Option<u16>,
    pub rgb: Option<(u8, u8, u8)>,
    pub scene: Option<u8>,
}

impl Pilot {
    /// The `setPilot` params, clamped to what the bulb accepts. Colour wins
    /// over white temperature, which wins over a scene: the bulb holds one.
    fn params(&self) -> Value {
        let mut p = serde_json::Map::new();
        if let Some(on) = self.on {
            p.insert("state".into(), json!(on));
        }
        if let Some(b) = self.brightness {
            p.insert("dimming".into(), json!(b.clamp(10, 100)));
        }
        if let Some((r, g, b)) = self.rgb {
            p.insert("r".into(), json!(r));
            p.insert("g".into(), json!(g));
            p.insert("b".into(), json!(b));
        } else if let Some(k) = self.kelvin {
            p.insert("temp".into(), json!(k.clamp(2200, 6500)));
        } else if let Some(scene) = self.scene {
            p.insert("sceneId".into(), json!(scene.clamp(1, 32)));
        }
        // Changing anything but power implies the light should be on.
        if !p.is_empty() && !p.contains_key("state") {
            p.insert("state".into(), json!(true));
        }
        Value::Object(p)
    }
}

/// Wiz's built-in scenes, by the names people say.
pub const SCENES: &[(&str, u8)] = &[
    ("ocean", 1), ("romance", 2), ("sunset", 3), ("party", 4), ("fireplace", 5),
    ("cozy", 6), ("forest", 7), ("pastel", 8), ("wake up", 9), ("bedtime", 10),
    ("warm white", 11), ("daylight", 12), ("cool white", 13), ("night light", 14),
    ("focus", 15), ("relax", 16), ("true colors", 17), ("tv time", 18),
    ("plant growth", 19), ("spring", 20), ("summer", 21), ("fall", 22),
    ("deep dive", 23), ("jungle", 24), ("mojito", 25), ("club", 26),
    ("christmas", 27), ("halloween", 28), ("candlelight", 29), ("golden white", 30),
    ("pulse", 31), ("steampunk", 32),
];

pub fn scene_id(name: &str) -> Option<u8> {
    let name = name.trim().to_ascii_lowercase().replace(['_', '-'], " ");
    SCENES.iter().find(|(n, _)| *n == name).map(|(_, id)| *id)
}

fn scene_name(id: u64) -> Option<&'static str> {
    SCENES.iter().find(|(_, i)| u64::from(*i) == id).map(|(n, _)| *n)
}

/// What a bulb can show, from the module name it reports: `SHRGB` bulbs do
/// colour and white tones, `SHTW` white tones, `SHDW` brightness only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Caps {
    pub colour: bool,
    pub white: bool,
}

impl Caps {
    pub fn of(module: &str) -> Self {
        let m = module.to_ascii_uppercase();
        if m.contains("RGB") {
            Self { colour: true, white: true }
        } else if m.contains("TW") {
            Self { colour: false, white: true }
        } else if m.contains("DW") {
            Self::default()
        } else {
            // Unknown module: let it try everything rather than refuse.
            Self { colour: true, white: true }
        }
    }

    pub fn describe(&self) -> &'static str {
        match (self.colour, self.white) {
            (true, _) => "colour",
            (false, true) => "white tones",
            (false, false) => "dimmable",
        }
    }
}

/// A bulb's identity, as it reports it.
pub struct System {
    pub mac: String,
    pub module: String,
}

pub struct Bulb {
    pub ip: Ipv4Addr,
}

impl Bulb {
    pub fn set(&self, pilot: &Pilot) -> Result<(), String> {
        let params = pilot.params();
        if params.as_object().is_none_or(|p| p.is_empty()) {
            return Err("nothing to change".into());
        }
        self.set_raw(params)
    }

    fn set_raw(&self, params: Value) -> Result<(), String> {
        let answer = self.call(json!({"method": "setPilot", "params": params}))?;
        match answer.pointer("/result/success") {
            Some(Value::Bool(true)) => Ok(()),
            _ => Err(format!("bulb refused: {answer}")),
        }
    }

    fn pilot(&self) -> Result<Value, String> {
        let answer = self.call(json!({"method": "getPilot", "params": {}}))?;
        answer.get("result").cloned().ok_or_else(|| "bulb sent no state".into())
    }

    pub fn system(&self) -> Result<System, String> {
        let answer = self.call(json!({"method": "getSystemConfig", "params": {}}))?;
        let r = answer.get("result").ok_or("bulb sent no config")?;
        Ok(System {
            mac: r.get("mac").and_then(Value::as_str).unwrap_or_default().to_string(),
            module: r.get("moduleName").and_then(Value::as_str).unwrap_or_default().to_string(),
        })
    }

    /// A one-line description of what the bulb is doing now.
    pub fn status(&self) -> Result<String, String> {
        Ok(describe(&self.pilot()?))
    }

    /// Dips the light twice and puts it back as it was, so you can tell
    /// which bulb is which.
    pub fn blink(&self) -> Result<(), String> {
        let before = self.pilot()?;
        let was_on = before.get("state") != Some(&Value::Bool(false));
        let mut blinked = Ok(());
        for dimming in [10, 100, 10, 100] {
            blinked = self.set_raw(json!({"state": true, "dimming": dimming}));
            if blinked.is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(350));
        }
        // Put it back even if a blink got lost.
        let restored = self.set_raw(restore(&before, was_on));
        blinked.and(restored)
    }

    fn call(&self, request: Value) -> Result<Value, String> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
        socket.set_read_timeout(Some(Duration::from_millis(500))).map_err(|e| e.to_string())?;
        let target = SocketAddr::from((self.ip, PORT));
        let body = request.to_string();
        let deadline = Instant::now() + WAIT;
        let mut buf = [0u8; 2048];
        // UDP: resend until it answers or we give up.
        while Instant::now() < deadline {
            socket.send_to(body.as_bytes(), target).map_err(|e| e.to_string())?;
            if let Ok((n, from)) = socket.recv_from(&mut buf)
                && from == target
            {
                return serde_json::from_slice(&buf[..n]).map_err(|e| e.to_string());
            }
        }
        Err(format!("the light at {} did not answer", self.ip))
    }
}

fn describe(r: &Value) -> String {
    if r.get("state") == Some(&Value::Bool(false)) {
        return "off".into();
    }
    let mut out = format!("on · {}%", r.get("dimming").and_then(Value::as_u64).unwrap_or(100));
    if let Some(scene) = r.get("sceneId").and_then(Value::as_u64).and_then(scene_name) {
        out.push_str(&format!(" · {scene}"));
    } else if let Some(k) = r.get("temp").and_then(Value::as_u64).filter(|k| *k > 0) {
        out.push_str(&format!(" · {k}K"));
    } else if let (Some(red), Some(g), Some(b)) = (
        r.get("r").and_then(Value::as_u64),
        r.get("g").and_then(Value::as_u64),
        r.get("b").and_then(Value::as_u64),
    ) {
        out.push_str(&format!(" · #{red:02x}{g:02x}{b:02x}"));
    }
    out
}

/// The `setPilot` params that put a bulb back to a `getPilot` state.
fn restore(before: &Value, was_on: bool) -> Value {
    if !was_on {
        return json!({"state": false});
    }
    let mut p = serde_json::Map::new();
    p.insert("state".into(), json!(true));
    if let Some(d) = before.get("dimming") {
        p.insert("dimming".into(), d.clone());
    }
    match before.get("sceneId").and_then(Value::as_u64) {
        Some(id) if id > 0 => {
            p.insert("sceneId".into(), json!(id));
        }
        _ => {
            if let Some(v) = before.get("temp").filter(|v| v.as_u64().is_some_and(|n| n > 0)) {
                p.insert("temp".into(), v.clone());
            } else if ["r", "g", "b"].iter().any(|k| before.get(*k).is_some()) {
                // A colour needs all three channels back: pure red has g and b at 0.
                for key in ["r", "g", "b"] {
                    p.insert(key.into(), before.get(key).cloned().unwrap_or(json!(0)));
                }
            }
        }
    }
    Value::Object(p)
}

/// Every Wiz bulb that answers a broadcast within `wait`, with the MAC it
/// reported.
pub fn discover(wait: Duration) -> Vec<(Ipv4Addr, Option<String>)> {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else { return Vec::new() };
    let _ = socket.set_broadcast(true);
    let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
    let probe = json!({"method": "getPilot", "params": {}}).to_string();
    let deadline = Instant::now() + wait;
    let mut found: Vec<(Ipv4Addr, Option<String>)> = Vec::new();
    let mut buf = [0u8; 2048];
    let mut next_probe = Instant::now();
    while Instant::now() < deadline {
        // Broadcasts get lost; ask a few times.
        if Instant::now() >= next_probe {
            let _ = socket.send_to(probe.as_bytes(), (Ipv4Addr::BROADCAST, PORT));
            next_probe = Instant::now() + Duration::from_millis(700);
        }
        if let Ok((n, SocketAddr::V4(from))) = socket.recv_from(&mut buf)
            && !found.iter().any(|(ip, _)| ip == from.ip())
        {
            let mac = serde_json::from_slice::<Value>(&buf[..n])
                .ok()
                .and_then(|v| v.pointer("/result/mac")?.as_str().map(str::to_string));
            found.push((*from.ip(), mac));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pilot_is_clamped_and_implies_on() {
        let p = Pilot { brightness: Some(3), kelvin: Some(9000), ..Default::default() };
        assert_eq!(p.params(), json!({"dimming": 10, "temp": 6500, "state": true}));
        let off = Pilot { on: Some(false), ..Default::default() };
        assert_eq!(off.params(), json!({"state": false}));
        let both = Pilot { rgb: Some((255, 0, 0)), kelvin: Some(2700), ..Default::default() };
        assert_eq!(both.params(), json!({"r": 255, "g": 0, "b": 0, "state": true}));
    }

    #[test]
    fn capabilities_from_module_names() {
        assert_eq!(Caps::of("ESP01_SHTW1C_31"), Caps { colour: false, white: true });
        assert_eq!(Caps::of("ESP01_SHRGB1C_31"), Caps { colour: true, white: true });
        assert_eq!(Caps::of("ESP06_SHDW9_01"), Caps { colour: false, white: false });
    }

    #[test]
    fn restores_what_it_found() {
        let scene = json!({"state": true, "sceneId": 15, "dimming": 80, "temp": 0});
        assert_eq!(restore(&scene, true), json!({"state": true, "dimming": 80, "sceneId": 15}));
        let white = json!({"state": true, "sceneId": 0, "dimming": 40, "temp": 2700});
        assert_eq!(restore(&white, true), json!({"state": true, "dimming": 40, "temp": 2700}));
        assert_eq!(restore(&white, false), json!({"state": false}));
        assert_eq!(describe(&scene), "on · 80% · focus");
        assert_eq!(describe(&white), "on · 40% · 2700K");
    }

    #[test]
    fn scenes_by_spoken_name() {
        assert_eq!(scene_id("Night-Light"), Some(14));
        assert_eq!(scene_id("tv_time"), Some(18));
        assert_eq!(scene_id("disco"), None);
    }
}
