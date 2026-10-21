//! The Devices panel: what home found on the network, what has been added,
//! and pairing, testing, renaming and forgetting them.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::home::{self, DeviceView, Request, State, View};

#[derive(Default)]
pub struct Devices {
    pub selected: usize,
    /// The new name being typed, while renaming.
    pub renaming: Option<String>,
    /// `x` was pressed once; a second press forgets.
    pub confirm_forget: bool,
}

/// What a key in the panel asks for.
pub enum Action {
    Stay,
    Close,
    Ask(Request),
    /// Switch home on or off for the open group.
    ToggleGroup,
}

impl Devices {
    pub fn current<'a>(&self, view: &'a View) -> Option<&'a DeviceView> {
        view.devices.get(self.selected.min(view.devices.len().saturating_sub(1)))
    }

    pub fn key(&mut self, key: KeyEvent, view: &View) -> Action {
        if let Some(name) = &mut self.renaming {
            match key.code {
                KeyCode::Enter => {
                    let name = std::mem::take(name);
                    self.renaming = None;
                    if let Some(d) = self.current(view) {
                        return Action::Ask(Request::Rename(d.id.clone(), name));
                    }
                }
                KeyCode::Esc => self.renaming = None,
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => name.clear(),
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL) && name.chars().count() < 32 =>
                {
                    name.push(c);
                }
                _ => {}
            }
            return Action::Stay;
        }

        let forgetting = std::mem::take(&mut self.confirm_forget);
        let last = view.devices.len().saturating_sub(1);
        self.selected = self.selected.min(last);
        let current = self.current(view);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Action::Close,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(last),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = last,
            KeyCode::Char('r') => return Action::Ask(Request::Scan),
            KeyCode::Char('g') => return Action::ToggleGroup,
            KeyCode::Enter | KeyCode::Char('a') | KeyCode::Char('p') => {
                if let Some(d) = current {
                    return Action::Ask(match primary(d) {
                        Primary::Add | Primary::Pair | Primary::Retry => Request::Add(d.id.clone()),
                        Primary::Test => Request::Test(d.id.clone()),
                        Primary::Wait => return Action::Stay,
                    });
                }
            }
            KeyCode::Char('t') => {
                if let Some(d) = current.filter(|d| d.added) {
                    return Action::Ask(Request::Test(d.id.clone()));
                }
            }
            KeyCode::Char('n') | KeyCode::F(2) => {
                if let Some(d) = current {
                    self.renaming = Some(d.name.clone());
                }
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(d) = current.filter(|d| d.added) {
                    if forgetting {
                        return Action::Ask(Request::Forget(d.id.clone()));
                    }
                    self.confirm_forget = true;
                }
            }
            _ => {}
        }
        Action::Stay
    }
}

/// What Enter does for a device.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Primary {
    Add,
    Pair,
    Retry,
    Test,
    Wait,
}

pub fn primary(d: &DeviceView) -> Primary {
    match (&d.state, d.kind, d.added) {
        (State::Pairing { .. }, _, _) => Primary::Wait,
        (State::Failed(_), _, false) => Primary::Retry,
        (State::New, home::Kind::Tv, _) => Primary::Pair,
        (_, _, false) => Primary::Add,
        _ => Primary::Test,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn device(added: bool, kind: home::Kind, state: State) -> DeviceView {
        DeviceView {
            id: "id".into(),
            kind,
            name: "Light".into(),
            ip: [10, 0, 0, 1].into(),
            added,
            state,
            detail: String::new(),
        }
    }

    #[test]
    fn enter_does_the_next_sensible_thing() {
        use home::Kind::{Light, Tv};
        assert_eq!(primary(&device(false, Light, State::New)), Primary::Add);
        assert_eq!(primary(&device(false, Tv, State::New)), Primary::Pair);
        // An added TV whose pairing never finished pairs again.
        assert_eq!(primary(&device(true, Tv, State::New)), Primary::Pair);
        assert_eq!(primary(&device(false, Tv, State::Failed("no".into()))), Primary::Retry);
        assert_eq!(primary(&device(true, Tv, State::Pairing { until: Instant::now() })), Primary::Wait);
        assert_eq!(primary(&device(true, Light, State::Ready)), Primary::Test);
    }

    #[test]
    fn forgetting_takes_two_presses() {
        let view = View { devices: vec![device(true, home::Kind::Light, State::Ready)], ..Default::default() };
        let mut panel = Devices::default();
        let x = KeyEvent::from(KeyCode::Char('x'));
        assert!(matches!(panel.key(x, &view), Action::Stay));
        assert!(panel.confirm_forget);
        assert!(matches!(panel.key(x, &view), Action::Ask(Request::Forget(_))));
        // Anything in between cancels.
        panel.key(x, &view);
        panel.key(KeyEvent::from(KeyCode::Down), &view);
        assert!(!panel.confirm_forget);
    }

    #[test]
    fn renaming_edits_then_saves() {
        let view = View { devices: vec![device(true, home::Kind::Light, State::Ready)], ..Default::default() };
        let mut panel = Devices::default();
        panel.key(KeyEvent::from(KeyCode::Char('n')), &view);
        assert_eq!(panel.renaming.as_deref(), Some("Light"));
        for _ in 0..5 {
            panel.key(KeyEvent::from(KeyCode::Backspace), &view);
        }
        for c in "Desk".chars() {
            panel.key(KeyEvent::from(KeyCode::Char(c)), &view);
        }
        match panel.key(KeyEvent::from(KeyCode::Enter), &view) {
            Action::Ask(Request::Rename(_, name)) => assert_eq!(name, "Desk"),
            _ => panic!("Enter should save the name"),
        }
        assert!(panel.renaming.is_none());
    }
}
