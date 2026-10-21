//! "home": lets the people in a group talk to this machine's smart devices.
//!
//! A group message that mentions `@home`, in a group where home is switched
//! on (`/home on`), goes to a local model with one tool per thing the
//! devices can do. The model's answer goes back into the group. Group
//! membership is the permission: anyone MLS admitted to that group may ask.

mod ollama;
pub mod webos;
pub mod wiz;

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::engine::{Command, Input, Update};
use ollama::{Ollama, Turn};

/// The word that addresses the devices in a group.
pub const MENTION: &str = "@home";
/// What every message from home starts with.
pub const SIGNATURE: &str = "🏠 ";
/// Model turns per request: act, maybe act again on what it saw, then say.
const MAX_TURNS: usize = 4;
const MAX_REQUEST_CHARS: usize = 500;

#[derive(Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub bulb: Option<Ipv4Addr>,
    #[serde(default)]
    pub tv: Option<Ipv4Addr>,
    #[serde(default)]
    pub tv_mac: Option<String>,
    #[serde(default)]
    pub tv_key: Option<String>,
    /// The TV certificate pinned at pairing.
    #[serde(default)]
    pub tv_cert: Option<String>,
    /// Groups whose members may use the devices.
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_ollama")]
    pub ollama: String,
}

fn default_model() -> String {
    "qwen2.5:7b".into()
}

fn default_ollama() -> String {
    std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| if h.starts_with("http") { h } else { format!("http://{h}") })
        .unwrap_or_else(|| "http://127.0.0.1:11434".into())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bulb: None,
            tv: None,
            tv_mac: None,
            tv_key: None,
            tv_cert: None,
            groups: Vec::new(),
            model: default_model(),
            ollama: default_ollama(),
        }
    }
}

/// What the rest of hopchat asks of home.
pub enum Request {
    /// A group message that mentioned [`MENTION`].
    Ask { group: String, from: String, text: String },
    /// `/home …` typed in a conversation (`group` when it is one).
    Configure { group: Option<String>, args: String },
}

/// Whether `text` addresses home, and the request with the mention removed.
pub fn addressed(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find(MENTION)?;
    // "@homer" is someone else.
    let end = at + MENTION.len();
    if lower[end..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let after = text[end..].trim_start().trim_start_matches([',', ':']);
    let rest = format!("{} {after}", &text[..at]);
    Some(rest.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub struct Home {
    path: PathBuf,
    config: Config,
    engine: Sender<Input>,
    ui: Sender<Update>,
}

impl Home {
    pub fn open(root: &Path, engine: Sender<Input>, ui: Sender<Update>) -> Self {
        let path = root.join("home.json");
        let config = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, config, engine, ui }
    }

    pub fn enabled(&self, group: &str) -> bool {
        self.config.groups.iter().any(|g| g == group)
    }

    fn save(&self) {
        let result = serde_json::to_vec_pretty(&self.config)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::engine::write_private(&self.path, &bytes));
        if let Err(e) = result {
            self.note(format!("home settings not saved: {e}"));
        }
    }

    fn note(&self, text: String) {
        let _ = self.ui.send(Update::Home(text));
    }

    fn say(&self, group: &str, text: String) {
        let _ = self.engine.send(Input::Ui(Command::SendGroup {
            group: group.to_string(),
            text: format!("{SIGNATURE}{text}"),
            reply: None,
            from_home: true,
        }));
    }

    pub fn run(mut self, requests: Receiver<Request>) {
        if self.config.bulb.is_none()
            && let Some(ip) = wiz::discover(Duration::from_secs(2)).first()
        {
            self.config.bulb = Some(*ip);
            self.save();
            self.note(format!("home found a Wiz bulb at {ip}"));
        }
        for request in requests {
            match request {
                Request::Ask { group, from, text } => {
                    if !self.enabled(&group) {
                        continue;
                    }
                    let text: String = text.chars().take(MAX_REQUEST_CHARS).collect();
                    let answer = self.ask(&from, &text);
                    self.say(&group, answer);
                }
                Request::Configure { group, args } => {
                    let reply = self.configure(group.as_deref(), &args);
                    self.note(reply);
                }
            }
        }
    }

    // ----- /home ---------------------------------------------------------

    fn configure(&mut self, group: Option<&str>, args: &str) -> String {
        let mut words = args.split_whitespace();
        let verb = words.next().unwrap_or("status");
        let value = words.next();
        match (verb, value) {
            ("on", _) => {
                let Some(group) = group else {
                    return "open a group first: /home on lets that group use your devices".into();
                };
                if !self.enabled(group) {
                    self.config.groups.push(group.to_string());
                    self.save();
                }
                self.say(group, format!(
                    "is listening. Mention {MENTION} to use {}.",
                    self.devices_phrase()
                ));
                "home is on in this group".into()
            }
            ("off", _) => {
                let Some(group) = group else { return "open the group to switch off".into() };
                self.config.groups.retain(|g| g != group);
                self.save();
                self.say(group, "has stopped listening.".into());
                "home is off in this group".into()
            }
            ("bulb", Some(ip)) => match ip.parse() {
                Ok(ip) => {
                    self.config.bulb = Some(ip);
                    self.save();
                    match (wiz::Bulb { ip }).status() {
                        Ok(state) => format!("bulb at {ip}: {state}"),
                        Err(e) => format!("bulb saved, but {e}"),
                    }
                }
                Err(_) => format!("{ip} isn't an IPv4 address"),
            },
            ("tv", Some(ip)) => match ip.parse() {
                Ok(ip) => {
                    self.config.tv = Some(ip);
                    self.config.tv_key = None;
                    self.config.tv_cert = None;
                    self.config.tv_mac = webos::mac_of(ip).map(|m| webos::format_mac(&m));
                    self.save();
                    self.note(format!("pairing with the TV at {ip}: accept the prompt on the TV"));
                    match self.tv_do(webos::status) {
                        Ok(state) => format!("TV at {ip} paired: {state}"),
                        Err(e) => format!("TV saved, but {e}"),
                    }
                }
                Err(_) => format!("{ip} isn't an IPv4 address"),
            },
            ("model", Some(model)) => {
                self.config.model = model.to_string();
                self.save();
                format!("home will use {model}")
            }
            _ => format!(
                "home: bulb {} · TV {} · model {} · on in {} group{} · /home on|off|bulb IP|tv IP|model NAME",
                self.config.bulb.map_or("not set".into(), |ip| ip.to_string()),
                self.config.tv.map_or("not set".into(), |ip| ip.to_string()),
                self.config.model,
                self.config.groups.len(),
                if self.config.groups.len() == 1 { "" } else { "s" },
            ),
        }
    }

    fn devices_phrase(&self) -> &'static str {
        match (self.config.bulb.is_some(), self.config.tv.is_some()) {
            (true, true) => "the light and the TV",
            (true, false) => "the light",
            (false, true) => "the TV",
            (false, false) => "the devices (none set up yet)",
        }
    }

    // ----- the model ---------------------------------------------------------

    fn ask(&mut self, from: &str, text: &str) -> String {
        if self.config.bulb.is_none() && self.config.tv.is_none() {
            return "no devices are set up yet.".into();
        }
        let model = Ollama::new(self.config.ollama.clone(), self.config.model.clone());
        let tools = self.tools();
        let mut messages = vec![
            json!({"role": "system", "content": self.system_prompt()}),
            json!({"role": "user", "content": format!("{from}: {text}")}),
        ];
        let mut done = Vec::new();
        for _ in 0..MAX_TURNS {
            match model.turn(&messages, &tools) {
                Ok(Turn::Say(text)) if !text.is_empty() => return one_line(&text),
                Ok(Turn::Say(_)) => break,
                Ok(Turn::Act(calls, assistant)) => {
                    messages.push(assistant);
                    for call in calls {
                        let result = self.run_tool(&call.name, &call.args);
                        let text = match &result {
                            Ok(t) => t.clone(),
                            Err(e) => format!("error: {e}"),
                        };
                        done.push(text.clone());
                        messages.push(json!({"role": "tool", "tool_name": call.name, "content": text}));
                    }
                }
                Err(e) => {
                    self.note(format!("home: {e}"));
                    return if done.is_empty() { format!("can't think right now: {e}") } else { done.join("; ") };
                }
            }
        }
        if done.is_empty() { "I'm not sure what to do with that.".into() } else { done.join("; ") }
    }

    fn system_prompt(&self) -> String {
        let mut devices = Vec::new();
        if self.config.bulb.is_some() {
            devices.push("a Wiz smart bulb (\"the light\", \"the lamp\")");
        }
        if self.config.tv.is_some() {
            devices.push("an LG webOS TV");
        }
        format!(
            "You are home, the assistant for a household's devices, answering in a group chat. \
             The devices are: {}. Each message starts with the sender's name. \
             Use the tools to do what they ask, and check state with a status tool when they ask \
             how things are. Never invent device state. After acting, reply with one short, \
             friendly sentence saying what you did. If a request has nothing to do with the \
             devices, say briefly that you only handle the devices. Scenes for the light: {}.",
            devices.join(" and "),
            wiz::SCENES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "),
        )
    }

    fn tools(&self) -> Value {
        let mut tools = Vec::new();
        let tool = |name: &str, description: &str, properties: Value| {
            json!({"type": "function", "function": {
                "name": name,
                "description": description,
                "parameters": {"type": "object", "properties": properties},
            }})
        };
        if self.config.bulb.is_some() {
            tools.push(tool(
                "light_set",
                "Change the light. Give only what should change. Setting brightness, colour, \
                 temperature or a scene also turns it on.",
                json!({
                    "on": {"type": "boolean", "description": "true to switch on, false to switch off"},
                    "brightness": {"type": "integer", "description": "percent, 10 to 100"},
                    "kelvin": {"type": "integer", "description": "white temperature, 2200 (warm) to 6500 (cool)"},
                    "color": {"type": "string", "description": "a colour name like red or purple, or #rrggbb"},
                    "scene": {"type": "string", "description": "one of the light's scene names"},
                }),
            ));
            tools.push(tool("light_status", "Read what the light is doing now.", json!({})));
        }
        if self.config.tv.is_some() {
            tools.push(tool(
                "tv_power",
                "Turn the TV on or off.",
                json!({"on": {"type": "boolean"}}),
            ));
            tools.push(tool(
                "tv_volume",
                "Set, raise, lower or mute the TV's volume.",
                json!({
                    "level": {"type": "integer", "description": "absolute volume, 0 to 100"},
                    "change": {"type": "integer", "description": "steps up (positive) or down (negative)"},
                    "mute": {"type": "boolean"},
                }),
            ));
            tools.push(tool(
                "tv_open_app",
                "Open an app on the TV, such as Netflix, YouTube, Prime Video, Disney+, Spotify or Live TV.",
                json!({"app": {"type": "string"}}),
            ));
            tools.push(tool(
                "tv_input",
                "Switch the TV's input, such as HDMI_1.",
                json!({"input": {"type": "string"}}),
            ));
            tools.push(tool(
                "tv_media",
                "Control what is playing.",
                json!({"action": {"type": "string", "enum": ["play", "pause", "stop", "rewind", "fastForward"]}}),
            ));
            tools.push(tool(
                "tv_message",
                "Pop up a short message on the TV screen.",
                json!({"text": {"type": "string"}}),
            ));
            tools.push(tool("tv_status", "Read what the TV is showing and its volume.", json!({})));
        }
        Value::Array(tools)
    }

    fn run_tool(&mut self, name: &str, args: &Value) -> Result<String, String> {
        let summary = format!("home: {name} {args}");
        self.note(summary);
        match name {
            "light_set" => {
                let rgb = match text(args, "color") {
                    Some(c) => Some(colour(&c).ok_or_else(|| format!("I don't know the colour {c}"))?),
                    None => None,
                };
                let scene = match text(args, "scene") {
                    Some(s) => Some(wiz::scene_id(&s).ok_or_else(|| format!("no scene called {s}"))?),
                    None => None,
                };
                let pilot = wiz::Pilot {
                    on: flag(args, "on"),
                    brightness: number(args, "brightness").map(|b| b.clamp(0.0, 100.0) as u8),
                    kelvin: number(args, "kelvin").map(|k| k.clamp(0.0, 10000.0) as u16),
                    rgb,
                    scene,
                };
                let bulb = self.bulb()?;
                bulb.set(&pilot)?;
                Ok(format!("light is now {}", bulb.status()?))
            }
            "light_status" => Ok(format!("light is {}", self.bulb()?.status()?)),
            "tv_power" => match flag(args, "on") {
                Some(true) => {
                    if self.tv_do(webos::status).is_ok() {
                        return Ok("TV is already on".into());
                    }
                    self.tv()?.wake()?;
                    Ok("sent the TV a wake-up signal".into())
                }
                _ => self.tv_do(webos::power_off),
            },
            "tv_volume" => {
                let level = number(args, "level").map(|l| l.clamp(0.0, 100.0) as u8);
                let change = number(args, "change").map(|c| c.clamp(-20.0, 20.0) as i8);
                let mute = flag(args, "mute");
                self.tv_do(|s| webos::volume(s, level, change, mute))
            }
            "tv_open_app" => {
                let app = text(args, "app").ok_or("which app?")?;
                self.tv_do(|s| webos::launch(s, &app))
            }
            "tv_input" => {
                let input = text(args, "input").ok_or("which input?")?;
                self.tv_do(|s| webos::input(s, &input))
            }
            "tv_media" => {
                let action = text(args, "action").ok_or("which action?")?;
                self.tv_do(|s| webos::media(s, &action))
            }
            "tv_message" => {
                let message = text(args, "text").ok_or("what should it say?")?;
                self.tv_do(|s| webos::toast(s, &message))
            }
            "tv_status" => self.tv_do(webos::status),
            other => Err(format!("there's no tool called {other}")),
        }
    }

    fn bulb(&self) -> Result<wiz::Bulb, String> {
        let ip = self.config.bulb.ok_or("no light is set up")?;
        Ok(wiz::Bulb { ip })
    }

    fn tv(&self) -> Result<webos::Tv, String> {
        let ip = self.config.tv.ok_or("no TV is set up")?;
        Ok(webos::Tv {
            ip,
            mac: self.config.tv_mac.as_deref().and_then(webos::parse_mac),
            client_key: self.config.tv_key.clone(),
            cert: self.config.tv_cert.clone(),
        })
    }

    /// Runs `f` against the TV and keeps the key and certificate it paired
    /// with.
    fn tv_do<T>(&mut self, f: impl FnOnce(&mut webos::Session) -> Result<T, String>) -> Result<T, String> {
        let tv = self.tv()?;
        let (result, learned) = tv.with(f);
        if learned.cert.is_some() && self.config.tv_cert.is_none() {
            self.config.tv_cert = learned.cert;
            self.save();
        }
        if let Some(key) = learned.client_key {
            self.config.tv_key = Some(key);
            if self.config.tv_mac.is_none() {
                self.config.tv_mac = webos::mac_of(tv.ip).map(|m| webos::format_mac(&m));
            }
            self.save();
            self.note("home paired with the TV".into());
        }
        result
    }
}

// ----- lenient argument reading: small models send "50" as often as 50 ----

fn number(args: &Value, key: &str) -> Option<f64> {
    match args.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches(['%', 'K', 'k']).parse().ok(),
        _ => None,
    }
}

fn flag(args: &Value, key: &str) -> Option<bool> {
    match args.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" => Some(true),
            "false" | "off" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key)?.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn colour(name: &str) -> Option<(u8, u8, u8)> {
    let name = name.trim().to_ascii_lowercase();
    if let Some(hex) = name.strip_prefix('#')
        && hex.len() == 6
    {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some(((v >> 16) as u8, (v >> 8) as u8, v as u8));
    }
    Some(match name.as_str() {
        "red" => (255, 0, 0),
        "orange" => (255, 120, 0),
        "yellow" => (255, 200, 0),
        "green" => (0, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "teal" => (0, 160, 140),
        "blue" => (0, 0, 255),
        "purple" | "violet" => (140, 0, 255),
        "magenta" => (255, 0, 255),
        "pink" => (255, 80, 160),
        _ => return None,
    })
}

fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(400).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions() {
        assert_eq!(addressed("@home lights off").as_deref(), Some("lights off"));
        assert_eq!(addressed("hey @Home, put on netflix").as_deref(), Some("hey put on netflix"));
        assert_eq!(addressed("dim it @home").as_deref(), Some("dim it"));
        assert_eq!(addressed("ask @homer"), None);
        assert_eq!(addressed("no mention"), None);
    }

    /// Asks the real local model what it would do, without doing it.
    /// `cargo test -- --ignored` with `ollama serve` running.
    #[test]
    #[ignore = "needs a local Ollama with the model pulled"]
    fn model_picks_the_right_tools() {
        let (engine, _) = std::sync::mpsc::channel();
        let (ui, _) = std::sync::mpsc::channel();
        let home = Home {
            path: std::env::temp_dir().join("hopchat-home-test.json"),
            config: Config {
                bulb: Some(Ipv4Addr::new(10, 0, 0, 1)),
                tv: Some(Ipv4Addr::new(10, 0, 0, 2)),
                ..Default::default()
            },
            engine,
            ui,
        };
        let model = Ollama::new(home.config.ollama.clone(), home.config.model.clone());
        let cases: &[(&str, &[&str])] = &[
            ("turn off the lights", &["light_set"]),
            ("make the lamp purple", &["light_set"]),
            ("dim the lights to 20% and put on netflix", &["light_set", "tv_open_app"]),
            ("turn the volume down a bit", &["tv_volume"]),
            ("is the tv on?", &["tv_status"]),
        ];
        for (ask, want) in cases {
            let messages = vec![
                json!({"role": "system", "content": home.system_prompt()}),
                json!({"role": "user", "content": format!("bob: {ask}")}),
            ];
            let Turn::Act(calls, _) = model.turn(&messages, &home.tools()).unwrap() else {
                panic!("{ask}: no tool call");
            };
            let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
            println!("{ask} -> {:?}", calls.iter().map(|c| format!("{} {}", c.name, c.args)).collect::<Vec<_>>());
            for w in *want {
                assert!(names.contains(w), "{ask}: wanted {w}, got {names:?}");
            }
        }
    }

    #[test]
    fn lenient_arguments() {
        let args = json!({"brightness": "40%", "on": "yes", "kelvin": 2700, "app": " Netflix "});
        assert_eq!(number(&args, "brightness"), Some(40.0));
        assert_eq!(number(&args, "kelvin"), Some(2700.0));
        assert_eq!(flag(&args, "on"), Some(true));
        assert_eq!(text(&args, "app").as_deref(), Some("Netflix"));
        assert_eq!(colour("#ff8000"), Some((255, 128, 0)));
        assert_eq!(colour("Purple"), Some((140, 0, 255)));
        assert_eq!(colour("plaid"), None);
    }
}
