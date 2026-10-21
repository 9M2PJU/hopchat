//! A local model behind Ollama's `/api/chat`, with tool calling. Nothing
//! leaves the network: at most it is lent to a group member (see `lend`).

use std::time::Duration;

use serde_json::{json, Value};

/// How long one model turn may take; a cold 7B load on a laptop is slow.
const TURN: Duration = Duration::from_secs(120);
/// How long a free-form answer may take: longer, since it is written out
/// in full before anyone sees it.
const ANSWER: Duration = Duration::from_secs(300);

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

/// One model turn; `Act` carries the assistant message to append before
/// the tool results.
pub enum Turn {
    Say(String),
    Act(Vec<Call>, Value),
}

impl Turn {
    /// Reads an assistant message, ours or one a lender sent.
    pub fn from_message(message: Value) -> Turn {
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
            Turn::Say(text_of(&message))
        } else {
            Turn::Act(calls, message)
        }
    }
}

pub fn text_of(message: &Value) -> String {
    message.get("content").and_then(Value::as_str).unwrap_or_default().trim().to_string()
}

impl Ollama {
    pub fn new(url: String, model: String) -> Self {
        Self::with_timeout(url, model, TURN)
    }

    /// For plain answers, which are written out whole.
    pub fn patient(url: String, model: String) -> Self {
        Self::with_timeout(url, model, ANSWER)
    }

    fn with_timeout(url: String, model: String, timeout: Duration) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build()
            .new_agent();
        Self { url, model, agent }
    }

    /// The assistant message the model writes for `messages`: with
    /// `tools`, a deterministic turn that may call them.
    pub fn raw(&self, messages: &[Value], tools: Option<&Value>) -> Result<Value, String> {
        let mut request = json!({
            "model": self.model,
            "messages": messages,
            "stream": false,
        });
        if let Some(tools) = tools {
            request["tools"] = tools.clone();
            request["options"] = json!({"temperature": 0});
        }
        self.post(&request)
    }

    /// Sends one `/api/chat` request and returns the assistant message.
    fn post(&self, request: &Value) -> Result<Value, String> {
        let mut response = self
            .agent
            .post(format!("{}/api/chat", self.url.trim_end_matches('/')))
            .send_json(request)
            .map_err(|e| match e {
                ureq::Error::StatusCode(404) => format!(
                    "model {} isn't installed; run `ollama pull {}`",
                    self.model, self.model
                ),
                ureq::Error::StatusCode(code) => format!("Ollama answered HTTP {code}"),
                ureq::Error::Timeout(_) => "the model took too long to answer".into(),
                _ => format!("can't reach Ollama at {} (is `ollama serve` running?)", self.url),
            })?;
        let body: Value = response.body_mut().read_json().map_err(|e| e.to_string())?;
        body.get("message").cloned().ok_or_else(|| "Ollama sent no message".into())
    }
}
