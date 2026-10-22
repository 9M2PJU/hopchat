//! Switches: plugs and anything else that is only on or off. The tools
//! the model gets for every switch, whatever its brand, and what a switch
//! backend has to do to answer them.
//!
//! There is no "all" here, as there is for lights: a plug may be running
//! the fridge.

use serde_json::{json, Value};

use super::assistant::{flag, text, tool};
use super::driver::backend_of;
use super::{Device, Home};

pub trait SwitchDriver: Sync {
    /// Switches it, and says whether it is on when the answer to the
    /// change said so; `None` to ask with [`status`](Self::status).
    fn set(&self, d: &Device, on: bool) -> Result<Option<String>, String>;
    /// Whether it is on, in a word or two.
    fn status(&self, d: &Device) -> Result<String, String>;
}

type Switch = (&'static dyn SwitchDriver, Device);

impl Home {
    pub(super) fn switches(&self) -> Vec<Switch> {
        self.config
            .devices
            .iter()
            .filter_map(|d| Some((backend_of(d)?.as_switch()?, d.clone())))
            .collect()
    }

    pub(super) fn switch_tools(&self) -> Vec<Value> {
        let switches = self.switches();
        if switches.is_empty() {
            return Vec::new();
        }
        let names: Vec<String> = switches.iter().map(|(_, d)| d.name.clone()).collect();
        let which = json!({"type": "string", "enum": names, "description": "which switch"});
        vec![
            tool(
                "switch_set",
                "Turn a switch or plug on or off.",
                json!({"switch": which, "on": {"type": "boolean", "description": "true to switch on, false to switch off"}}),
            ),
            tool("switch_status", "Read whether a switch or plug is on.", json!({"switch": which})),
        ]
    }

    /// Runs a switch tool; `None` when `name` isn't one.
    pub(super) fn run_switch(&mut self, name: &str, args: &Value) -> Option<Result<String, String>> {
        Some(match name {
            "switch_set" => match flag(args, "on") {
                Some(on) => self.pick_switch(args).and_then(|(s, d)| {
                    // Never say it is off when that couldn't be seen: it may run the fridge.
                    let state = match s.set(&d, on)? {
                        Some(told) => Ok(told),
                        None => s.status(&d),
                    };
                    Ok(match state {
                        Ok(state) => format!("{} is now {state}", d.name),
                        Err(e) => format!("switched {} {}, but couldn't read it back: {e}", d.name, if on { "on" } else { "off" }),
                    })
                }),
                None => Err("say whether to switch it on or off".into()),
            },
            "switch_status" => self.pick_switch(args).and_then(|(s, d)| Ok(format!("{} is {}", d.name, s.status(&d)?))),
            _ => return None,
        })
    }

    /// The switch a request names, or the only one.
    pub(super) fn pick_switch(&self, args: &Value) -> Result<Switch, String> {
        let mut switches = self.switches();
        let Some(n) = text(args, "switch").map(|n| n.to_ascii_lowercase()) else {
            return match switches.len() {
                0 => Err("no switches are set up".into()),
                1 => Ok(switches.remove(0)),
                _ => Err("say which switch".into()),
            };
        };
        if let Some(at) = switches.iter().position(|(_, d)| d.name.eq_ignore_ascii_case(&n)) {
            return Ok(switches.remove(at));
        }
        let mut close: Vec<Switch> =
            switches.into_iter().filter(|(_, d)| d.name.to_ascii_lowercase().contains(&n)).collect();
        match close.len() {
            0 => Err(format!("there's no switch called {n}")),
            1 => Ok(close.remove(0)),
            _ => Err(format!("which {n}? {}", close.iter().map(|(_, d)| d.name.as_str()).collect::<Vec<_>>().join(", "))),
        }
    }
}
