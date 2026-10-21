//! A local model behind Ollama's `/api/chat`, with tool calling. Nothing
//! leaves the machine.

use std::time::Duration;

use serde_json::{json, Value};

/// How long one model turn may take; a cold 7B load on a laptop is slow.
const TURN: Duration = Duration::from_secs(120);

/// Whether Ollama answers at `url` and has `model` pulled.
pub fn check(url: &str, model: &str) -> super::ModelState {
    use super::ModelState;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .new_agent();
    let Ok(mut response) = agent.get(format!("{}/api/tags", url.trim_end_matches('/'))).call() else {
        return ModelState::NoServer;
    };
    let Ok(tags) = response.body_mut().read_json::<Value>() else {
        return ModelState::NoServer;
    };
    let wanted = if model.contains(':') { model.to_string() } else { format!("{model}:latest") };
    let pulled = tags
        .get("models")
        .and_then(Value::as_array)
        .is_some_and(|models| {
            models.iter().any(|m| m.get("name").and_then(Value::as_str) == Some(wanted.as_str()))
        });
    if pulled { ModelState::Ready } else { ModelState::Missing }
}

pub struct Ollama {
    pub url: String,
    pub model: String,
    agent: ureq::Agent,
}

/// One tool call the model asked for.
pub struct Call {
    pub name: String,
    pub args: Value,
}

pub enum Turn {
    Say(String),
    Act(Vec<Call>, Value),
}

impl Ollama {
    pub fn new(url: String, model: String) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(TURN))
            .build()
            .new_agent();
        Self { url, model, agent }
    }

    /// One model turn over `messages`; `Act` carries the assistant message
    /// to append before the tool results.
    pub fn turn(&self, messages: &[Value], tools: &Value) -> Result<Turn, String> {
        let request = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "stream": false,
            "options": {"temperature": 0},
        });
        let mut response = self
            .agent
            .post(format!("{}/api/chat", self.url.trim_end_matches('/')))
            .send_json(&request)
            .map_err(|e| match e {
                ureq::Error::StatusCode(404) => format!(
                    "model {} isn't installed; run `ollama pull {}`",
                    self.model, self.model
                ),
                ureq::Error::StatusCode(code) => format!("Ollama answered HTTP {code}"),
                _ => format!("can't reach Ollama at {} (is `ollama serve` running?)", self.url),
            })?;
        let body: Value = response.body_mut().read_json().map_err(|e| e.to_string())?;
        let message = body.get("message").cloned().ok_or("Ollama sent no message")?;
        let calls: Vec<Call> = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|c| {
                        let f = c.get("function")?;
                        let args = match f.get("arguments")? {
                            // Some models send the arguments as a JSON string.
                            Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                            other => other.clone(),
                        };
                        Some(Call { name: f.get("name")?.as_str()?.to_string(), args })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if calls.is_empty() {
            let text = message.get("content").and_then(Value::as_str).unwrap_or_default();
            Ok(Turn::Say(text.trim().to_string()))
        } else {
            Ok(Turn::Act(calls, message))
        }
    }
}
