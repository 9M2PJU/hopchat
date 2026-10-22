//! Lights Home Assistant drives.

use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use super::{flag, is_off, called, check_entity, entity, Hub};
use crate::home::driver::{Backend, Category};
use crate::home::{Device, State};
use crate::home::light::{scene_key, Caps, LightDriver, Pilot};

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
pub(super) fn describe_light(state: &Value) -> String {
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

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::super::describe;
    use super::*;

    #[test]
    fn an_on_off_light_says_it_only_switches() {
        let state = json!({"state": "off", "attributes": {"supported_color_modes": ["onoff"]}});
        let (backend, data, _) = describe("light.porch", &state).unwrap();
        let d = Device::new(backend, "Porch".into(), Ipv4Addr::UNSPECIFIED, None, data);
        assert_eq!(HaLight.about(&d), "on or off only");
        let older = Device::new("ha_light", "Hall".into(), Ipv4Addr::UNSPECIFIED, None, json!({}));
        assert_eq!(HaLight.about(&older), "dimmable", "listed before dims was kept");
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
}
