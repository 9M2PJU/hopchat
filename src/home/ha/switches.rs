//! Plugs and switches Home Assistant drives.

use serde_json::{json, Value};

use super::{called, check_entity, entity, Hub};
use crate::home::driver::{Backend, Category};
use crate::home::{Device, State};
use crate::home::switch::SwitchDriver;

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

pub(super) fn describe_switch(state: &Value) -> String {
    state.get("state").and_then(Value::as_str).unwrap_or("unknown").to_string()
}
