//! LG webOS TVs: the "second screen" WebSocket API, over TLS on port 3001
//! (current firmware) or plain on 3000 (older sets). The first connection
//! shows a prompt on the TV; accepting it yields a client key that later
//! connections present instead.
//!
//! The TV's certificate is self-signed, so it cannot be checked against a
//! CA. Instead its fingerprint is pinned when the TV pairs, and a later
//! connection presenting a different certificate is refused.

use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Connector, Message, WebSocket};

const TLS_PORT: u16 = 3001;
const PLAIN_PORT: u16 = 3000;
const CONNECT: Duration = Duration::from_secs(3);
const ANSWER: Duration = Duration::from_secs(5);
/// How long someone has to walk over and press OK on the TV.
const PAIRING: Duration = Duration::from_secs(60);

/// Spoken app names to webOS app ids; anything else is looked up among the
/// TV's installed apps by title.
const APPS: &[(&str, &str)] = &[
    ("netflix", "netflix"),
    ("youtube", "youtube.leanback.v4"),
    ("prime video", "amazon"),
    ("amazon", "amazon"),
    ("disney+", "com.disney.disneyplus-prod"),
    ("disney", "com.disney.disneyplus-prod"),
    ("spotify", "spotify-beehive"),
    ("live tv", "com.webos.app.livetv"),
    ("browser", "com.webos.app.browser"),
    ("settings", "com.palm.app.settings"),
];

pub struct Tv {
    pub ip: Ipv4Addr,
    pub mac: Option<[u8; 6]>,
    pub client_key: Option<String>,
    /// SHA-256 of the certificate the TV presented when it paired, in hex.
    pub cert: Option<String>,
}

/// What a connection learned that should be kept for next time.
#[derive(Default)]
pub struct Learned {
    pub client_key: Option<String>,
    pub cert: Option<String>,
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// One registered connection.
pub struct Session {
    ws: Socket,
    next: u32,
}

impl Session {
    fn request(&mut self, uri: &str, payload: Value) -> Result<Value, String> {
        self.next += 1;
        let id = format!("hc{}", self.next);
        let msg = json!({"type": "request", "id": id, "uri": uri, "payload": payload});
        self.ws.send(Message::text(msg.to_string())).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + ANSWER;
        loop {
            let answer = read_json(&mut self.ws, deadline)?;
            if answer.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                continue;
            }
            if answer.get("type").and_then(Value::as_str) == Some("error") {
                let why = answer.get("error").and_then(Value::as_str).unwrap_or("refused");
                return Err(format!("TV said: {why}"));
            }
            let payload = answer.get("payload").cloned().unwrap_or(Value::Null);
            if payload.get("returnValue") == Some(&Value::Bool(false)) {
                let why = payload.get("errorText").and_then(Value::as_str).unwrap_or("refused");
                return Err(format!("TV said: {why}"));
            }
            return Ok(payload);
        }
    }
}

fn read_json(ws: &mut Socket, deadline: Instant) -> Result<Value, String> {
    loop {
        if Instant::now() > deadline {
            return Err("TV stopped answering".into());
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                return serde_json::from_str(text.as_str()).map_err(|e| e.to_string());
            }
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                continue
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The permissions hopchat asks for when pairing, as Home Assistant's
/// webOS library does; the manifest needs no signature.
fn registration(client_key: Option<&str>) -> Value {
    let mut payload = json!({
        "forcePairing": false,
        "pairingType": "PROMPT",
        "manifest": {
            "appVersion": "1.1",
            "manifestVersion": 1,
            "permissions": [
                "APP_TO_APP", "CLOSE", "CONTROL_AUDIO", "CONTROL_DISPLAY",
                "CONTROL_INPUT_JOYSTICK", "CONTROL_INPUT_MEDIA_PLAYBACK",
                "CONTROL_INPUT_TV", "CONTROL_POWER", "CONTROL_TV_SCREEN", "LAUNCH",
                "READ_APP_STATUS", "READ_CURRENT_CHANNEL", "READ_INPUT_DEVICE_LIST",
                "READ_INSTALLED_APPS", "READ_POWER_STATE", "READ_RUNNING_APPS",
                "READ_SETTINGS", "WRITE_NOTIFICATION_TOAST"
            ]
        }
    });
    if let Some(key) = client_key {
        payload["client-key"] = json!(key);
    }
    json!({"type": "register", "id": "register_0", "payload": payload})
}

impl Tv {
    /// Opens the WebSocket: TLS on 3001 if the TV listens there, else plain
    /// on 3000. Returns the certificate fingerprint for TLS.
    fn open(&self) -> Result<(Socket, Option<String>), String> {
        let unreachable = || format!("TV at {} is off or unreachable", self.ip);
        let tcp = |port| {
            let stream = TcpStream::connect_timeout(&SocketAddr::from((self.ip, port)), CONNECT).ok()?;
            stream.set_read_timeout(Some(ANSWER)).ok()?;
            Some(stream)
        };
        if let Some(stream) = tcp(TLS_PORT) {
            let tls = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .build()
                .map_err(|e| e.to_string())?;
            let url = format!("wss://{}:{TLS_PORT}/", self.ip);
            let (ws, _) = tungstenite::client_tls_with_config(url.as_str(), stream, None, Some(Connector::NativeTls(tls)))
                .map_err(|e| format!("TLS to the TV failed: {e}"))?;
            let fingerprint = match ws.get_ref() {
                MaybeTlsStream::NativeTls(t) => t
                    .peer_certificate()
                    .ok()
                    .flatten()
                    .and_then(|c| c.to_der().ok())
                    .map(|der| Sha256::digest(der).iter().map(|b| format!("{b:02x}")).collect()),
                _ => None,
            };
            let fingerprint = fingerprint.ok_or("the TV presented no certificate")?;
            if let Some(pinned) = &self.cert
                && *pinned != fingerprint
            {
                return Err("the TV's certificate changed since it paired; refusing to connect. \
                     If you reset the TV, pair again with /home tv IP"
                    .into());
            }
            Ok((ws, Some(fingerprint)))
        } else {
            // A TV that paired over TLS never falls back to plain text.
            if self.cert.is_some() {
                return Err(unreachable());
            }
            let stream = tcp(PLAIN_PORT).ok_or_else(unreachable)?;
            let url = format!("ws://{}:{PLAIN_PORT}/", self.ip);
            let (ws, _) = tungstenite::client(url.as_str(), MaybeTlsStream::Plain(stream))
                .map_err(|e| e.to_string())?;
            Ok((ws, None))
        }
    }

    /// Connects and registers, returning anything worth keeping.
    fn connect(&self) -> Result<(Session, Learned), String> {
        let (mut ws, cert) = self.open()?;
        set_read_timeout(&ws, Duration::from_millis(250));
        let register = registration(self.client_key.as_deref());
        ws.send(Message::text(register.to_string())).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + PAIRING;
        loop {
            let answer = read_json(&mut ws, deadline)
                .map_err(|e| format!("pairing failed ({e}); accept the prompt on the TV"))?;
            match answer.get("type").and_then(Value::as_str) {
                Some("registered") => {
                    let key = answer
                        .pointer("/payload/client-key")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let learned = Learned {
                        client_key: key.filter(|k| Some(k) != self.client_key.as_ref()),
                        cert: cert.filter(|c| Some(c) != self.cert.as_ref()),
                    };
                    return Ok((Session { ws, next: 0 }, learned));
                }
                Some("error") => {
                    let why = answer.get("error").and_then(Value::as_str).unwrap_or("refused");
                    return Err(format!("TV refused pairing: {why}"));
                }
                // "response" with pairingType PROMPT: the TV is asking.
                _ => {}
            }
        }
    }

    /// Runs `f` on a registered session; returns its result and what the
    /// connection learned.
    pub fn with<T>(
        &self,
        f: impl FnOnce(&mut Session) -> Result<T, String>,
    ) -> (Result<T, String>, Learned) {
        match self.connect() {
            Ok((mut session, learned)) => {
                let result = f(&mut session);
                let _ = session.ws.close(None);
                (result, learned)
            }
            Err(e) => (Err(e), Learned::default()),
        }
    }

    /// Whether something at `ip` speaks the webOS WebSocket API. Only the
    /// handshake: nothing registers, so the TV shows nothing.
    pub fn probe(ip: Ipv4Addr) -> bool {
        let tv = Tv { ip, mac: None, client_key: None, cert: None };
        match tv.open() {
            Ok((mut ws, _)) => {
                let _ = ws.close(None);
                true
            }
            Err(_) => false,
        }
    }

    /// Whether the TV's API port answers at all; a TV in standby doesn't.
    pub fn reachable(&self) -> bool {
        [TLS_PORT, PLAIN_PORT].iter().any(|port| {
            TcpStream::connect_timeout(&SocketAddr::from((self.ip, *port)), Duration::from_millis(800)).is_ok()
        })
    }

    /// Wakes the TV with a magic packet. webOS keeps the network up in
    /// standby only with "Turn on via Wi-Fi" / "Mobile TV On" enabled.
    pub fn wake(&self) -> Result<(), String> {
        let mac = self.mac.ok_or("I don't know the TV's MAC address, so I can't turn it on")?;
        let mut packet = vec![0xFFu8; 6];
        for _ in 0..16 {
            packet.extend_from_slice(&mac);
        }
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
        socket.set_broadcast(true).map_err(|e| e.to_string())?;
        for port in [9, 7] {
            socket.send_to(&packet, (Ipv4Addr::BROADCAST, port)).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn set_read_timeout(ws: &Socket, timeout: Duration) {
    let tcp = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::NativeTls(t) => t.get_ref(),
        _ => return,
    };
    let _ = tcp.set_read_timeout(Some(timeout));
}

/// The TV's model, e.g. "OLED55C3PSA".
pub fn model_name(s: &mut Session) -> Result<String, String> {
    let info = s.request("ssap://system/getSystemInfo", json!({}))?;
    Ok(info.get("modelName").and_then(Value::as_str).unwrap_or("LG TV").to_string())
}

pub fn power_off(s: &mut Session) -> Result<String, String> {
    s.request("ssap://system/turnOff", json!({}))?;
    Ok("TV turned off".into())
}

pub fn volume(s: &mut Session, level: Option<u8>, change: Option<i8>, mute: Option<bool>) -> Result<String, String> {
    if let Some(mute) = mute {
        s.request("ssap://audio/setMute", json!({"mute": mute}))?;
    }
    if let Some(level) = level {
        s.request("ssap://audio/setVolume", json!({"volume": level.min(100)}))?;
    } else if let Some(change) = change {
        let uri = if change > 0 { "ssap://audio/volumeUp" } else { "ssap://audio/volumeDown" };
        for _ in 0..change.unsigned_abs().min(20) {
            s.request(uri, json!({}))?;
        }
    }
    let (now, muted) = current_volume(s)?;
    Ok(format!("volume {now}{}", if muted { " (muted)" } else { "" }))
}

fn current_volume(s: &mut Session) -> Result<(u64, bool), String> {
    let p = s.request("ssap://audio/getVolume", json!({}))?;
    // Older firmware answers flat, newer nests it in volumeStatus.
    let v = p.get("volumeStatus").unwrap_or(&p);
    let level = v.get("volume").and_then(Value::as_u64).unwrap_or(0);
    let muted = v.get("muteStatus").or_else(|| v.get("muted")).and_then(Value::as_bool).unwrap_or(false);
    Ok((level, muted))
}

pub fn launch(s: &mut Session, app: &str) -> Result<String, String> {
    let wanted = app.trim().to_ascii_lowercase();
    let known = APPS.iter().find(|(name, _)| *name == wanted).map(|(_, id)| id.to_string());
    let (id, title) = match known {
        Some(id) => (id, app.trim().to_string()),
        None => {
            let listed = s.request("ssap://com.webos.applicationManager/listApps", json!({}))?;
            let apps = listed.get("apps").and_then(Value::as_array).cloned().unwrap_or_default();
            let hit = apps.iter().find_map(|a| {
                let title = a.get("title")?.as_str()?;
                let id = a.get("id")?.as_str()?;
                title.to_ascii_lowercase().contains(&wanted).then(|| (id.to_string(), title.to_string()))
            });
            hit.ok_or_else(|| format!("no app called \"{}\" on the TV", app.trim()))?
        }
    };
    s.request("ssap://system.launcher/launch", json!({"id": id}))?;
    Ok(format!("opened {title}"))
}

pub fn input(s: &mut Session, input: &str) -> Result<String, String> {
    let digits: String = input.chars().filter(char::is_ascii_digit).collect();
    let id = if input.to_ascii_lowercase().contains("hdmi") && !digits.is_empty() {
        format!("HDMI_{digits}")
    } else {
        input.trim().to_ascii_uppercase()
    };
    s.request("ssap://tv/switchInput", json!({"inputId": id}))?;
    Ok(format!("switched to {id}"))
}

pub fn media(s: &mut Session, action: &str) -> Result<String, String> {
    let action = match action.trim().to_ascii_lowercase().as_str() {
        "play" | "resume" => "play",
        "pause" => "pause",
        "stop" => "stop",
        "rewind" => "rewind",
        "fastforward" | "fast forward" | "forward" => "fastForward",
        other => return Err(format!("can't {other} on the TV")),
    };
    s.request(&format!("ssap://media.controls/{action}"), json!({}))?;
    Ok(format!("{action} sent"))
}

pub fn toast(s: &mut Session, message: &str) -> Result<String, String> {
    let message: String = message.chars().filter(|c| !c.is_control()).take(200).collect();
    s.request("ssap://system.notifications/createToast", json!({"message": message}))?;
    Ok("message shown on the TV".into())
}

pub fn status(s: &mut Session) -> Result<String, String> {
    let app = s
        .request("ssap://com.webos.applicationManager/getForegroundAppInfo", json!({}))?
        .get("appId")
        .and_then(Value::as_str)
        .unwrap_or("nothing")
        .to_string();
    let (level, muted) = current_volume(s)?;
    Ok(format!("on, showing {app}, volume {level}{}", if muted { " (muted)" } else { "" }))
}

/// The MAC the OS has cached for `ip`, from the ARP table.
pub fn mac_of(ip: Ipv4Addr) -> Option<[u8; 6]> {
    let out = std::process::Command::new("arp").arg("-n").arg(ip.to_string()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().find_map(parse_mac)
}

pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = text.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (slot, part) in mac.iter_mut().zip(parts) {
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    Some(mac)
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_from_arp() {
        // macOS drops leading zeros in arp output.
        assert_eq!(parse_mac("a4:36:c7:1:2:ff"), Some([0xa4, 0x36, 0xc7, 1, 2, 0xff]));
        assert_eq!(parse_mac("(incomplete)"), None);
        assert_eq!(format_mac(&[0xa4, 0x36, 0xc7, 1, 2, 0xff]), "a4:36:c7:01:02:ff");
    }

    #[test]
    fn registration_presents_a_known_key() {
        assert_eq!(registration(Some("k1"))["payload"]["client-key"], "k1");
        assert!(registration(None)["payload"].get("client-key").is_none());
    }
}
