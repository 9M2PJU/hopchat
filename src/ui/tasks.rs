//! The todos and reminders panel: one line to type into, and the list.
//!
//! Whatever is typed becomes a todo, or a reminder if it says when ("call
//! mom at 5pm"); the line under it says which before Enter. ↓ goes into
//! the list to check things off, snooze, edit or delete them.

use chrono::Local;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::editor::Editor;
use crate::tasks::{Draft, Origin, Task, Tasks, TEXT_MAX};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Focus {
    Input,
    List,
}

pub struct TaskPanel {
    pub input: Editor,
    pub focus: Focus,
    /// Index into [`Tasks::ordered`].
    pub cursor: usize,
    /// The task being rewritten, while editing.
    pub editing: Option<String>,
    /// Where it was opened, for what is added from here.
    pub origin: Option<Origin>,
    /// Why the last key did nothing.
    pub warning: Option<&'static str>,
    /// The last task deleted, for `u`.
    pub undo: Option<Task>,
}

/// What a key in the panel asks for.
pub enum Action {
    Stay,
    Close,
    Add(Draft),
    Save(String, Draft),
    Toggle(String),
    Snooze(String),
    Delete(String),
    Undo(Task),
    /// Open the conversation the task was made in.
    Go(String),
    ClearDone,
}

impl TaskPanel {
    pub fn new(origin: Option<Origin>, text: &str) -> Self {
        let mut input = Editor::default();
        input.set(&one_line(text));
        Self {
            input,
            focus: Focus::Input,
            cursor: 0,
            editing: None,
            origin,
            warning: None,
            undo: None,
        }
    }

    /// What the line reads as right now.
    pub fn draft(&self) -> Draft {
        Draft::read(self.input.text(), Local::now())
    }

    pub fn type_str(&mut self, text: &str) {
        self.focus = Focus::Input;
        let room = TEXT_MAX.saturating_sub(self.input.text().chars().count());
        // Pasted lines run on as one.
        let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(room).collect();
        self.input.insert_str(&text);
    }

    /// The task under the cursor.
    pub fn current<'a>(&self, tasks: &'a Tasks) -> Option<&'a Task> {
        let list = tasks.ordered();
        list.get(self.cursor.min(list.len().saturating_sub(1))).copied()
    }

    pub fn key(&mut self, key: KeyEvent, tasks: &Tasks) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        self.warning = None;
        let count = tasks.items.len();
        self.cursor = self.cursor.min(count.saturating_sub(1));
        if self.focus == Focus::List && count == 0 {
            self.focus = Focus::Input;
        }
        match (self.focus, key.code) {
            (_, KeyCode::Char('t')) if ctrl => return Action::Close,
            (Focus::Input, KeyCode::Esc) if self.editing.is_some() => {
                self.editing = None;
                self.input.take();
                self.focus = Focus::List;
            }
            (_, KeyCode::Esc) => return Action::Close,
            (Focus::Input, KeyCode::Enter) => return self.submit(),
            (Focus::Input, KeyCode::Down | KeyCode::Tab) if count > 0 => self.focus = Focus::List,
            (Focus::Input, KeyCode::Backspace) if alt => self.input.delete_word(),
            (Focus::Input, KeyCode::Backspace) => self.input.backspace(),
            (Focus::Input, KeyCode::Delete) => self.input.delete(),
            (Focus::Input, KeyCode::Left) if ctrl || alt => self.input.word_left(),
            (Focus::Input, KeyCode::Right) if ctrl || alt => self.input.word_right(),
            (Focus::Input, KeyCode::Char('b')) if alt => self.input.word_left(),
            (Focus::Input, KeyCode::Char('f')) if alt => self.input.word_right(),
            (Focus::Input, KeyCode::Left) => self.input.left(),
            (Focus::Input, KeyCode::Right) => self.input.right(),
            (Focus::Input, KeyCode::Home) => self.input.home(),
            (Focus::Input, KeyCode::End) => self.input.end(),
            (Focus::Input, KeyCode::Char('a')) if ctrl => self.input.home(),
            (Focus::Input, KeyCode::Char('e')) if ctrl => self.input.end(),
            (Focus::Input, KeyCode::Char('w')) if ctrl => self.input.delete_word(),
            (Focus::Input, KeyCode::Char('u')) if ctrl => self.input.kill_line(),
            (Focus::Input, KeyCode::Char(c)) if !ctrl && !alt => self.type_str(&c.to_string()),

            (Focus::List, KeyCode::Up | KeyCode::Char('k')) if self.cursor == 0 => self.focus = Focus::Input,
            (Focus::List, KeyCode::BackTab | KeyCode::Tab) => self.focus = Focus::Input,
            (Focus::List, KeyCode::Up | KeyCode::Char('k')) => self.cursor -= 1,
            (Focus::List, KeyCode::Down | KeyCode::Char('j')) => {
                self.cursor = (self.cursor + 1).min(count.saturating_sub(1))
            }
            (Focus::List, KeyCode::Home | KeyCode::Char('g')) => self.cursor = 0,
            (Focus::List, KeyCode::End | KeyCode::Char('G')) => self.cursor = count.saturating_sub(1),
            (Focus::List, KeyCode::Char(' ' | 'x')) => {
                if let Some(task) = self.current(tasks) {
                    return Action::Toggle(task.id.clone());
                }
            }
            (Focus::List, KeyCode::Char('s')) => {
                if let Some(task) = self.current(tasks) {
                    return Action::Snooze(task.id.clone());
                }
            }
            (Focus::List, KeyCode::Enter | KeyCode::Char('e')) => {
                if let Some(task) = self.current(tasks) {
                    self.input.set(&crate::tasks::as_text(task, Local::now()));
                    self.editing = Some(task.id.clone());
                    self.focus = Focus::Input;
                }
            }
            (Focus::List, KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace) => {
                if let Some(task) = self.current(tasks) {
                    return Action::Delete(task.id.clone());
                }
            }
            (Focus::List, KeyCode::Char('u')) => match self.undo.take() {
                Some(task) => return Action::Undo(task),
                None => self.warning = Some("nothing to undo"),
            },
            (Focus::List, KeyCode::Char('o')) => match self.current(tasks).and_then(|t| t.origin.as_ref()) {
                Some(origin) => return Action::Go(origin.address.clone()),
                None => self.warning = Some("that one wasn't made in a chat"),
            },
            (Focus::List, KeyCode::Char('c')) => {
                if tasks.items.iter().any(Task::done) {
                    return Action::ClearDone;
                }
                self.warning = Some("nothing is checked off yet");
            }
            (Focus::List, KeyCode::Char('a' | 'n' | 'i' | '/')) => self.focus = Focus::Input,
            _ => {}
        }
        Action::Stay
    }

    fn submit(&mut self) -> Action {
        if self.input.text().trim().is_empty() {
            self.warning = Some("type something to do first");
            return Action::Stay;
        }
        let draft = self.draft();
        if draft.due.is_some_and(|d| d <= Local::now()) {
            self.warning = Some("that time has already passed");
            return Action::Stay;
        }
        self.input.take();
        match self.editing.take() {
            Some(id) => {
                self.focus = Focus::List;
                Action::Save(id, draft)
            }
            None => Action::Add(draft),
        }
    }

    /// Keeps the cursor on `id` after the list moves around it.
    pub fn follow(&mut self, tasks: &Tasks, id: &str) {
        if let Some(at) = tasks.ordered().iter().position(|t| t.id == id) {
            self.cursor = at;
        }
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(panel: &mut TaskPanel, tasks: &Tasks, code: KeyCode) -> Action {
        panel.key(KeyEvent::from(code), tasks)
    }

    fn type_in(panel: &mut TaskPanel, tasks: &Tasks, text: &str) {
        for c in text.chars() {
            press(panel, tasks, KeyCode::Char(c));
        }
    }

    #[test]
    fn typing_then_enter_adds() {
        let tasks = Tasks::default();
        let mut panel = TaskPanel::new(None, "");
        assert!(matches!(press(&mut panel, &tasks, KeyCode::Enter), Action::Stay));
        assert!(panel.warning.is_some(), "an empty line adds nothing");
        type_in(&mut panel, &tasks, "stretch in 20m");
        assert!(panel.draft().due.is_some());
        match press(&mut panel, &tasks, KeyCode::Enter) {
            Action::Add(d) => assert_eq!(d.text, "stretch"),
            _ => panic!("Enter should add"),
        }
        assert!(panel.input.is_empty(), "ready for the next one");
        assert_eq!(panel.focus, Focus::Input);
    }

    #[test]
    fn letters_in_the_list_are_keys() {
        let mut tasks = Tasks::default();
        tasks.add(Draft::read("buy milk", Local::now()), None, 0);
        let id = tasks.items[0].id.clone();
        let mut panel = TaskPanel::new(None, "");
        press(&mut panel, &tasks, KeyCode::Down);
        assert_eq!(panel.focus, Focus::List);
        assert!(matches!(press(&mut panel, &tasks, KeyCode::Char(' ')), Action::Toggle(t) if t == id));
        assert!(matches!(press(&mut panel, &tasks, KeyCode::Char('s')), Action::Snooze(t) if t == id));
        assert!(matches!(press(&mut panel, &tasks, KeyCode::Char('d')), Action::Delete(t) if t == id));
        assert!(matches!(press(&mut panel, &tasks, KeyCode::Char('o')), Action::Stay));
        assert!(panel.warning.is_some(), "nowhere to go back to");
        press(&mut panel, &tasks, KeyCode::Up);
        assert_eq!(panel.focus, Focus::Input, "↑ from the top goes back to typing");
        assert!(matches!(press(&mut panel, &tasks, KeyCode::Esc), Action::Close));
    }

    #[test]
    fn enter_on_a_task_edits_it() {
        let mut tasks = Tasks::default();
        tasks.add(Draft::read("call mom tomorrow at 5pm", Local::now()), None, 0);
        let id = tasks.items[0].id.clone();
        let mut panel = TaskPanel::new(None, "");
        press(&mut panel, &tasks, KeyCode::Down);
        press(&mut panel, &tasks, KeyCode::Enter);
        assert_eq!(panel.editing.as_deref(), Some(id.as_str()));
        assert_eq!(panel.input.text(), "call mom tomorrow at 5pm");
        press(&mut panel, &tasks, KeyCode::Char('!'));
        match press(&mut panel, &tasks, KeyCode::Enter) {
            Action::Save(t, d) => {
                assert_eq!(t, id);
                assert_eq!(d.text, "call mom");
            }
            _ => panic!("Enter should save the edit"),
        }
        // Esc while editing lets go of the edit, not the panel.
        press(&mut panel, &tasks, KeyCode::Enter);
        assert!(matches!(press(&mut panel, &tasks, KeyCode::Esc), Action::Stay));
        assert!(panel.editing.is_none() && panel.input.is_empty());
    }

    #[test]
    fn a_time_gone_by_is_refused() {
        let tasks = Tasks::default();
        let mut panel = TaskPanel::new(None, "");
        type_in(&mut panel, &tasks, "yesterday's thing today at midnight");
        assert!(matches!(press(&mut panel, &tasks, KeyCode::Enter), Action::Stay));
        assert_eq!(panel.warning, Some("that time has already passed"));
        assert!(!panel.input.is_empty(), "what was typed is kept to fix");
    }
}
