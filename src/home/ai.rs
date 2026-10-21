//! `@ai`: lets the people in a group ask the model running on this
//! machine anything, not just about the devices.
//!
//! Answers are written on a worker thread, so a long one doesn't hold up
//! the devices, and each goes back as a reply to the question it answers.
//! Each group keeps a short memory of what was asked and answered, so
//! "and in French?" works.

use std::thread;

use serde_json::{json, Value};

use super::ollama::Ollama;
use super::{Home, Request};
use crate::engine::{Command, Input, Quote};

/// The word that addresses the model in a group.
pub const MENTION: &str = "@ai";
/// What every answer from the model starts with.
pub const SIGNATURE: &str = "🤖 ";
const MAX_QUESTION_CHARS: usize = 2000;
/// Long enough for a real answer, short enough to read in a chat.
const MAX_ANSWER_CHARS: usize = 4000;
/// Questions and answers each group's memory keeps.
const MEMORY: usize = 12;

/// A group message that mentioned [`MENTION`].
pub struct Question {
    pub group: String,
    pub from: String,
    /// The message's id and sender, so the answer can reply to it.
    pub id: String,
    pub sender: String,
    pub text: String,
}

impl Home {
    pub fn ai_enabled(&self, group: &str) -> bool {
        self.config.ai_groups.iter().any(|g| g == group)
    }

    pub(super) fn ai_ask(&mut self, q: Question) {
        if !self.ai_enabled(&q.group) {
            return;
        }
        let text: String = q.text.chars().take(MAX_QUESTION_CHARS).collect();
        if text.is_empty() {
            return self.ai_say(&q, "ask me something after @ai.".into());
        }
        let mut messages = vec![json!({"role": "system", "content": self.ai_prompt()})];
        messages.extend(self.memory.get(&q.group).cloned().unwrap_or_default());
        let asked = json!({"role": "user", "content": format!("{}: {text}", q.from)});
        messages.push(asked.clone());
        let model = Ollama::patient(self.config.ollama.clone(), self.config.model.clone());
        let me = self.me.clone();
        thread::spawn(move || {
            let answer = model.chat(&messages);
            let _ = me.send(Request::Answered { question: q, asked, answer });
        });
    }

    pub(super) fn ai_answered(&mut self, q: Question, asked: Value, answer: Result<String, String>) {
        // Switched off while it was thinking.
        if !self.ai_enabled(&q.group) {
            return;
        }
        let text = match answer {
            Ok(text) if !text.is_empty() => {
                let memory = self.memory.entry(q.group.clone()).or_default();
                memory.push(asked);
                memory.push(json!({"role": "assistant", "content": text}));
                let excess = memory.len().saturating_sub(MEMORY * 2);
                memory.drain(..excess);
                clip(&text)
            }
            Ok(_) => "I don't have an answer to that.".into(),
            Err(e) => {
                self.note(format!("ai: {e}"));
                format!("can't think right now: {e}")
            }
        };
        self.ai_say(&q, text);
    }

    fn ai_say(&self, q: &Question, text: String) {
        let _ = self.engine.send(Input::Ui(Command::SendGroup {
            group: q.group.clone(),
            text: format!("{SIGNATURE}{text}"),
            reply: Some(Quote {
                id: q.id.clone(),
                sender: q.sender.clone(),
                text: q.text.chars().take(80).collect(),
            }),
            from_home: true,
        }));
    }

    /// `/ai on|off` in a group, and the bare `/ai` status.
    pub(super) fn ai_configure(&mut self, group: Option<&str>, verb: &str) -> String {
        match verb {
            "on" => {
                let Some(group) = group else {
                    return "open a group first: /ai on lets that group ask your model".into();
                };
                if !self.ai_enabled(group) {
                    self.config.ai_groups.push(group.to_string());
                    self.save();
                }
                self.say_as(
                    group,
                    SIGNATURE,
                    format!("{} is here. Mention {MENTION} to ask it anything.", self.config.model),
                );
                format!("this group can ask {} now", self.config.model)
            }
            "off" => {
                let Some(group) = group else { return "open the group to switch off".into() };
                self.config.ai_groups.retain(|g| g != group);
                self.memory.remove(group);
                self.save();
                self.say_as(group, SIGNATURE, "has gone quiet.".into());
                "this group can't ask your model any more".into()
            }
            "forget" => {
                let Some(group) = group else { return "open the group whose chat to forget".into() };
                self.memory.remove(group);
                "forgot this group's chat with the model".into()
            }
            _ => {
                let n = self.config.ai_groups.len();
                let here = match group {
                    Some(g) if self.ai_enabled(g) => " · on here",
                    Some(_) => " · off here",
                    None => "",
                };
                format!(
                    "ai: model {} · on in {n} group{}{here} · /ai on|off in a group",
                    self.config.model,
                    if n == 1 { "" } else { "s" },
                )
            }
        }
    }

    fn ai_prompt(&self) -> String {
        format!(
            "You are {}, a helpful assistant running on one person's computer, answering in a \
             group chat on their local network. Each message starts with the sender's name; \
             several people may be talking to you. Answer the latest message directly and \
             concisely, in plain text that reads well in a terminal: no tables, little or no \
             markdown. You have no internet access and can't see anything outside this chat.",
            self.config.model,
        )
    }
}

/// Whether `text` addresses the model, and the question with the mention
/// removed.
pub fn addressed(text: &str) -> Option<String> {
    super::mentioned(text, MENTION)
}

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_ANSWER_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_ANSWER_CHARS).collect();
    format!("{}…", cut.trim_end())
}
