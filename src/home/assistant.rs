//! The model's side of home: what it is told, the tools it may call, and
//! running those tools against the devices that have been added.

use serde_json::{json, Value};

use super::ollama::{Ollama, Turn};
use super::{webos, wiz, Device, Home, Kind};

/// Model turns per request: act, maybe act again on what it saw, then say.
const MAX_TURNS: usize = 4;

impl Home {
    pub(super) fn ask(&mut self, from: &str, text: &str) -> String {
        if self.lights().is_empty() && self.tvs().is_empty() {
            return "no devices are set up yet; whoever hosts home can add them with /devices.".into();
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
                        let text = match self.run_tool(&call.name, &call.args) {
                            Ok(t) => t,
                            Err(e) => format!("error: {e}"),
                        };
                        done.push(text.clone());
                        messages.push(json!({"role": "tool", "tool_name": call.name, "content": text}));
                    }
                }
                Err(e) => {
                    self.note(format!("home: {e}"));
                    return if done.is_empty() {
                        format!("can't think right now: {e}")
                    } else {
                        done.join("; ")
                    };
                }
            }
        }
        if done.is_empty() {
            "I'm not sure what to do with that.".into()
        } else {
            done.join("; ")
        }
    }

    fn lights(&self) -> Vec<&Device> {
        self.config.devices.iter().filter(|d| d.kind == Kind::Light).collect()
    }

    /// TVs that finished pairing; one without a key can't be driven.
    fn tvs(&self) -> Vec<&Device> {
        self.config
            .devices
            .iter()
            .filter(|d| d.kind == Kind::Tv && d.key.is_some())
            .collect()
    }

    pub(super) fn system_prompt(&self) -> String {
        let mut devices: Vec<String> = self
            .lights()
            .iter()
            .map(|d| format!("- light \"{}\" ({})", d.name, d.caps().describe()))
            .collect();
        devices.extend(self.tvs().iter().map(|d| format!("- TV \"{}\" (LG webOS)", d.name)));
        let several = self.lights().len() > 1;
        format!(
            "You are home, the assistant for a household's devices, answering in a group chat. \
             The devices are:\n{}\n\
             Each message starts with the sender's name. Use the tools to do what they ask, and \
             check state with a status tool when they ask how things are. Never invent device \
             state. {}A light that only does white tones cannot show colours; say so instead of \
             trying. After acting, reply with one short, friendly sentence saying what you did. \
             If a request has nothing to do with the devices, say briefly that you only handle \
             the devices. Light scenes: {}.",
            devices.join("\n"),
            if several {
                "When they say \"the lights\" without naming one, use all of them. "
            } else {
                ""
            },
            wiz::SCENES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "),
        )
    }

    pub(super) fn tools(&self) -> Value {
        let tool = |name: &str, description: &str, properties: Value| {
            json!({"type": "function", "function": {
                "name": name,
                "description": description,
                "parameters": {"type": "object", "properties": properties},
            }})
        };
        let mut tools = Vec::new();
        let lights = self.lights();
        if !lights.is_empty() {
            let mut names: Vec<String> = lights.iter().map(|d| d.name.clone()).collect();
            names.push("all".into());
            let mut props = json!({
                "light": {"type": "string", "enum": names, "description": "which light; all for every one"},
                "on": {"type": "boolean", "description": "true to switch on, false to switch off"},
                "brightness": {"type": "integer", "description": "percent, 10 to 100"},
                "scene": {"type": "string", "description": "one of the light scene names"},
            });
            if lights.iter().any(|d| d.caps().white) {
                props["kelvin"] = json!({"type": "integer", "description": "white temperature, 2200 (warm) to 6500 (cool)"});
            }
            if lights.iter().any(|d| d.caps().colour) {
                props["color"] = json!({"type": "string", "description": "a colour name like red or purple, or #rrggbb"});
            }
            tools.push(tool(
                "light_set",
                "Change a light. Give only what should change. Setting brightness, temperature, \
                 colour or a scene also turns it on.",
                props,
            ));
            tools.push(tool(
                "light_status",
                "Read what the lights are doing now.",
                json!({"light": {"type": "string", "description": "which light; omit for all"}}),
            ));
        }
        let tvs = self.tvs();
        if !tvs.is_empty() {
            let pick = |mut props: Value| {
                if tvs.len() > 1 {
                    let names: Vec<String> = tvs.iter().map(|d| d.name.clone()).collect();
                    props["tv"] = json!({"type": "string", "enum": names});
                }
                props
            };
            tools.push(tool("tv_power", "Turn the TV on or off.", pick(json!({"on": {"type": "boolean"}}))));
            tools.push(tool(
                "tv_volume",
                "Set, raise, lower or mute the TV's volume.",
                pick(json!({
                    "level": {"type": "integer", "description": "absolute volume, 0 to 100"},
                    "change": {"type": "integer", "description": "steps up (positive) or down (negative)"},
                    "mute": {"type": "boolean"},
                })),
            ));
            tools.push(tool(
                "tv_open_app",
                "Open an app on the TV, such as Netflix, YouTube, Prime Video, Disney+, Spotify or Live TV.",
                pick(json!({"app": {"type": "string"}})),
            ));
            tools.push(tool(
                "tv_input",
                "Switch the TV's input, such as HDMI_1.",
                pick(json!({"input": {"type": "string"}})),
            ));
            tools.push(tool(
                "tv_media",
                "Control what is playing.",
                pick(json!({"action": {"type": "string", "enum": ["play", "pause", "stop", "rewind", "fastForward"]}})),
            ));
            tools.push(tool(
                "tv_message",
                "Pop up a short message on the TV screen.",
                pick(json!({"text": {"type": "string"}})),
            ));
            tools.push(tool("tv_status", "Read what the TV is showing and its volume.", pick(json!({}))));
        }
        Value::Array(tools)
    }

    pub(super) fn run_tool(&mut self, name: &str, args: &Value) -> Result<String, String> {
        self.note(format!("home: {name} {args}"));
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
                let lights = self.pick_lights(text(args, "light").as_deref())?;
                let results: Vec<String> = lights
                    .iter()
                    .map(|d| {
                        let caps = d.caps();
                        if pilot.rgb.is_some() && !caps.colour {
                            return format!("{} only does white tones, not colours", d.name);
                        }
                        if pilot.kelvin.is_some() && !caps.white {
                            return format!("{} only dims; it has no white temperature", d.name);
                        }
                        let bulb = wiz::Bulb { ip: d.ip };
                        match bulb.set(&pilot).and_then(|()| bulb.status()) {
                            Ok(state) => format!("{} is now {state}", d.name),
                            Err(e) => format!("{}: {e}", d.name),
                        }
                    })
                    .collect();
                Ok(results.join("; "))
            }
            "light_status" => {
                let lights = self.pick_lights(text(args, "light").as_deref())?;
                let results: Vec<String> = lights
                    .iter()
                    .map(|d| match (wiz::Bulb { ip: d.ip }).status() {
                        Ok(state) => format!("{} is {state}", d.name),
                        Err(e) => format!("{}: {e}", d.name),
                    })
                    .collect();
                Ok(results.join("; "))
            }
            "tv_power" => {
                let id = self.pick_tv(args)?;
                match flag(args, "on") {
                    Some(true) => {
                        if self.tv_do(&id, webos::status).is_ok() {
                            return Ok("the TV is already on".into());
                        }
                        self.tv(&id)?.wake()?;
                        Ok("sent the TV a wake-up signal".into())
                    }
                    _ => self.tv_do(&id, webos::power_off),
                }
            }
            "tv_volume" => {
                let id = self.pick_tv(args)?;
                let level = number(args, "level").map(|l| l.clamp(0.0, 100.0) as u8);
                let change = number(args, "change").map(|c| c.clamp(-20.0, 20.0) as i8);
                let mute = flag(args, "mute");
                self.tv_do(&id, |s| webos::volume(s, level, change, mute))
            }
            "tv_open_app" => {
                let id = self.pick_tv(args)?;
                let app = text(args, "app").ok_or("which app?")?;
                self.tv_do(&id, |s| webos::launch(s, &app))
            }
            "tv_input" => {
                let id = self.pick_tv(args)?;
                let input = text(args, "input").ok_or("which input?")?;
                self.tv_do(&id, |s| webos::input(s, &input))
            }
            "tv_media" => {
                let id = self.pick_tv(args)?;
                let action = text(args, "action").ok_or("which action?")?;
                self.tv_do(&id, |s| webos::media(s, &action))
            }
            "tv_message" => {
                let id = self.pick_tv(args)?;
                let message = text(args, "text").ok_or("what should it say?")?;
                self.tv_do(&id, |s| webos::toast(s, &message))
            }
            "tv_status" => {
                let id = self.pick_tv(args)?;
                self.tv_do(&id, webos::status)
            }
            other => Err(format!("there's no tool called {other}")),
        }
    }

    /// The lights a request names; none named, or "all", means every one.
    fn pick_lights(&self, name: Option<&str>) -> Result<Vec<Device>, String> {
        let lights = self.lights();
        let wanted = name.map(str::to_ascii_lowercase).filter(|n| n != "all" && !n.is_empty());
        let picked: Vec<Device> = match &wanted {
            None => lights.into_iter().cloned().collect(),
            Some(n) => lights
                .into_iter()
                .filter(|d| d.name.to_ascii_lowercase().contains(n.as_str()))
                .cloned()
                .collect(),
        };
        if picked.is_empty() {
            return Err(match wanted {
                Some(n) => format!("there's no light called {n}"),
                None => "no lights are set up".into(),
            });
        }
        Ok(picked)
    }

    fn pick_tv(&self, args: &Value) -> Result<String, String> {
        let tvs = self.tvs();
        let found = match text(args, "tv").map(|t| t.to_ascii_lowercase()) {
            Some(n) => tvs.into_iter().find(|d| d.name.to_ascii_lowercase().contains(&n)),
            None => tvs.into_iter().next(),
        };
        found.map(Device::id).ok_or_else(|| "no TV is set up".into())
    }
}

// ----- lenient argument reading: small models send "50" as often as 50 ----

pub(super) fn number(args: &Value, key: &str) -> Option<f64> {
    match args.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches(['%', 'K', 'k']).parse().ok(),
        _ => None,
    }
}

pub(super) fn flag(args: &Value, key: &str) -> Option<bool> {
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

pub(super) fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key)?.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

pub(super) fn colour(name: &str) -> Option<(u8, u8, u8)> {
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
