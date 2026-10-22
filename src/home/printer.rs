//! Printers and copiers. The tool the model gets for every printer,
//! whatever its brand, and what a printer backend has to do to answer it.
//!
//! Only reading: what it is doing, its ink or toner, and anything wrong.
//! Printing is left to the computers that send the jobs.

use serde_json::{json, Value};

use super::assistant::{text, tool};
use super::driver::backend_of;
use super::{Device, Home};

pub trait PrinterDriver: Sync {
    /// What it is doing, its supplies, and anything wrong, in a few words.
    fn status(&self, d: &Device) -> Result<String, String>;
}

type Printer = (&'static dyn PrinterDriver, Device);

impl Home {
    pub(super) fn printers(&self) -> Vec<Printer> {
        self.config
            .devices
            .iter()
            .filter_map(|d| Some((backend_of(d)?.as_printer()?, self.reach(d))))
            .collect()
    }

    pub(super) fn printer_tools(&self) -> Vec<Value> {
        let printers = self.printers();
        if printers.is_empty() {
            return Vec::new();
        }
        let names: Vec<String> = printers.iter().map(|(_, d)| d.name.clone()).collect();
        vec![tool(
            "printer_status",
            "Read a printer or copier's state: idle or printing, ink or toner levels, and problems such as a paper jam or no paper.",
            json!({"printer": {"type": "string", "enum": names, "description": "which printer"}}),
        )]
    }

    /// Runs a printer tool; `None` when `name` isn't one.
    pub(super) fn run_printer(&mut self, name: &str, args: &Value) -> Option<Result<String, String>> {
        Some(match name {
            "printer_status" => self.pick_printer(args).and_then(|(p, d)| Ok(format!("{} is {}", d.name, p.status(&d)?))),
            _ => return None,
        })
    }

    /// The printer a request names, or the only one.
    fn pick_printer(&self, args: &Value) -> Result<Printer, String> {
        let mut printers = self.printers();
        let Some(n) = text(args, "printer").map(|n| n.to_ascii_lowercase()) else {
            return match printers.len() {
                0 => Err("no printers are set up".into()),
                1 => Ok(printers.remove(0)),
                _ => Err("say which printer".into()),
            };
        };
        if let Some(at) = printers.iter().position(|(_, d)| d.name.eq_ignore_ascii_case(&n)) {
            return Ok(printers.remove(at));
        }
        let mut close: Vec<Printer> =
            printers.into_iter().filter(|(_, d)| d.name.to_ascii_lowercase().contains(&n)).collect();
        match close.len() {
            0 => Err(format!("there's no printer called {n}")),
            1 => Ok(close.remove(0)),
            _ => Err(format!("which {n}? {}", close.iter().map(|(_, d)| d.name.as_str()).collect::<Vec<_>>().join(", "))),
        }
    }
}
