//! The terminal UI: peers on the left, the conversation in the middle, the
//! protocol's own event stream on the right (toggle with Ctrl-E).

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use offline_protocol::Event;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::engine::{Command, Input, Update};

const LOG_LINES: usize = 300;
const ACCENT: Color = Color::Rgb(255, 170, 60);

#[derive(Clone)]
enum Status {
    Queued,
    Waiting(String),
    Retrying(u32),
    Sent,
    Delivered { hops: u8, ms: u64 },
    Failed(String),
}

struct ChatLine {
    mine: bool,
    text: String,
    at: String,
    id: Option<String>,
    hops: u8,
}

struct Peer {
    address: String,
    nick: String,
    online: bool,
    secure: bool,
    unread: usize,
}

pub struct App {
    me_nick: String,
    me_address: String,
    peers: Vec<Peer>,
    selected: usize,
    chats: HashMap<String, Vec<ChatLine>>,
    status: HashMap<String, Status>,
    log: VecDeque<Line<'static>>,
    show_log: bool,
    input: String,
    started: Instant,
    quit: bool,
}

impl App {
    pub fn new(me_nick: String, me_address: String) -> Self {
        Self {
            me_nick,
            me_address,
            peers: Vec::new(),
            selected: 0,
            chats: HashMap::new(),
            status: HashMap::new(),
            log: VecDeque::new(),
            show_log: true,
            input: String::new(),
            started: Instant::now(),
            quit: false,
        }
    }

    pub fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        updates: Receiver<Update>,
        commands: Sender<Input>,
    ) -> std::io::Result<()> {
        while !self.quit {
            while let Ok(update) = updates.try_recv() {
                self.apply(update);
            }
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(50))?
                && let TermEvent::Key(key) = event::read()?
                    && key.kind == KeyEventKind::Press {
                        self.on_key(key.code, key.modifiers, &commands);
                    }
        }
        Ok(())
    }

    fn peer_mut(&mut self, address: &str) -> &mut Peer {
        if let Some(i) = self.peers.iter().position(|p| p.address == address) {
            return &mut self.peers[i];
        }
        self.peers.push(Peer {
            address: address.to_string(),
            nick: short(address),
            online: false,
            secure: false,
            unread: 0,
        });
        self.peers.last_mut().unwrap()
    }

    fn current(&self) -> Option<&Peer> {
        self.peers.get(self.selected)
    }

    fn nick_of(&self, address: &str) -> String {
        self.peers
            .iter()
            .find(|p| p.address == address)
            .map(|p| p.nick.clone())
            .unwrap_or_else(|| short(address))
    }

    fn clock(&self) -> String {
        let s = self.started.elapsed().as_secs();
        format!("{:02}:{:02}", s / 60 % 60, s % 60)
    }

    fn apply(&mut self, update: Update) {
        match update {
            Update::PeerSeen { address, nick } => self.peer_mut(&address).nick = nick,
            Update::PeerUp { address } => {
                self.peer_mut(&address).online = true;
                let who = self.nick_of(&address);
                self.note(Color::Green, format!("link up    {who}"));
            }
            Update::PeerDown { address } => {
                let peer = self.peer_mut(&address);
                peer.online = false;
                peer.secure = false;
                let who = self.nick_of(&address);
                self.note(Color::Red, format!("link down  {who}"));
            }
            Update::SessionReady { address } => {
                self.peer_mut(&address).secure = true;
                let who = self.nick_of(&address);
                self.note(Color::Green, format!("mls ready  {who}"));
            }
            Update::Queued { to, id, text } => {
                self.status.entry(id.clone()).or_insert(Status::Queued);
                let at = self.clock();
                self.chats.entry(to).or_default().push(ChatLine {
                    mine: true,
                    text,
                    at,
                    id: Some(id),
                    hops: 0,
                });
            }
            Update::Incoming { from, text, hops } => {
                let at = self.clock();
                let focused = self.current().is_some_and(|p| p.address == from);
                let peer = self.peer_mut(&from);
                if !focused {
                    peer.unread += 1;
                }
                self.chats.entry(from).or_default().push(ChatLine {
                    mine: false,
                    text,
                    at,
                    id: None,
                    hops,
                });
            }
            Update::Protocol(event) => self.on_protocol(*event),
            Update::Error(e) => self.note(Color::Red, format!("error      {e}")),
        }
    }

    fn on_protocol(&mut self, event: Event) {
        match &event {
            Event::MessageSent { message_id, .. } => {
                self.advance(message_id, Status::Sent);
            }
            Event::MessageDelivered {
                message_id,
                hop_count,
                latency_ms,
                ..
            } => {
                self.status.insert(
                    message_id.clone(),
                    Status::Delivered {
                        hops: *hop_count,
                        ms: *latency_ms,
                    },
                );
            }
            Event::MessageDeferred {
                message_id, reason, ..
            } => self.advance(message_id, Status::Waiting(reason.clone())),
            Event::MessageRetrying {
                message_id,
                retry_count,
                ..
            } => self.advance(message_id, Status::Retrying(*retry_count)),
            Event::MessageFailed {
                message_id, reason, ..
            }
            | Event::MessageUndeliverable {
                message_id, reason, ..
            } => {
                self.status
                    .insert(message_id.clone(), Status::Failed(reason.clone()));
            }
            _ => {}
        }
        if let Some(line) = describe(&event, |a| self.nick_of(a)) {
            self.push_log(line);
        }
    }

    /// Moves a message forward, never back: a late "sent" must not undo
    /// "delivered".
    fn advance(&mut self, id: &str, next: Status) {
        let done = matches!(
            self.status.get(id),
            Some(Status::Delivered { .. } | Status::Failed(_))
        );
        if !done {
            self.status.insert(id.to_string(), next);
        }
    }

    fn note(&mut self, color: Color, text: String) {
        let at = self.clock();
        self.push_log(Line::from(vec![
            Span::raw(format!("{at} ")).dark_gray(),
            Span::styled(text, Style::new().fg(color)),
        ]));
    }

    fn push_log(&mut self, line: Line<'static>) {
        self.log.push_back(line);
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers, commands: &Sender<Input>) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('e') if ctrl => self.show_log = !self.show_log,
            KeyCode::Esc => self.quit = true,
            KeyCode::Tab | KeyCode::Down if !self.peers.is_empty() => {
                self.selected = (self.selected + 1) % self.peers.len();
            }
            KeyCode::BackTab | KeyCode::Up if !self.peers.is_empty() => {
                self.selected = (self.selected + self.peers.len() - 1) % self.peers.len();
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if let (false, Some(peer)) = (text.is_empty(), self.current()) {
                    let to = peer.address.clone();
                    let _ = commands.send(Input::Ui(Command::Send { to, text }));
                    self.input.clear();
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) if !ctrl => self.input.push(c),
            _ => {}
        }
        if let Some(i) = Some(self.selected).filter(|&i| i < self.peers.len()) {
            self.peers[i].unread = 0;
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).areas(frame.area());
        let log_width = if self.show_log { 46 } else { 0 };
        let [side, main, log] = Layout::horizontal([
            Constraint::Length(28),
            Constraint::Min(30),
            Constraint::Length(log_width),
        ])
        .areas(body);

        self.draw_peers(frame, side);
        self.draw_chat(frame, main);
        if self.show_log {
            self.draw_log(frame, log);
        }
        frame.render_widget(
            Line::from(vec![
                " Enter".fg(ACCENT).bold(),
                " send  ".dark_gray(),
                "Tab/↑↓".fg(ACCENT).bold(),
                " switch peer  ".dark_gray(),
                "Ctrl-E".fg(ACCENT).bold(),
                " protocol log  ".dark_gray(),
                "Esc".fg(ACCENT).bold(),
                " quit".dark_gray(),
            ]),
            footer,
        );
    }

    fn draw_peers(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" hopchat ").title_bottom(
            Line::from(format!(" {} · {} ", self.me_nick, short(&self.me_address))).dark_gray(),
        );
        let items: Vec<ListItem> = if self.peers.is_empty() {
            vec![
                ListItem::new("  looking for peers…".dark_gray()),
                ListItem::new(""),
                ListItem::new("  run hopchat on another".dark_gray()),
                ListItem::new("  machine on this network".dark_gray()),
            ]
        } else {
            self.peers
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let dot = if p.online { "● ".green() } else { "○ ".dark_gray() };
                    let lock = if p.secure { " 🔒" } else { "" };
                    let unread = if p.unread > 0 {
                        format!(" ({})", p.unread)
                    } else {
                        String::new()
                    };
                    let mut name = Span::raw(p.nick.clone());
                    if i == self.selected {
                        name = name.fg(ACCENT).bold();
                    }
                    let marker = if i == self.selected { "▌" } else { " " };
                    ListItem::new(Line::from(vec![
                        marker.fg(ACCENT),
                        dot,
                        name,
                        Span::raw(lock),
                        unread.fg(ACCENT).bold(),
                    ]))
                })
                .collect()
        };
        frame.render_widget(List::new(items).block(block), area);
    }

    fn draw_chat(&self, frame: &mut Frame, area: Rect) {
        let [history, input] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(area);

        let (title, lines) = match self.current() {
            None => (" no conversation ".to_string(), Vec::new()),
            Some(peer) => {
                let state = match (peer.online, peer.secure) {
                    (true, true) => "end-to-end encrypted (MLS)",
                    (true, false) => "handshaking…",
                    (false, _) => "offline · messages wait for them",
                };
                let title = format!(" @{} · {} · {} ", peer.nick, short(&peer.address), state);
                let lines = self
                    .chats
                    .get(&peer.address)
                    .map(|chat| chat.iter().map(|l| self.chat_line(l, &peer.nick)).collect())
                    .unwrap_or_default();
                (title, lines)
            }
        };

        // Show the newest lines: scroll so the last one sits at the bottom.
        let inner_height = history.height.saturating_sub(2) as usize;
        let inner_width = history.width.saturating_sub(2).max(1) as usize;
        let total: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner_width)).sum();
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let scroll = total.saturating_sub(inner_height) as u16;
        frame.render_widget(
            paragraph.block(panel(&title)).scroll((scroll, 0)),
            history,
        );

        let prompt = Paragraph::new(Line::from(vec![
            "› ".fg(ACCENT).bold(),
            Span::raw(self.input.clone()),
        ]))
        .block(panel(""));
        frame.render_widget(prompt, input);
        frame.set_cursor_position((input.x + 3 + self.input.chars().count() as u16, input.y + 1));
    }

    fn chat_line(&self, line: &ChatLine, their_nick: &str) -> Line<'static> {
        let mut spans = vec![Span::raw(format!("{} ", line.at)).dark_gray()];
        if line.mine {
            spans.push(format!("{}: ", self.me_nick).fg(ACCENT).bold());
        } else {
            spans.push(format!("{their_nick}: ").cyan().bold());
        }
        spans.push(Span::raw(line.text.clone()));
        if line.mine {
            let status = line.id.as_ref().and_then(|id| self.status.get(id));
            spans.push(Span::raw("  "));
            spans.push(tick(status));
        } else if line.hops > 1 {
            spans.push(format!("  via {} hops", line.hops).dark_gray());
        }
        Line::from(spans)
    }

    fn draw_log(&self, frame: &mut Frame, area: Rect) {
        let height = area.height.saturating_sub(2) as usize;
        let skip = self.log.len().saturating_sub(height);
        let lines: Vec<Line> = self.log.iter().skip(skip).cloned().collect();
        frame.render_widget(Paragraph::new(lines).block(panel(" protocol ")), area);
    }
}

fn tick(status: Option<&Status>) -> Span<'static> {
    match status {
        None | Some(Status::Queued) => "◷".dark_gray(),
        Some(Status::Waiting(reason)) => format!("◷ {reason}").dark_gray(),
        Some(Status::Retrying(n)) => format!("↻ retry {n}").yellow(),
        Some(Status::Sent) => "✓".dark_gray(),
        Some(Status::Delivered { hops, ms }) => {
            let detail = if *hops > 1 {
                format!("✓✓ {hops} hops · {ms}ms")
            } else {
                format!("✓✓ {ms}ms")
            };
            Span::styled(detail, Style::new().fg(Color::Green))
        }
        Some(Status::Failed(reason)) => {
            Span::styled(format!("✗ {reason}"), Style::new().fg(Color::Red))
        }
    }
}

fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().dark_gray())
        .title(Span::styled(
            title.to_string(),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

/// `off1qz8…k3d` style: enough to tell addresses apart at a glance.
fn short(address: &str) -> String {
    let n = address.chars().count();
    if n <= 12 {
        return address.to_string();
    }
    let head: String = address.chars().take(7).collect();
    let tail: String = address.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// One protocol-log line per event worth watching; the rest are noise here.
fn describe(event: &Event, nick: impl Fn(&str) -> String) -> Option<Line<'static>> {
    let (color, text) = match event {
        Event::MessageSent { message_id, .. } => (Color::Gray, format!("sent       {}", id8(message_id))),
        Event::MessageReceived {
            sender,
            hop_count,
            transport,
            encrypted,
            ..
        } => (
            Color::Cyan,
            format!(
                "recv       {} {}hop {transport}{}",
                nick(sender),
                hop_count,
                if *encrypted { " 🔒" } else { "" }
            ),
        ),
        Event::MessageDelivered {
            message_id,
            latency_ms,
            ..
        } => (Color::Green, format!("ack        {} {latency_ms}ms", id8(message_id))),
        Event::MessageDeferred { message_id, reason, .. } => {
            (Color::DarkGray, format!("deferred   {} {reason}", id8(message_id)))
        }
        Event::MessageRetrying {
            message_id,
            retry_count,
            ..
        } => (Color::Yellow, format!("retry #{retry_count}   {}", id8(message_id))),
        Event::MessageFailed { message_id, reason, .. }
        | Event::MessageUndeliverable { message_id, reason, .. } => {
            (Color::Red, format!("failed     {} {reason}", id8(message_id)))
        }
        Event::MessageRelayed {
            sender,
            recipient,
            hop_count,
            ..
        } => (
            Color::Magenta,
            format!("relay      {}→{} hop {hop_count}", nick(sender), nick(recipient)),
        ),
        Event::SecureSessionFailed { peer_id, reason } => {
            (Color::Red, format!("mls failed {} {reason}", nick(peer_id)))
        }
        Event::SecurityWarning { peer_id, reason, .. } => {
            (Color::Yellow, format!("warning    {} {reason}", nick(peer_id)))
        }
        _ => return None,
    };
    Some(Line::from(Span::styled(text, Style::new().fg(color))))
}

fn id8(id: &str) -> String {
    id.chars().take(8).collect()
}
