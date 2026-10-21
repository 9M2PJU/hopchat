//! The group panel: name a group and pick who is in it, without opening
//! anyone's chat first. Opened on a group, it adds people to that group.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const NAME_CHARS: usize = 40;

/// Someone who could be in the group, in the order the panel lists them.
pub struct Candidate {
    pub address: String,
    pub nick: String,
    pub online: bool,
    pub secure: bool,
    /// Already in the group being added to.
    pub member: bool,
}

impl Candidate {
    /// An invite needs their key package, so only a live peer can be added.
    pub fn pickable(&self) -> bool {
        self.online && !self.member
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Focus {
    Name,
    People,
}

pub struct GroupPanel {
    /// The group (id, name) people are being added to; `None` while
    /// starting a new one.
    pub target: Option<(String, String)>,
    pub name: String,
    pub focus: Focus,
    pub cursor: usize,
    /// Addresses ticked so far.
    pub picked: Vec<String>,
    /// Why the last Enter did nothing.
    pub warning: Option<&'static str>,
}

/// What a key in the panel asks for.
pub enum Action {
    Stay,
    Close,
    Create { name: String, members: Vec<String> },
    Invite { group: String, members: Vec<String> },
}

impl GroupPanel {
    pub fn new_group(name: &str) -> Self {
        Self {
            target: None,
            name: name.chars().take(NAME_CHARS).collect(),
            focus: Focus::Name,
            cursor: 0,
            picked: Vec::new(),
            warning: None,
        }
    }

    pub fn add_to(group: String, name: String) -> Self {
        Self {
            target: Some((group, name)),
            name: String::new(),
            focus: Focus::People,
            cursor: 0,
            picked: Vec::new(),
            warning: None,
        }
    }

    pub fn is_picked(&self, address: &str) -> bool {
        self.picked.iter().any(|p| p == address)
    }

    pub fn type_str(&mut self, text: &str) {
        if self.focus == Focus::Name {
            let room = NAME_CHARS.saturating_sub(self.name.chars().count());
            self.name.extend(text.chars().filter(|c| !c.is_control()).take(room));
        }
    }

    pub fn key(&mut self, key: KeyEvent, people: &[Candidate]) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.warning = None;
        let last = people.len().saturating_sub(1);
        self.cursor = self.cursor.min(last);
        match (self.focus, key.code) {
            (_, KeyCode::Esc) => return Action::Close,
            (_, KeyCode::Enter) => return self.finish(people),
            (Focus::Name, KeyCode::Down | KeyCode::Tab) if !people.is_empty() => {
                self.focus = Focus::People
            }
            (Focus::Name, KeyCode::Backspace) => {
                self.name.pop();
            }
            (Focus::Name, KeyCode::Char('u')) if ctrl => self.name.clear(),
            (Focus::Name, KeyCode::Char(c)) if !ctrl => self.type_str(&c.to_string()),
            (Focus::People, KeyCode::Up | KeyCode::Char('k')) if self.cursor == 0 => {
                if self.target.is_none() {
                    self.focus = Focus::Name;
                }
            }
            (Focus::People, KeyCode::BackTab | KeyCode::Tab) if self.target.is_none() => {
                self.focus = Focus::Name
            }
            (Focus::People, KeyCode::Up | KeyCode::Char('k')) => self.cursor -= 1,
            (Focus::People, KeyCode::Down | KeyCode::Char('j')) => {
                self.cursor = (self.cursor + 1).min(last)
            }
            (Focus::People, KeyCode::Home) => self.cursor = 0,
            (Focus::People, KeyCode::End) => self.cursor = last,
            (Focus::People, KeyCode::Char(' ' | 'x')) => {
                if let Some(person) = people.get(self.cursor) {
                    if !person.pickable() {
                        self.warning = Some(if person.member {
                            "they're already in this group"
                        } else {
                            "they're offline; they can be added once they're back"
                        });
                    } else if let Some(i) = self.picked.iter().position(|p| *p == person.address) {
                        self.picked.remove(i);
                    } else {
                        self.picked.push(person.address.clone());
                    }
                }
            }
            _ => {}
        }
        Action::Stay
    }

    fn finish(&mut self, people: &[Candidate]) -> Action {
        // Only people who can still be added: someone may have gone
        // offline since they were ticked.
        let members: Vec<String> = people
            .iter()
            .filter(|p| p.pickable() && self.is_picked(&p.address))
            .map(|p| p.address.clone())
            .collect();
        match &self.target {
            None if self.name.trim().is_empty() => {
                self.focus = Focus::Name;
                self.warning = Some("give the group a name first");
                Action::Stay
            }
            None => Action::Create {
                name: self.name.trim().to_string(),
                members,
            },
            Some(_) if members.is_empty() => {
                self.warning = Some("pick someone with Space first");
                Action::Stay
            }
            Some((group, _)) => Action::Invite {
                group: group.clone(),
                members,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(nick: &str, online: bool, member: bool) -> Candidate {
        Candidate {
            address: format!("off1{nick}"),
            nick: nick.into(),
            online,
            secure: online,
            member,
        }
    }

    fn press(panel: &mut GroupPanel, people: &[Candidate], code: KeyCode) -> Action {
        panel.key(KeyEvent::from(code), people)
    }

    #[test]
    fn name_then_pick_then_create() {
        let people = [person("bob", true, false), person("carol", true, false), person("dave", false, false)];
        let mut panel = GroupPanel::new_group("");
        for c in "house trip".chars() {
            press(&mut panel, &people, KeyCode::Char(c));
        }
        assert_eq!(panel.name, "house trip", "space types into the name");
        press(&mut panel, &people, KeyCode::Down);
        assert_eq!(panel.focus, Focus::People);
        press(&mut panel, &people, KeyCode::Char(' '));
        press(&mut panel, &people, KeyCode::Down);
        press(&mut panel, &people, KeyCode::Down);
        press(&mut panel, &people, KeyCode::Char(' '));
        assert!(panel.warning.is_some(), "an offline peer can't be picked");
        match press(&mut panel, &people, KeyCode::Enter) {
            Action::Create { name, members } => {
                assert_eq!(name, "house trip");
                assert_eq!(members, ["off1bob"]);
            }
            _ => panic!("Enter should create the group"),
        }
    }

    #[test]
    fn a_group_needs_a_name() {
        let people = [person("bob", true, false)];
        let mut panel = GroupPanel::new_group("  ");
        panel.focus = Focus::People;
        assert!(matches!(press(&mut panel, &people, KeyCode::Enter), Action::Stay));
        assert_eq!(panel.focus, Focus::Name);
        assert!(panel.warning.is_some());
    }

    #[test]
    fn space_untags_and_up_returns_to_the_name() {
        let people = [person("bob", true, false)];
        let mut panel = GroupPanel::new_group("x");
        press(&mut panel, &people, KeyCode::Tab);
        press(&mut panel, &people, KeyCode::Char(' '));
        press(&mut panel, &people, KeyCode::Char(' '));
        assert!(panel.picked.is_empty());
        press(&mut panel, &people, KeyCode::Up);
        assert_eq!(panel.focus, Focus::Name);
    }

    #[test]
    fn adding_to_a_group_skips_members() {
        let people = [person("bob", true, true), person("carol", true, false)];
        let mut panel = GroupPanel::add_to("g1".into(), "house".into());
        assert!(matches!(press(&mut panel, &people, KeyCode::Enter), Action::Stay));
        press(&mut panel, &people, KeyCode::Char(' '));
        assert!(panel.picked.is_empty(), "bob is already in");
        press(&mut panel, &people, KeyCode::Up);
        assert_eq!(panel.focus, Focus::People, "no name to go back to");
        press(&mut panel, &people, KeyCode::Down);
        press(&mut panel, &people, KeyCode::Char(' '));
        match press(&mut panel, &people, KeyCode::Enter) {
            Action::Invite { group, members } => {
                assert_eq!(group, "g1");
                assert_eq!(members, ["off1carol"]);
            }
            _ => panic!("Enter should invite"),
        }
    }
}
