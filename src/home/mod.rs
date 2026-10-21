//! "home": lets the people in a group talk to this machine's smart devices.
//!
//! The host finds and adds devices in the Devices panel (`/devices`). A
//! group message that mentions `@home`, in a group where home is switched
//! on, goes to a local model with one tool per thing the devices can do;
//! its answer goes back into the group. Group membership is the
//! permission: anyone MLS admitted to that group may ask.
//!
//! The same model also answers `@ai`, for anything at all, in the groups
//! the host has switched that on for (see [`ai`]).

pub mod ai;
mod assistant;
mod discover;
mod ollama;
pub mod webos;
pub mod wiz;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::engine::{Command, Input, Update};

/// The word that addresses the devices in a group.
pub const MENTION: &str = "@home";
/// What every message from home starts with.
pub const SIGNATURE: &str = "🏠 ";
const MAX_REQUEST_CHARS: usize = 500;
const MAX_NAME_CHARS: usize = 32;
/// How long the TV shows its "allow this device?" prompt.
const PAIRING: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Light,
    Tv,
}

/// A device someone added.
#[derive(Clone, Serialize, Deserialize)]
pub struct Device {
    pub kind: Kind,
    pub name: String,
    pub ip: Ipv4Addr,
    /// What identifies it when DHCP hands it a new address.
    #[serde(default)]
    pub mac: Option<String>,
    /// The Wiz module name, or the TV's model.
    #[serde(default)]
    pub model: Option<String>,
    /// The TV's client key, issued when it paired.
    #[serde(default)]
    pub key: Option<String>,
    /// The TV certificate pinned when it paired.
    #[serde(default)]
    pub cert: Option<String>,
}

impl Device {
    pub fn id(&self) -> String {
        self.mac.clone().unwrap_or_else(|| self.ip.to_string())
    }

    fn caps(&self) -> wiz::Caps {
        wiz::Caps::of(self.model.as_deref().unwrap_or_default())
    }

    fn same(&self, kind: Kind, ip: Ipv4Addr, mac: Option<&str>) -> bool {
        self.kind == kind
            && match (self.mac.as_deref(), mac) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => self.ip == ip,
            }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub devices: Vec<Device>,
    /// Groups whose members may use the devices.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Groups whose members may ask the model anything with `@ai`.
    #[serde(default)]
    pub ai_groups: Vec<String>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_ollama")]
    pub ollama: String,
    // The single-bulb, single-TV settings of earlier versions, folded into
    // `devices` when read.
    #[serde(default, skip_serializing)]
    bulb: Option<Ipv4Addr>,
    #[serde(default, skip_serializing)]
    tv: Option<Ipv4Addr>,
    #[serde(default, skip_serializing)]
    tv_mac: Option<String>,
    #[serde(default, skip_serializing)]
    tv_key: Option<String>,
    #[serde(default, skip_serializing)]
    tv_cert: Option<String>,
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
            devices: Vec::new(),
            groups: Vec::new(),
            ai_groups: Vec::new(),
            model: default_model(),
            ollama: default_ollama(),
            bulb: None,
            tv: None,
            tv_mac: None,
            tv_key: None,
            tv_cert: None,
        }
    }
}

impl Config {
    fn migrate(&mut self) {
        if let Some(ip) = self.bulb.take() {
            self.devices.push(Device {
                kind: Kind::Light,
                name: "Light".into(),
                ip,
                mac: None,
                model: None,
                key: None,
                cert: None,
            });
        }
        if let Some(ip) = self.tv.take() {
            self.devices.push(Device {
                kind: Kind::Tv,
                name: "TV".into(),
                ip,
                mac: self.tv_mac.take(),
                model: None,
                key: self.tv_key.take(),
                cert: self.tv_cert.take(),
            });
        }
    }
}

// ----- what the Devices panel shows ----------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Found on the network, not added.
    New,
    Checking,
    Ready,
    /// Added, but not answering.
    Offline,
    /// Waiting for someone to accept the prompt on the TV.
    Pairing { until: Instant },
    Failed(String),
}

#[derive(Clone)]
pub struct DeviceView {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub ip: Ipv4Addr,
    pub added: bool,
    pub state: State,
    /// What it is doing, or what it is.
    pub detail: String,
}

#[derive(Clone, Default, PartialEq)]
pub enum ModelState {
    #[default]
    Unknown,
    Ready,
    /// Ollama is up but the model isn't pulled.
    Missing,
    /// Ollama isn't answering.
    NoServer,
}

#[derive(Clone, Default)]
pub struct View {
    pub devices: Vec<DeviceView>,
    pub scanning: bool,
    pub model: String,
    pub model_state: ModelState,
    /// Groups home is switched on in.
    pub groups: Vec<String>,
    /// Groups that may ask the model with `@ai`.
    pub ai_groups: Vec<String>,
}

/// What the rest of hopchat asks of home.
pub enum Request {
    /// A group message that mentioned [`MENTION`].
    Ask { group: String, from: String, text: String },
    /// A group message that mentioned [`ai::MENTION`].
    Chat(ai::Question),
    /// `/home …` typed in a conversation (`group` when it is one).
    Configure { group: Option<String>, args: String },
    Scan,
    /// Add a found device: a light at once, a TV by pairing.
    Add(String),
    Forget(String),
    /// Blink a light, or put a message on a TV.
    Test(String),
    Rename(String, String),
    SetGroup { group: String, on: bool },
    // Results from home's own worker threads.
    Found(discover::Found),
    ScanDone,
    Checked { id: String, state: State, detail: String },
    Paired { id: String, result: Result<(webos::Learned, String), String> },
    ModelChecked(ModelState),
    Answered { question: ai::Question, asked: serde_json::Value, answer: Result<String, String> },
}

/// Whether `text` addresses home, and the request with the mention removed.
pub fn addressed(text: &str) -> Option<String> {
    mentioned(text, MENTION)
}

/// Whether `text` mentions `word`, and the text with the mention removed.
fn mentioned(text: &str, word: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find(word)?;
    // "@homer" is someone else.
    let end = at + word.len();
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
    /// Home's own queue, for worker threads to report back on.
    me: Sender<Request>,
    /// Found on the network and not added.
    found: Vec<Device>,
    live: HashMap<String, (State, String)>,
    scanning: bool,
    model_state: ModelState,
    /// Each group's recent `@ai` chat, oldest first.
    memory: HashMap<String, Vec<serde_json::Value>>,
}

impl Home {
    pub fn open(root: &Path, engine: Sender<Input>, ui: Sender<Update>, me: Sender<Request>) -> Self {
        let path = root.join("home.json");
        let mut config: Config = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        config.migrate();
        Self {
            path,
            config,
            engine,
            ui,
            me,
            found: Vec::new(),
            live: HashMap::new(),
            scanning: false,
            model_state: ModelState::Unknown,
            memory: HashMap::new(),
        }
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

    fn publish(&self) {
        let _ = self.ui.send(Update::HomeView(Box::new(self.view())));
    }

    fn view(&self) -> View {
        let live = |d: &Device, fallback: State| {
            self.live
                .get(&d.id())
                .cloned()
                .unwrap_or((fallback, String::new()))
        };
        let mut devices: Vec<DeviceView> = Vec::new();
        for d in &self.config.devices {
            let (state, detail) = live(d, State::Checking);
            // An added TV that never finished pairing is offered again.
            let state = if d.kind == Kind::Tv && d.key.is_none() && matches!(state, State::Ready) {
                State::New
            } else {
                state
            };
            devices.push(DeviceView {
                id: d.id(),
                kind: d.kind,
                name: d.name.clone(),
                ip: d.ip,
                added: true,
                state,
                detail,
            });
        }
        for d in &self.found {
            let (state, detail) = live(d, State::New);
            devices.push(DeviceView {
                id: d.id(),
                kind: d.kind,
                name: d.name.clone(),
                ip: d.ip,
                added: false,
                state,
                detail,
            });
        }
        View {
            devices,
            scanning: self.scanning,
            model: self.config.model.clone(),
            model_state: self.model_state.clone(),
            groups: self.config.groups.clone(),
            ai_groups: self.config.ai_groups.clone(),
        }
    }

    fn say(&self, group: &str, text: String) {
        self.say_as(group, SIGNATURE, text);
    }

    fn say_as(&self, group: &str, signature: &str, text: String) {
        let _ = self.engine.send(Input::Ui(Command::SendGroup {
            group: group.to_string(),
            text: format!("{signature}{text}"),
            reply: None,
            from_home: true,
        }));
    }

    pub fn run(mut self, requests: Receiver<Request>) {
        self.publish();
        self.scan();
        for request in requests {
            self.handle(request);
            self.publish();
        }
    }

    fn handle(&mut self, request: Request) {
        match request {
            Request::Ask { group, from, text } => {
                if self.enabled(&group) {
                    let text: String = text.chars().take(MAX_REQUEST_CHARS).collect();
                    let answer = self.ask(&from, &text);
                    self.say(&group, answer);
                }
            }
            Request::Chat(question) => self.ai_ask(question),
            Request::Answered { question, asked, answer } => self.ai_answered(question, asked, answer),
            Request::Configure { group, args } => {
                let reply = self.configure(group.as_deref(), &args);
                self.note(reply);
            }
            Request::Scan => self.scan(),
            Request::Add(id) => self.add(&id),
            Request::Forget(id) => {
                if let Some(at) = self.config.devices.iter().position(|d| d.id() == id) {
                    let d = self.config.devices.remove(at);
                    self.live.remove(&id);
                    self.save();
                    self.note(format!("forgot {}", d.name));
                } else {
                    self.found.retain(|d| d.id() != id);
                }
            }
            Request::Test(id) => self.test(&id),
            Request::Rename(id, name) => {
                let name: String = name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
                if name.is_empty() {
                    return;
                }
                let taken = self.config.devices.iter().any(|d| d.id() != id && d.name.eq_ignore_ascii_case(&name));
                if taken {
                    return self.note(format!("there's already a device called {name}"));
                }
                if let Some(d) = self.config.devices.iter_mut().find(|d| d.id() == id) {
                    d.name = name;
                    self.save();
                } else if let Some(d) = self.found.iter_mut().find(|d| d.id() == id) {
                    d.name = name;
                }
            }
            Request::SetGroup { group, on } => {
                let verb = if on { "on" } else { "off" };
                let reply = self.configure(Some(&group), verb);
                self.note(reply);
            }
            Request::Found(found) => self.on_found(found),
            Request::ScanDone => self.scanning = false,
            Request::Checked { id, state, detail } => {
                // A check finishing must not overwrite a pairing in progress.
                if !matches!(self.live.get(&id), Some((State::Pairing { .. }, _))) {
                    self.live.insert(id, (state, detail));
                }
            }
            Request::Paired { id, result } => self.on_paired(&id, result),
            Request::ModelChecked(state) => self.model_state = state,
        }
    }

    // ----- finding and adding devices ----------------------------------------

    fn scan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        for d in &self.config.devices {
            self.live.entry(d.id()).or_insert((State::Checking, String::new()));
            self.check(d.clone());
        }
        let me = self.me.clone();
        let (url, model) = (self.config.ollama.clone(), self.config.model.clone());
        thread::spawn(move || {
            let _ = me.send(Request::ModelChecked(ollama::check(&url, &model)));
            let each = {
                let me = me.clone();
                move |found| {
                    let _ = me.send(Request::Found(found));
                }
            };
            discover::scan(each);
            let _ = me.send(Request::ScanDone);
        });
    }

    /// Looks at an added device in the background and reports its state.
    fn check(&self, d: Device) {
        let me = self.me.clone();
        thread::spawn(move || {
            let id = d.id();
            let (state, detail) = match d.kind {
                Kind::Light => match (wiz::Bulb { ip: d.ip }).status() {
                    Ok(status) => (State::Ready, status),
                    Err(_) => (State::Offline, "not answering".into()),
                },
                Kind::Tv => {
                    let tv = webos::Tv { ip: d.ip, mac: None, client_key: None, cert: None };
                    if tv.reachable() {
                        (State::Ready, d.model.clone().unwrap_or_else(|| "on".into()))
                    } else {
                        (State::Offline, "off or in standby".into())
                    }
                }
            };
            let _ = me.send(Request::Checked { id, state, detail });
        });
    }

    fn on_found(&mut self, f: discover::Found) {
        // Something we already have, perhaps at a new address.
        if let Some(d) = self.config.devices.iter_mut().find(|d| d.same(f.kind, f.ip, f.mac.as_deref())) {
            let mut changed = false;
            let mut moved = None;
            if d.ip != f.ip {
                moved = Some(format!("{} moved to {}", d.name, f.ip));
                d.ip = f.ip;
                changed = true;
            }
            if d.mac.is_none() && f.mac.is_some() {
                // The id changes with the MAC; carry its state over.
                if let Some(live) = self.live.remove(&d.id()) {
                    d.mac = f.mac.clone();
                    self.live.insert(d.id(), live);
                } else {
                    d.mac = f.mac.clone();
                }
                changed = true;
            }
            if d.model.is_none() && f.module.is_some() {
                d.model = f.module.clone();
                changed = true;
            }
            let d = d.clone();
            if changed {
                self.save();
            }
            if let Some(moved) = moved {
                self.note(moved);
            }
            self.check(d);
            return;
        }
        if self.found.iter().any(|d| d.same(f.kind, f.ip, f.mac.as_deref())) {
            return;
        }
        let base = match f.kind {
            Kind::Light => "Light",
            Kind::Tv => "TV",
        };
        let name = self.free_name(base);
        let detail = match f.kind {
            Kind::Light => format!("Wiz · {}", wiz::Caps::of(f.module.as_deref().unwrap_or_default()).describe()),
            Kind::Tv => "LG webOS TV".into(),
        };
        let d = Device {
            kind: f.kind,
            name,
            ip: f.ip,
            mac: f.mac,
            model: f.module,
            key: None,
            cert: None,
        };
        self.live.insert(d.id(), (State::New, detail));
        self.found.push(d);
    }

    /// `base`, or `base 2`, `base 3`… whichever no device, added or
    /// found, has yet.
    fn free_name(&self, base: &str) -> String {
        let taken = |n: &str| {
            self.config.devices.iter().chain(&self.found).any(|d| d.name.eq_ignore_ascii_case(n))
        };
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|i| format!("{base} {i}")).find(|n| !taken(n)).unwrap()
    }

    fn add(&mut self, id: &str) {
        let candidate = match self.found.iter().position(|d| d.id() == id) {
            Some(at) => Some(self.found[at].clone()),
            // An added TV whose pairing never finished can pair again.
            None => self
                .config
                .devices
                .iter()
                .find(|d| d.id() == id && d.kind == Kind::Tv && d.key.is_none())
                .cloned(),
        };
        let Some(d) = candidate else { return };
        match d.kind {
            Kind::Light => {
                self.found.retain(|f| f.id() != id);
                let name = d.name.clone();
                self.config.devices.push(d.clone());
                self.save();
                self.live.insert(d.id(), (State::Checking, String::new()));
                self.check(d);
                self.note(format!("added {name}"));
            }
            Kind::Tv => {
                if matches!(self.live.get(id), Some((State::Pairing { .. }, _))) {
                    return;
                }
                self.live.insert(
                    d.id(),
                    (State::Pairing { until: Instant::now() + PAIRING }, String::new()),
                );
                let me = self.me.clone();
                thread::spawn(move || {
                    let tv = webos::Tv { ip: d.ip, mac: None, client_key: None, cert: None };
                    let (result, learned) = tv.with(|s| {
                        let model = webos::model_name(s).unwrap_or_else(|_| "LG TV".into());
                        let _ = webos::toast(s, "hopchat is connected 👋");
                        Ok(model)
                    });
                    let result = result.map(|model| (learned, model));
                    let _ = me.send(Request::Paired { id: d.id(), result });
                });
            }
        }
    }

    fn on_paired(&mut self, id: &str, result: Result<(webos::Learned, String), String>) {
        match result {
            Ok((learned, model)) => {
                let mut d = match self.found.iter().position(|d| d.id() == id) {
                    Some(at) => self.found.remove(at),
                    None => match self.config.devices.iter().position(|d| d.id() == id) {
                        Some(at) => self.config.devices.remove(at),
                        None => return,
                    },
                };
                d.key = learned.client_key.or(d.key);
                d.cert = learned.cert.or(d.cert);
                d.model = Some(model.clone());
                if d.mac.is_none() {
                    d.mac = webos::mac_of(d.ip).map(|m| webos::format_mac(&m));
                }
                let name = d.name.clone();
                self.live.remove(id);
                self.live.insert(d.id(), (State::Ready, model));
                self.config.devices.push(d);
                self.save();
                self.note(format!("paired with {name}"));
            }
            Err(e) => {
                self.live.insert(id.to_string(), (State::Failed(e.clone()), String::new()));
                self.note(format!("pairing failed: {e}"));
            }
        }
    }

    fn test(&mut self, id: &str) {
        let Some(d) = self.config.devices.iter().find(|d| d.id() == id).cloned() else {
            return self.note("add it first, then test it".into());
        };
        let result = match d.kind {
            Kind::Light => (wiz::Bulb { ip: d.ip }).blink().map(|()| format!("{} blinked", d.name)),
            Kind::Tv => self
                .tv_do(id, |s| webos::toast(s, "👋 hello from hopchat"))
                .map(|_| format!("{} shows a hello", d.name)),
        };
        match result {
            Ok(text) => {
                self.note(text);
                self.check(d);
            }
            Err(e) => {
                self.live.insert(id.to_string(), (State::Failed(e.clone()), String::new()));
                self.note(format!("{}: {e}", d.name));
            }
        }
    }

    fn tv(&self, id: &str) -> Result<webos::Tv, String> {
        let d = self
            .config
            .devices
            .iter()
            .find(|d| d.id() == id && d.kind == Kind::Tv)
            .ok_or("no TV is set up")?;
        Ok(webos::Tv {
            ip: d.ip,
            mac: d.mac.as_deref().and_then(webos::parse_mac),
            client_key: d.key.clone(),
            cert: d.cert.clone(),
        })
    }

    /// Runs `f` against a TV and keeps the key and certificate it paired
    /// with.
    fn tv_do<T>(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut webos::Session) -> Result<T, String>,
    ) -> Result<T, String> {
        let tv = self.tv(id)?;
        let (result, learned) = tv.with(f);
        if let Some(d) = self.config.devices.iter_mut().find(|d| d.id() == id) {
            let mut changed = false;
            if learned.cert.is_some() && d.cert.is_none() {
                d.cert = learned.cert;
                changed = true;
            }
            if let Some(key) = learned.client_key {
                d.key = Some(key);
                changed = true;
            }
            if changed {
                self.save();
            }
        }
        result
    }

    // ----- /home ---------------------------------------------------------

    fn configure(&mut self, group: Option<&str>, args: &str) -> String {
        let mut words = args.split_whitespace();
        let verb = words.next().unwrap_or("status");
        let value = words.next();
        match (verb, value) {
            ("ai", verb) => self.ai_configure(group, verb.unwrap_or("status")),
            ("on", _) => {
                let Some(group) = group else {
                    return "open a group first: /home on lets that group use your devices".into();
                };
                if !self.enabled(group) {
                    self.config.groups.push(group.to_string());
                    self.save();
                }
                let names: Vec<String> = self.config.devices.iter().map(|d| d.name.clone()).collect();
                let what = if names.is_empty() { "the devices".to_string() } else { names.join(", ") };
                self.say(group, format!("is listening. Mention {MENTION} to use {what}."));
                "home is on in this group".into()
            }
            ("off", _) => {
                let Some(group) = group else { return "open the group to switch off".into() };
                self.config.groups.retain(|g| g != group);
                self.save();
                self.say(group, "has stopped listening.".into());
                "home is off in this group".into()
            }
            ("add" | "bulb" | "light" | "tv", Some(ip)) => match ip.parse::<Ipv4Addr>() {
                Ok(ip) => self.add_by_address(ip),
                Err(_) => format!("{ip} isn't an IPv4 address"),
            },
            ("model", Some(model)) => {
                self.config.model = model.to_string();
                self.save();
                self.model_state = ModelState::Unknown;
                let me = self.me.clone();
                let url = self.config.ollama.clone();
                let model = model.to_string();
                thread::spawn(move || {
                    let _ = me.send(Request::ModelChecked(ollama::check(&url, &model)));
                });
                format!("home will use {}", self.config.model)
            }
            ("scan", _) => {
                self.scan();
                "looking for devices…".into()
            }
            _ => format!(
                "home: {} device{} · model {} · on in {} group{} · /devices to manage",
                self.config.devices.len(),
                if self.config.devices.len() == 1 { "" } else { "s" },
                self.config.model,
                self.config.groups.len(),
                if self.config.groups.len() == 1 { "" } else { "s" },
            ),
        }
    }

    /// For a device the scan can't see (another subnet, broadcasts
    /// blocked): asks the address what it is, then offers it like a find.
    fn add_by_address(&mut self, ip: Ipv4Addr) -> String {
        if let Ok(system) = (wiz::Bulb { ip }).system() {
            let mac = Some(system.mac).filter(|m| !m.is_empty());
            self.on_found(discover::Found { kind: Kind::Light, ip, mac, module: Some(system.module) });
        } else if webos::Tv::probe(ip) {
            let mac = webos::mac_of(ip).map(|m| webos::format_mac(&m));
            self.on_found(discover::Found { kind: Kind::Tv, ip, mac, module: None });
        } else {
            return format!("nothing at {ip} answers as a Wiz light or an LG TV");
        }
        match self.found.iter().find(|d| d.ip == ip).map(Device::id) {
            Some(id) => {
                self.add(&id);
                format!("found a device at {ip}; adding it")
            }
            None => format!("{ip} is already added"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::assistant::{colour, flag, number, text};
    use super::ollama::{Ollama, Turn};
    use super::*;
    use serde_json::json;

    fn home_with(devices: Vec<Device>) -> Home {
        let (engine, _) = std::sync::mpsc::channel();
        let (ui, _) = std::sync::mpsc::channel();
        let (me, _) = std::sync::mpsc::channel();
        let mut home = Home::open(&std::env::temp_dir().join("hopchat-home-test-none"), engine, ui, me);
        home.config = Config { devices, ..Default::default() };
        home
    }

    fn light(name: &str, ip: [u8; 4], module: &str) -> Device {
        Device {
            kind: Kind::Light,
            name: name.into(),
            ip: ip.into(),
            mac: Some(format!("mac-{name}")),
            model: Some(module.into()),
            key: None,
            cert: None,
        }
    }

    #[test]
    fn mentions() {
        assert_eq!(addressed("@home lights off").as_deref(), Some("lights off"));
        assert_eq!(addressed("hey @Home, put on netflix").as_deref(), Some("hey put on netflix"));
        assert_eq!(addressed("dim it @home").as_deref(), Some("dim it"));
        assert_eq!(addressed("ask @homer"), None);
        assert_eq!(addressed("no mention"), None);
    }

    /// A home whose messages to the engine can be read.
    fn wired_home() -> (Home, std::sync::mpsc::Receiver<Input>) {
        let (engine, from_home) = std::sync::mpsc::channel();
        let (ui, _) = std::sync::mpsc::channel();
        let (me, _) = std::sync::mpsc::channel();
        let mut home = Home::open(&std::env::temp_dir().join("hopchat-home-test-none"), engine, ui, me);
        home.config = Config::default();
        (home, from_home)
    }

    #[test]
    fn ai_mentions() {
        assert_eq!(ai::addressed("@ai what's a monad?").as_deref(), Some("what's a monad?"));
        assert_eq!(ai::addressed("hey @AI: translate this").as_deref(), Some("hey translate this"));
        assert_eq!(ai::addressed("said@aim"), None);
        assert_eq!(ai::addressed("@home lights off"), None);
    }

    #[test]
    fn older_settings_have_no_ai_groups() {
        let config: Config = serde_json::from_value(json!({"groups": ["g"]})).unwrap();
        assert!(config.ai_groups.is_empty());
    }

    #[test]
    fn ai_answers_reply_to_the_question_and_remember() {
        let (mut home, engine) = wired_home();
        home.config.ai_groups.push("g".into());
        for n in 0..20 {
            let q = ai::Question {
                group: "g".into(),
                from: "bob".into(),
                id: format!("m{n}"),
                sender: "off1bob".into(),
                text: format!("question {n}"),
            };
            let asked = json!({"role": "user", "content": format!("bob: question {n}")});
            home.handle(Request::Answered { question: q, asked, answer: Ok(format!("answer {n}")) });
        }
        let Ok(Input::Ui(Command::SendGroup { group, text, reply: Some(quote), from_home: true })) = engine.try_recv() else {
            panic!("expected an answer in the group");
        };
        assert_eq!((group.as_str(), text.as_str()), ("g", "🤖 answer 0"));
        assert_eq!((quote.id.as_str(), quote.sender.as_str()), ("m0", "off1bob"));
        let memory = &home.memory["g"];
        assert_eq!(memory.len(), 24, "twelve questions and their answers");
        assert_eq!(memory.last().unwrap()["content"], "answer 19");
        // Switched off: no answer, and the memory goes.
        home.handle(Request::Configure { group: Some("g".into()), args: "ai off".into() });
        assert!(!home.memory.contains_key("g"));
    }

    #[test]
    fn old_settings_become_devices() {
        let old = json!({"bulb": "192.168.1.10", "tv": "192.168.1.30", "tv_key": "k", "groups": ["g"]});
        let mut config: Config = serde_json::from_value(old).unwrap();
        config.migrate();
        assert_eq!(config.devices.len(), 2);
        assert_eq!(config.devices[1].key.as_deref(), Some("k"));
        let saved = serde_json::to_value(&config).unwrap();
        assert!(saved.get("bulb").is_none(), "legacy fields are not written back");
    }

    #[test]
    fn found_devices_get_free_names_and_follow_their_mac() {
        let mut home = home_with(vec![light("Light", [10, 0, 0, 5], "ESP01_SHTW1C_31")]);
        home.on_found(discover::Found {
            kind: Kind::Light,
            ip: [10, 0, 0, 6].into(),
            mac: Some("other".into()),
            module: Some("ESP01_SHRGB1C_31".into()),
        });
        assert_eq!(home.found[0].name, "Light 2");
        assert_eq!(home.live[&home.found[0].id()].1, "Wiz · colour");
        // The added light, answering from a new address.
        home.on_found(discover::Found {
            kind: Kind::Light,
            ip: [10, 0, 0, 9].into(),
            mac: Some("mac-Light".into()),
            module: None,
        });
        assert_eq!(home.config.devices[0].ip, Ipv4Addr::new(10, 0, 0, 9));
        assert_eq!(home.found.len(), 1, "a known device is not offered again");
    }

    #[test]
    fn tools_follow_what_the_lights_can_do() {
        let home = home_with(vec![light("Desk", [10, 0, 0, 5], "ESP01_SHTW1C_31")]);
        let tools = home.tools().to_string();
        assert!(tools.contains("kelvin"));
        assert!(!tools.contains("\"color\""), "a white-tones bulb is offered no colours");
        let home = home_with(vec![
            light("Desk", [10, 0, 0, 5], "ESP01_SHTW1C_31"),
            light("Strip", [10, 0, 0, 6], "ESP03_SHRGB1C_01"),
        ]);
        let tools = home.tools().to_string();
        assert!(tools.contains("\"color\"") && tools.contains("\"Strip\"") && tools.contains("\"all\""));
        assert!(home.system_prompt().contains("\"Desk\" (white tones)"));
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

    /// Asks the real local model what it would do, without doing it.
    /// `cargo test -- --ignored` with `ollama serve` running.
    #[test]
    #[ignore = "needs a local Ollama with the model pulled"]
    fn model_picks_the_right_tools() {
        let mut tv = light("Living room", [10, 0, 0, 2], "");
        tv.kind = Kind::Tv;
        tv.key = Some("k".into());
        let home = home_with(vec![
            light("Desk lamp", [10, 0, 0, 1], "ESP01_SHTW1C_31"),
            light("Strip", [10, 0, 0, 3], "ESP03_SHRGB1C_01"),
            tv,
        ]);
        let model = Ollama::new(home.config.ollama.clone(), home.config.model.clone());
        let cases: &[(&str, &[&str])] = &[
            ("turn off the lights", &["light_set"]),
            ("make the strip purple", &["light_set"]),
            ("dim the desk lamp to 20% and put on netflix", &["light_set", "tv_open_app"]),
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
}

