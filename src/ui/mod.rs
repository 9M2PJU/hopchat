//! The terminal UI: conversations on the left, the open one in the middle,
//! the protocol's own event stream on the right (toggle with Ctrl-E).

mod devices;
mod editor;
mod picker;
mod render;

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use base64::Engine as _;
use offline_protocol::Event;
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::DefaultTerminal;

use crate::engine::{self, Command, Input, Quote, Update};
use crate::history::{self, Body, Conversation, Entry, Quoted, Status, Transfer};
use crate::home;
use devices::Devices;
use editor::Editor;
use picker::{Outcome, Picker};

const LOG_LINES: usize = 300;
/// How long a peer's "typing…" lasts without a refresh.
const TYPING_SHOWN: Duration = Duration::from_secs(6);
/// How often we re-announce that we are typing, and how long after the last
/// keystroke we say we stopped.
const TYPING_REFRESH: Duration = Duration::from_secs(3);
const TYPING_IDLE: Duration = Duration::from_secs(5);
const SAVE_EVERY: Duration = Duration::from_secs(1);
const FLASH_FOR: Duration = Duration::from_secs(4);

pub struct Convo {
    pub data: Conversation,
    pub online: bool,
    pub secure: bool,
    pub unread: usize,
    pub typing_until: Option<Instant>,
    /// For ordering the list: the newest message, or when we first saw them.
    pub active_ms: i64,
}

impl Convo {
    fn new(address: &str) -> Self {
        Self {
            data: Conversation {
                address: address.to_string(),
                nick: render::short(address),
                ..Default::default()
            },
            online: false,
            secure: false,
            unread: 0,
            typing_until: None,
            active_ms: now_ms(),
        }
    }

    pub fn typing(&self) -> bool {
        self.typing_until.is_some_and(|t| t > Instant::now())
    }

    fn push(&mut self, entry: Entry) {
        self.active_ms = self.active_ms.max(entry.at_ms);
        self.data.entries.push(entry);
        self.data.dirty = true;
    }

    fn find(&mut self, id: &str) -> Option<&mut Entry> {
        self.data.entries.iter_mut().rev().find(|e| e.id == id)
    }
}

/// An inbound transfer whose sender we do not know until it completes.
pub struct Incoming {
    pub name: String,
    pub size: u64,
    pub done: u32,
    pub total: u32,
}

pub enum Mode {
    Normal,
    /// Picking a message in the open conversation, by entry index.
    Select(usize),
    Picker(Picker),
    Devices(Devices),
    Help,
}

pub struct App {
    pub me_nick: String,
    pub me_address: String,
    pub convos: Vec<Convo>,
    pub selected: Option<String>,
    pub editor: Editor,
    pub attachments: Vec<PathBuf>,
    pub reply: Option<String>,
    pub mode: Mode,
    /// Rows scrolled up from the newest message.
    pub scroll: usize,
    pub log: VecDeque<Line<'static>>,
    pub show_log: bool,
    pub incoming: HashMap<String, Incoming>,
    /// When each transfer started moving, for its rate.
    pub started: HashMap<String, Instant>,
    pub flash: Option<(String, bool, Instant)>,
    pub downloads: PathBuf,
    /// Areas from the last draw, for the mouse.
    pub peer_rows: Vec<(Rect, String)>,
    pub chat_area: Rect,
    /// Message id or file id → the conversation it belongs to.
    owners: HashMap<String, String>,
    store: Option<history::Store>,
    last_save: Instant,
    focused: bool,
    typing_sent: Option<(String, Instant)>,
    last_edit: Instant,
    title: String,
    quit: bool,
    home: Sender<home::Request>,
    /// What home last reported, for the Devices panel.
    pub home_view: home::View,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl App {
    pub fn new(
        me_nick: String,
        me_address: String,
        store: Option<history::Store>,
        downloads: PathBuf,
        home: Sender<home::Request>,
    ) -> Self {
        let mut convos = Vec::new();
        let mut owners = HashMap::new();
        if let Some(store) = &store {
            for data in store.load() {
                for e in data.entries.iter().filter(|e| e.mine && !e.status.settled()) {
                    owners.insert(e.id.clone(), data.address.clone());
                }
                let active_ms = data.entries.last().map_or(0, |e| e.at_ms);
                // A group we are still in is announced by the engine at start.
                convos.push(Convo {
                    data,
                    online: false,
                    secure: false,
                    unread: 0,
                    typing_until: None,
                    active_ms,
                });
            }
        }
        let mut app = Self {
            me_nick,
            me_address,
            convos,
            selected: None,
            editor: Editor::default(),
            attachments: Vec::new(),
            reply: None,
            mode: Mode::Normal,
            scroll: 0,
            log: VecDeque::new(),
            show_log: false,
            incoming: HashMap::new(),
            started: HashMap::new(),
            flash: None,
            downloads,
            peer_rows: Vec::new(),
            chat_area: Rect::default(),
            owners,
            store,
            last_save: Instant::now(),
            focused: true,
            typing_sent: None,
            last_edit: Instant::now(),
            title: String::new(),
            quit: false,
            home,
            home_view: home::View::default(),
        };
        app.selected = app.ordered().first().map(|c| c.data.address.clone());
        app
    }

    pub fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        updates: Receiver<Update>,
        commands: Sender<Input>,
    ) -> std::io::Result<()> {
        let send = |c: Command| {
            let _ = commands.send(Input::Ui(c));
        };
        while !self.quit {
            while let Ok(update) = updates.try_recv() {
                self.apply(update, &send);
            }
            self.housekeeping(&send);
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(50))? {
                // Drain everything already waiting, so a paste or a burst of
                // keys costs one redraw, not one each.
                loop {
                    match event::read()? {
                        TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                            self.on_key(key, &send)
                        }
                        TermEvent::Paste(text) => self.on_paste(&text, &send),
                        TermEvent::Mouse(mouse) => self.on_mouse(mouse),
                        TermEvent::FocusGained => self.focused = true,
                        TermEvent::FocusLost => self.focused = false,
                        _ => {}
                    }
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
        self.stop_typing(&send);
        self.save_all();
        Ok(())
    }

    // ----- state -------------------------------------------------------

    /// Conversations in display order: reachable first, then most recent.
    pub fn ordered(&self) -> Vec<&Convo> {
        let mut list: Vec<&Convo> = self.convos.iter().collect();
        list.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then(b.active_ms.cmp(&a.active_ms))
                .then_with(|| a.data.nick.cmp(&b.data.nick))
        });
        list
    }

    fn convo_mut(&mut self, address: &str) -> &mut Convo {
        if let Some(i) = self.convos.iter().position(|c| c.data.address == address) {
            return &mut self.convos[i];
        }
        self.convos.push(Convo::new(address));
        if self.selected.is_none() {
            self.selected = Some(address.to_string());
        }
        self.convos.last_mut().unwrap()
    }

    pub fn current(&self) -> Option<&Convo> {
        let address = self.selected.as_deref()?;
        self.convos.iter().find(|c| c.data.address == address)
    }

    fn current_mut(&mut self) -> Option<&mut Convo> {
        let address = self.selected.clone()?;
        self.convos.iter_mut().find(|c| c.data.address == address)
    }

    pub fn nick_of(&self, address: &str) -> String {
        if address == self.me_address {
            return "you".into();
        }
        self.convos
            .iter()
            .find(|c| c.data.address == address)
            .map(|c| c.data.nick.clone())
            .unwrap_or_else(|| render::short(address))
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), false, Instant::now()));
    }

    fn flash_error(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), true, Instant::now()));
    }

    pub fn flash_text(&self) -> Option<(&str, bool)> {
        self.flash
            .as_ref()
            .filter(|(_, _, at)| at.elapsed() < FLASH_FOR)
            .map(|(t, e, _)| (t.as_str(), *e))
    }

    /// The entry a reply in progress points at.
    pub fn reply_target(&self) -> Option<&Entry> {
        let id = self.reply.as_deref()?;
        self.current()?.data.entries.iter().rev().find(|e| e.id == id)
    }

    fn select(&mut self, address: Option<String>, send: &impl Fn(Command)) {
        if address != self.selected {
            self.stop_typing(send);
            self.selected = address;
            self.scroll = 0;
            self.reply = None;
            if matches!(self.mode, Mode::Select(_)) {
                self.mode = Mode::Normal;
            }
        }
    }

    fn cycle(&mut self, step: isize, send: &impl Fn(Command)) {
        let order: Vec<String> = self.ordered().iter().map(|c| c.data.address.clone()).collect();
        if order.is_empty() {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|s| order.iter().position(|a| a == s))
            .unwrap_or(0) as isize;
        let next = (at + step).rem_euclid(order.len() as isize) as usize;
        self.select(Some(order[next].clone()), send);
    }

    // ----- updates from the engine ------------------------------------

    fn apply(&mut self, update: Update, send: &impl Fn(Command)) {
        match update {
            Update::PeerSeen { address, nick } => {
                let convo = self.convo_mut(&address);
                if convo.data.nick != nick {
                    convo.data.nick = nick;
                    convo.data.dirty = true;
                }
            }
            Update::PeerUp { address } => {
                self.convo_mut(&address).online = true;
                let who = self.nick_of(&address);
                self.note(render::OK, format!("link up    {who}"));
            }
            Update::PeerDown { address } => {
                let convo = self.convo_mut(&address);
                convo.online = false;
                convo.secure = false;
                convo.typing_until = None;
                let who = self.nick_of(&address);
                self.note(render::ERR, format!("link down  {who}"));
            }
            Update::SessionReady { address } => {
                self.convo_mut(&address).secure = true;
                let who = self.nick_of(&address);
                self.note(render::OK, format!("mls ready  {who}"));
            }
            Update::Queued { to, id, text, reply } => {
                self.owners.insert(id.clone(), to.clone());
                let me = self.me_address.clone();
                self.convo_mut(&to).push(Entry {
                    id,
                    mine: true,
                    at_ms: now_ms(),
                    body: Body::Text(text),
                    status: Status::Queued,
                    hops: 0,
                    reply: reply.map(|q| quoted(q, &me)),
                    read: false,
                    sender: None,
                });
            }
            Update::Incoming {
                from,
                id,
                text,
                hops,
                reply,
                ..
            } => {
                let me = self.me_address.clone();
                let entry = Entry {
                    id,
                    mine: false,
                    at_ms: now_ms(),
                    body: Body::Text(text.clone()),
                    status: Status::Sent,
                    hops,
                    reply: reply.map(|q| quoted(q, &me)),
                    read: false,
                    sender: None,
                };
                self.receive(&from, entry, &text);
            }
            Update::FileQueued { file_id, size, .. } => {
                if let Some(entry) = self.file_entry(&file_id)
                    && let Body::File { size: s, .. } = &mut entry.body
                {
                    *s = size;
                }
            }
            Update::FileProgress {
                file_id,
                done,
                total,
                inbound,
            } => {
                self.started.entry(file_id.clone()).or_insert_with(Instant::now);
                if let Some(entry) = self.file_entry(&file_id) {
                    if let Body::File { transfer, .. } = &mut entry.body {
                        *transfer = Transfer::Moving { done, total };
                    }
                } else if let Some((name, size)) = inbound {
                    self.incoming.insert(
                        file_id,
                        Incoming {
                            name,
                            size,
                            done,
                            total,
                        },
                    );
                } else if let Some(incoming) = self.incoming.get_mut(&file_id) {
                    incoming.done = done;
                    incoming.total = total;
                }
            }
            Update::FileSent { file_id } => {
                self.started.remove(&file_id);
                if let Some(entry) = self.file_entry(&file_id) {
                    if let Body::File { transfer, .. } = &mut entry.body {
                        *transfer = Transfer::Done;
                    }
                    entry.status.advance(Status::Delivered { hops: 1, ms: 0 });
                }
            }
            Update::FileFailed { file_id, reason } => {
                self.started.remove(&file_id);
                if let Some(entry) = self.file_entry(&file_id) {
                    if let Body::File { transfer, .. } = &mut entry.body {
                        *transfer = Transfer::Failed(reason.clone());
                    }
                    entry.status = Status::Failed(reason.clone());
                }
                self.flash_error(format!("file not sent: {reason}"));
            }
            Update::FileReceived {
                from,
                file_id,
                name,
                size,
                path,
                ..
            } => {
                self.incoming.remove(&file_id);
                self.started.remove(&file_id);
                let preview = format!("📎 {name}");
                let entry = Entry {
                    id: file_id.clone(),
                    mine: false,
                    at_ms: now_ms(),
                    body: Body::File {
                        file_id,
                        name,
                        size,
                        path,
                        transfer: Transfer::Done,
                    },
                    status: Status::Sent,
                    hops: 1,
                    reply: None,
                    read: false,
                    sender: None,
                };
                self.receive(&from, entry, &preview);
            }
            Update::FileLost {
                from,
                file_id,
                name,
                reason,
            } => {
                self.incoming.remove(&file_id);
                self.started.remove(&file_id);
                self.convo_mut(&from).push(notice(format!("{name} didn't arrive: {reason}")));
            }
            Update::ResendWanted { file_id, to } => {
                // We hold the path of everything we sent: re-supply it.
                let path = self.file_entry(&file_id).and_then(|e| match &e.body {
                    Body::File { path, .. } => Some(path.clone()),
                    _ => None,
                });
                if let Some(path) = path.filter(|p| p.is_file()) {
                    if let Some(entry) = self.file_entry(&file_id)
                        && let Body::File { transfer, .. } = &mut entry.body
                    {
                        *transfer = Transfer::Queued;
                    }
                    send(Command::SendFile { to, path, file_id });
                }
            }
            Update::Typing { from, on } => {
                let convo = self.convo_mut(&from);
                convo.typing_until = on.then(|| Instant::now() + TYPING_SHOWN);
            }
            Update::Read { from, ids } => {
                let convo = self.convo_mut(&from);
                for id in ids {
                    if let Some(entry) = convo.find(&id)
                        && entry.mine
                    {
                        entry.status.advance(Status::Read);
                        convo.data.dirty = true;
                    }
                }
            }
            Update::Group { id, name, members } => {
                let me = self.me_address.clone();
                let convo = self.convo_mut(&id);
                let fresh = !convo.data.group || !convo.online;
                let joined: Vec<String> =
                    members.iter().filter(|m| !convo.data.members.contains(m)).cloned().collect();
                let had_members = !convo.data.members.is_empty();
                convo.data.group = true;
                convo.data.nick = name.clone();
                convo.data.members = members;
                convo.online = true;
                convo.secure = true;
                convo.data.dirty = true;
                if had_members {
                    for member in joined.into_iter().filter(|m| *m != me) {
                        let who = self.nick_of(&member);
                        self.convo_mut(&id).push(notice(format!("{who} joined")));
                    }
                }
                if fresh {
                    self.note(render::OK, format!("group      {name}"));
                }
            }
            Update::GroupLeft { id } => {
                let convo = self.convo_mut(&id);
                convo.online = false;
                convo.secure = false;
                convo.push(notice("you are no longer in this group".into()));
            }
            Update::GroupQueued {
                group,
                id,
                text,
                reply,
            } => {
                self.owners.insert(id.clone(), group.clone());
                let me = self.me_address.clone();
                self.convo_mut(&group).push(Entry {
                    id,
                    mine: true,
                    at_ms: now_ms(),
                    body: Body::Text(text),
                    status: Status::Queued,
                    hops: 0,
                    reply: reply.map(|q| quoted(q, &me)),
                    read: false,
                    sender: None,
                });
            }
            Update::GroupIncoming {
                group,
                from,
                id,
                text,
                reply_to,
            } => {
                let reply = reply_to.and_then(|rid| {
                    let convo = self.convos.iter().find(|c| c.data.address == group)?;
                    let e = convo.data.entries.iter().rev().find(|e| e.id == rid)?;
                    Some(Quoted {
                        id: rid,
                        mine: e.mine,
                        text: render::entry_summary(e),
                    })
                });
                let preview = format!("{}: {text}", self.nick_of(&from));
                let entry = Entry {
                    id,
                    mine: false,
                    at_ms: now_ms(),
                    body: Body::Text(text),
                    status: Status::Sent,
                    hops: 1,
                    reply,
                    read: false,
                    sender: Some(from),
                };
                self.receive(&group, entry, &preview);
            }
            Update::Home(text) => {
                self.note(render::ACCENT, text.clone());
                self.flash(text);
            }
            Update::HomeView(view) => self.home_view = *view,
            Update::Protocol(event) => self.on_protocol(*event),
            Update::Error(e) => {
                self.note(render::ERR, format!("error      {e}"));
                if e.starts_with("send failed") {
                    self.flash_error(e);
                }
            }
        }
    }

    /// Files one of theirs into its conversation, and gets attention for
    /// it if it is not already in view.
    fn receive(&mut self, from: &str, entry: Entry, preview: &str) {
        let visible = self.focused && self.selected.as_deref() == Some(from);
        let convo = self.convo_mut(from);
        convo.typing_until = None;
        convo.push(entry);
        if !visible {
            convo.unread += 1;
            let who = convo.data.nick.clone();
            self.flash(format!("{who}: {}", first_line(preview)));
            // The bell: most terminals turn it into a dock bounce or badge.
            let _ = std::io::stdout().write_all(b"\x07");
        }
    }

    fn file_entry(&mut self, file_id: &str) -> Option<&mut Entry> {
        let owner = self.owners.get(file_id)?.clone();
        self.convos
            .iter_mut()
            .find(|c| c.data.address == owner)?
            .find(file_id)
    }

    fn set_status(&mut self, id: &str, next: Status) {
        let Some(owner) = self.owners.get(id).cloned() else {
            return;
        };
        let convo = self.convo_mut(&owner);
        if let Some(entry) = convo.find(id) {
            entry.status.advance(next);
            convo.data.dirty = true;
        }
    }

    fn on_protocol(&mut self, event: Event) {
        match &event {
            Event::MessageSent { message_id, .. } => self.set_status(message_id, Status::Sent),
            Event::MessageDelivered {
                message_id,
                hop_count,
                latency_ms,
                ..
            } => self.set_status(
                message_id,
                Status::Delivered {
                    hops: *hop_count,
                    ms: *latency_ms,
                },
            ),
            Event::MessageDeferred {
                message_id, reason, ..
            } => self.set_status(message_id, Status::Waiting(reason.clone())),
            Event::MessageRetrying {
                message_id,
                retry_count,
                ..
            } => self.set_status(message_id, Status::Retrying(*retry_count)),
            Event::MessageFailed {
                message_id, reason, ..
            }
            | Event::MessageUndeliverable {
                message_id, reason, ..
            } => self.set_status(message_id, Status::Failed(reason.clone())),
            _ => {}
        }
        // Chunks, receipts and typing pings are messages too; only log the
        // ones a person sent.
        let ours = |id: &str| self.owners.contains_key(id);
        let relevant = match &event {
            Event::MessageSent { message_id, .. }
            | Event::MessageDelivered { message_id, .. }
            | Event::MessageDeferred { message_id, .. }
            | Event::MessageRetrying { message_id, .. } => ours(message_id),
            _ => true,
        };
        if relevant && let Some(line) = render::describe(&event, |a| self.nick_of(a)) {
            self.push_log(line);
        }
    }

    pub fn note(&mut self, color: ratatui::style::Color, text: String) {
        let at = chrono::Local::now().format("%H:%M:%S").to_string();
        self.push_log(render::log_line(&at, color, text));
    }

    fn push_log(&mut self, line: Line<'static>) {
        self.log.push_back(line);
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    /// Per-frame chores: read receipts, typing timeouts, saving, the title.
    fn housekeeping(&mut self, send: &impl Fn(Command)) {
        let picking = matches!(self.mode, Mode::Picker(_));
        if self.focused && !picking
            && let Some(convo) = self.current_mut()
        {
            convo.unread = 0;
            if convo.online && convo.secure && !convo.data.group {
                let mut ids = Vec::new();
                for entry in convo.data.entries.iter_mut().filter(|e| !e.mine && !e.read) {
                    entry.read = true;
                    if matches!(entry.body, Body::Text(_)) {
                        ids.push(entry.id.clone());
                    }
                }
                if !ids.is_empty() {
                    convo.data.dirty = true;
                    let to = convo.data.address.clone();
                    send(Command::Read { to, ids });
                }
            }
        }

        if self.typing_sent.is_some() && self.last_edit.elapsed() > TYPING_IDLE {
            self.stop_typing(send);
        }

        if self.last_save.elapsed() > SAVE_EVERY {
            self.save_all();
        }

        let unread: usize = self.convos.iter().map(|c| c.unread).sum();
        let title = if unread > 0 {
            format!("hopchat ({unread})")
        } else {
            format!("hopchat · {}", self.me_nick)
        };
        if title != self.title {
            let _ = ratatui::crossterm::execute!(
                std::io::stdout(),
                ratatui::crossterm::terminal::SetTitle(&title)
            );
            self.title = title;
        }
    }

    fn save_all(&mut self) {
        self.last_save = Instant::now();
        let Some(store) = &self.store else { return };
        let mut failed = None;
        for convo in self.convos.iter_mut().filter(|c| c.data.dirty) {
            match store.save(&convo.data) {
                Ok(()) => convo.data.dirty = false,
                Err(e) => failed = Some(e),
            }
        }
        if let Some(e) = failed {
            self.flash_error(format!("history not saved: {e}"));
        }
    }

    // ----- typing --------------------------------------------------------

    fn edited(&mut self, send: &impl Fn(Command)) {
        self.last_edit = Instant::now();
        let Some(to) = self.selected.clone() else { return };
        if self.in_group() {
            return;
        }
        if self.editor.is_empty() || self.editor.text().starts_with('/') {
            return self.stop_typing(send);
        }
        let fresh = match &self.typing_sent {
            Some((peer, at)) => *peer != to || at.elapsed() > TYPING_REFRESH,
            None => true,
        };
        if fresh {
            send(Command::Typing {
                to: to.clone(),
                on: true,
            });
            self.typing_sent = Some((to, Instant::now()));
        }
    }

    fn stop_typing(&mut self, send: &impl Fn(Command)) {
        if let Some((to, _)) = self.typing_sent.take() {
            send(Command::Typing { to, on: false });
        }
    }

    // ----- input -----------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent, send: &impl Fn(Command)) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Global keys, whatever is open.
        match key.code {
            KeyCode::Char('c' | 'q') if ctrl => {
                self.quit = true;
                return;
            }
            KeyCode::Char('e') if ctrl => {
                self.show_log = !self.show_log;
                return;
            }
            KeyCode::F(1) => {
                self.mode = match self.mode {
                    Mode::Help => Mode::Normal,
                    _ => Mode::Help,
                };
                return;
            }
            _ => {}
        }

        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Help => {} // any key closes it
            Mode::Picker(mut picker) => {
                if let Some(chosen) = picker_key(&mut picker, key) {
                    self.attach(chosen);
                } else if !matches!(key.code, KeyCode::Esc) {
                    self.mode = Mode::Picker(picker);
                }
            }
            Mode::Devices(mut panel) => match panel.key(key, &self.home_view) {
                devices::Action::Close => {}
                devices::Action::Stay => self.mode = Mode::Devices(panel),
                devices::Action::Ask(request) => {
                    let _ = self.home.send(request);
                    self.mode = Mode::Devices(panel);
                }
                devices::Action::ToggleGroup => {
                    self.toggle_home_here();
                    self.mode = Mode::Devices(panel);
                }
            },
            Mode::Select(at) => self.select_key(at, key, send),
            Mode::Normal if ctrl && key.code == KeyCode::Char('d') => self.open_devices(),
            Mode::Normal => self.normal_key(key, ctrl, alt, send),
        }
    }

    fn normal_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool, send: &impl Fn(Command)) {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = (self.chat_area.height as usize).saturating_sub(4).max(1);
        match key.code {
            KeyCode::Enter if shift || alt => self.editor.insert('\n'),
            KeyCode::Char('j') if ctrl => self.editor.insert('\n'),
            KeyCode::Enter => self.submit(send),
            KeyCode::Tab if self.editor.text().starts_with('/') => {
                if let Some(cmd) = render::command_matches(self.editor.text()).first() {
                    self.editor.set(&format!("{} ", cmd.0));
                }
            }
            KeyCode::Tab => self.cycle(1, send),
            KeyCode::BackTab => self.cycle(-1, send),
            KeyCode::Char('n') if ctrl => self.cycle(1, send),
            KeyCode::Char('p') if ctrl => self.cycle(-1, send),
            KeyCode::Char(c @ '1'..='9') if alt => {
                let n = c as usize - '1' as usize;
                let address = self.ordered().get(n).map(|c| c.data.address.clone());
                if address.is_some() {
                    self.select(address, send);
                }
            }
            KeyCode::Char('o') if ctrl => {
                let start = std::env::current_dir()
                    .ok()
                    .or_else(picker::home_dir)
                    .unwrap_or_else(|| PathBuf::from("/"));
                self.mode = Mode::Picker(Picker::new(start));
            }
            KeyCode::Esc => {
                if self.reply.take().is_none() {
                    self.scroll = 0;
                }
            }
            KeyCode::PageUp => self.scroll += page,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::Up if self.editor.is_empty() || ctrl => self.enter_select(),
            KeyCode::Up => {
                self.editor.up();
            }
            KeyCode::Down => {
                self.editor.down();
            }
            KeyCode::Left if ctrl || alt => self.editor.word_left(),
            KeyCode::Right if ctrl || alt => self.editor.word_right(),
            KeyCode::Char('b') if alt => self.editor.word_left(),
            KeyCode::Char('f') if alt => self.editor.word_right(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::Char('a') if ctrl => self.editor.home(),
            KeyCode::Char('e') if ctrl => self.editor.end(),
            KeyCode::Char('w') if ctrl => {
                self.editor.delete_word();
                self.edited(send);
            }
            KeyCode::Backspace if alt => {
                self.editor.delete_word();
                self.edited(send);
            }
            KeyCode::Char('u') if ctrl => {
                self.editor.kill_line();
                self.edited(send);
            }
            KeyCode::Backspace if self.editor.is_empty() => {
                if self.attachments.pop().is_none() {
                    self.reply = None;
                }
            }
            KeyCode::Backspace => {
                self.editor.backspace();
                self.edited(send);
            }
            KeyCode::Delete => {
                self.editor.delete();
                self.edited(send);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.editor.insert(c);
                self.edited(send);
            }
            _ => {}
        }
    }

    fn enter_select(&mut self) {
        let Some(convo) = self.current() else { return };
        if let Some(last) = selectable(convo, convo.data.entries.len(), -1) {
            self.mode = Mode::Select(last);
        }
    }

    fn select_key(&mut self, at: usize, key: KeyEvent, send: &impl Fn(Command)) {
        let Some(convo) = self.current() else { return };
        let Some(entry) = convo.data.entries.get(at).cloned() else {
            return;
        };
        let len = convo.data.entries.len();
        let mut next = Mode::Select(at);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(i) = selectable(convo, at, -1) {
                    next = Mode::Select(i);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                next = match selectable(convo, at, 1) {
                    Some(i) => Mode::Select(i),
                    None => Mode::Normal,
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(i) = selectable(convo, 0, 0) {
                    next = Mode::Select(i);
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(i) = selectable(convo, len, -1) {
                    next = Mode::Select(i);
                }
            }
            KeyCode::Char('r') => {
                self.reply = Some(entry.id.clone());
                next = Mode::Normal;
            }
            KeyCode::Char('o') | KeyCode::Enter => match &entry.body {
                Body::File { path, .. } if path.exists() => {
                    open_path(path);
                    self.flash(format!("opening {}", picker::tilde(path)));
                }
                Body::File { .. } => self.flash_error("that file is no longer there"),
                _ => {
                    self.reply = Some(entry.id.clone());
                    next = Mode::Normal;
                }
            },
            KeyCode::Char('f') => {
                if let Body::File { path, .. } = &entry.body {
                    reveal_path(path);
                }
            }
            KeyCode::Char('y') => {
                let text = match &entry.body {
                    Body::Text(t) | Body::Notice(t) => t.clone(),
                    Body::File { path, .. } => path.display().to_string(),
                };
                copy_to_clipboard(&text);
                self.flash("copied");
            }
            KeyCode::Char('R') if entry.mine => {
                self.retry(&entry, send);
                next = Mode::Normal;
            }
            KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('q') => next = Mode::Normal,
            _ => {}
        }
        self.mode = next;
    }

    fn retry(&mut self, entry: &Entry, send: &impl Fn(Command)) {
        let Some(to) = self.selected.clone() else { return };
        if self.in_group() {
            if let Body::Text(text) = &entry.body
                && matches!(entry.status, Status::Failed(_))
            {
                send(Command::SendGroup {
                    group: to,
                    text: text.clone(),
                    reply: None,
                    from_home: false,
                });
            } else {
                self.flash("nothing to retry");
            }
            return;
        }
        match &entry.body {
            Body::Text(text) if matches!(entry.status, Status::Failed(_)) => send(Command::Send {
                to,
                text: text.clone(),
                reply: None,
            }),
            Body::File {
                path,
                transfer: Transfer::Failed(_) | Transfer::Interrupted,
                ..
            } => {
                self.send_file(to, path.clone(), send);
            }
            _ => self.flash("nothing to retry"),
        }
    }

    fn on_paste(&mut self, text: &str, send: &impl Fn(Command)) {
        match &mut self.mode {
            Mode::Picker(picker) => {
                picker.filter.push_str(text.trim());
                return;
            }
            Mode::Devices(panel) => {
                if let Some(name) = &mut panel.renaming {
                    name.extend(text.chars().filter(|c| !c.is_control()).take(32));
                }
                return;
            }
            Mode::Normal => {}
            _ => self.mode = Mode::Normal,
        }
        // Dropping files on a terminal pastes their paths.
        if let Some(paths) = dropped_paths(text) {
            self.attach(paths);
        } else {
            self.editor.insert_str(text);
            self.edited(send);
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let inside = |r: Rect| {
            mouse.column >= r.x
                && mouse.column < r.x + r.width
                && mouse.row >= r.y
                && mouse.row < r.y + r.height
        };
        match mouse.kind {
            MouseEventKind::ScrollUp => match &mut self.mode {
                Mode::Picker(p) => p.up(),
                _ if inside(self.chat_area) => self.scroll += 3,
                _ => {}
            },
            MouseEventKind::ScrollDown => match &mut self.mode {
                Mode::Picker(p) => p.down(),
                _ if inside(self.chat_area) => self.scroll = self.scroll.saturating_sub(3),
                _ => {}
            },
            MouseEventKind::Down(MouseButton::Left) if matches!(self.mode, Mode::Normal) => {
                let hit = self
                    .peer_rows
                    .iter()
                    .find(|(r, _)| inside(*r))
                    .map(|(_, a)| a.clone());
                if hit.is_some() {
                    self.select(hit, &|_| {});
                }
            }
            _ => {}
        }
    }

    fn attach(&mut self, paths: Vec<PathBuf>) {
        let mut added = 0;
        for path in paths {
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > engine::MAX_FILE_BYTES => {
                    self.flash_error(format!("{} is over 100 MB", file_name(&path)));
                }
                Ok(m) if m.is_file() => {
                    if !self.attachments.contains(&path) {
                        self.attachments.push(path);
                        added += 1;
                    }
                }
                _ => self.flash_error(format!("can't read {}", picker::tilde(&path))),
            }
        }
        if added > 0 {
            self.flash(format!(
                "attached {added} file{} · Enter sends",
                if added == 1 { "" } else { "s" }
            ));
        }
    }

    fn send_file(&mut self, to: String, path: PathBuf, send: &impl Fn(Command)) {
        let file_id = engine::new_file_id();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.owners.insert(file_id.clone(), to.clone());
        self.convo_mut(&to).push(Entry {
            id: file_id.clone(),
            mine: true,
            at_ms: now_ms(),
            body: Body::File {
                file_id: file_id.clone(),
                name: file_name(&path),
                size,
                path: path.clone(),
                transfer: Transfer::Queued,
            },
            status: Status::Queued,
            hops: 0,
            reply: None,
            read: false,
            sender: None,
        });
        send(Command::SendFile { to, path, file_id });
    }

    fn submit(&mut self, send: &impl Fn(Command)) {
        let text = self.editor.text().trim().to_string();
        if let Some(command) = text.strip_prefix('/') {
            self.editor.take();
            return self.run_command(command, send);
        }
        let Some(to) = self.selected.clone() else {
            if !text.is_empty() || !self.attachments.is_empty() {
                self.flash_error("no one to send to yet");
            }
            return;
        };
        if self.in_group() {
            if !self.attachments.is_empty() {
                self.attachments.clear();
                self.flash_error("files can't go to a group yet; send them to one person");
            }
            if !text.is_empty() {
                let reply = self.reply_target().map(|e| Quote {
                    id: e.id.clone(),
                    sender: e.sender.clone().unwrap_or_else(|| self.me_address.clone()),
                    text: render::entry_summary(e),
                });
                send(Command::SendGroup {
                    group: to,
                    text,
                    reply,
                    from_home: false,
                });
            }
            self.editor.take();
            self.reply = None;
            self.scroll = 0;
            return;
        }
        for path in std::mem::take(&mut self.attachments) {
            self.send_file(to.clone(), path, send);
        }
        if !text.is_empty() {
            let reply = self.reply_target().map(|e| Quote {
                id: e.id.clone(),
                sender: if e.mine { self.me_address.clone() } else { to.clone() },
                text: render::entry_summary(e),
            });
            send(Command::Send {
                to,
                text,
                reply,
            });
        }
        self.editor.take();
        self.reply = None;
        self.scroll = 0;
        self.stop_typing(send);
    }

    fn run_command(&mut self, line: &str, send: &impl Fn(Command)) {
        let (name, arg) = line.split_once(' ').unwrap_or((line, ""));
        let arg = arg.trim();
        match name {
            "file" | "send" | "attach" if !arg.is_empty() => {
                let paths = dropped_paths(arg)
                    .unwrap_or_else(|| vec![expand_tilde(arg)]);
                self.attach(paths);
                if let Some(to) = self.selected.clone() {
                    for path in std::mem::take(&mut self.attachments) {
                        self.send_file(to.clone(), path, send);
                    }
                }
            }
            "file" | "send" | "attach" => {
                let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
                self.mode = Mode::Picker(Picker::new(start));
            }
            "downloads" | "dl" => {
                let _ = std::fs::create_dir_all(&self.downloads);
                open_path(&self.downloads);
            }
            "clear" => {
                if let Some(convo) = self.current_mut() {
                    convo.data.entries.clear();
                    let address = convo.data.address.clone();
                    convo.data.dirty = false;
                    if let Some(store) = &self.store {
                        store.forget(&address);
                    }
                    self.flash("conversation cleared on this device");
                }
            }
            "info" | "whois" => {
                if let Some(convo) = self.current() {
                    let text = format!(
                        "{} is {} · {} · {}",
                        convo.data.nick,
                        convo.data.address,
                        if convo.online { "online" } else { "offline" },
                        if convo.secure { "MLS session up" } else { "no session yet" }
                    );
                    let address = convo.data.address.clone();
                    self.convo_mut(&address).push(notice(text));
                }
            }
            "me" | "whoami" => {
                let text = format!("you are {} · {}", self.me_nick, self.me_address);
                copy_to_clipboard(&self.me_address);
                self.flash(format!("{text} (address copied)"));
            }
            "group" | "newgroup" if !arg.is_empty() => {
                send(Command::CreateGroup { name: arg.to_string() });
            }
            "group" | "newgroup" => self.flash_error("name it: /group NAME"),
            "invite" | "add" => {
                let Some(group) = self.selected.clone().filter(|_| self.in_group()) else {
                    return self.flash_error("open a group first, then /invite NICK");
                };
                match self.find_peer(arg) {
                    Some(member) => {
                        let who = self.nick_of(&member);
                        send(Command::Invite { group, member });
                        self.flash(format!("inviting {who}…"));
                    }
                    None => self.flash_error(format!("no one nearby called {arg}")),
                }
            }
            "leave" => match self.selected.clone().filter(|_| self.in_group()) {
                Some(group) => send(Command::Leave { group }),
                None => self.flash_error("/leave works in a group"),
            },
            "members" | "who" => {
                if let Some(convo) = self.current().filter(|c| c.data.group) {
                    let names: Vec<String> =
                        convo.data.members.iter().map(|m| self.nick_of(m)).collect();
                    let text = format!("members: {}", names.join(", "));
                    let address = convo.data.address.clone();
                    self.convo_mut(&address).push(notice(text));
                }
            }
            "devices" | "home" if arg.is_empty() => self.open_devices(),
            "home" => {
                let group = self.selected.clone().filter(|_| self.in_group());
                let _ = self.home.send(home::Request::Configure {
                    group,
                    args: arg.to_string(),
                });
            }
            "log" => self.show_log = !self.show_log,
            "help" | "?" => self.mode = Mode::Help,
            "quit" | "exit" | "q" => self.quit = true,
            _ => self.flash_error(format!("unknown command /{name} · /help lists them")),
        }
    }
}

impl App {
    fn open_devices(&mut self) {
        self.mode = Mode::Devices(Devices::default());
        let _ = self.home.send(home::Request::Scan);
    }

    /// The open group, and whether home is on in it.
    pub fn home_here(&self) -> Option<(String, String, bool)> {
        let convo = self.current().filter(|c| c.data.group && c.online)?;
        let on = self.home_view.groups.contains(&convo.data.address);
        Some((convo.data.address.clone(), convo.data.nick.clone(), on))
    }

    fn toggle_home_here(&mut self) {
        match self.home_here() {
            Some((group, _, on)) => {
                let _ = self.home.send(home::Request::SetGroup { group, on: !on });
            }
            None => self.flash_error("open a group first; g then lets that group use your devices"),
        }
    }

    fn in_group(&self) -> bool {
        self.current().is_some_and(|c| c.data.group)
    }

    /// A person by nick (case-insensitive) or by address.
    fn find_peer(&self, who: &str) -> Option<String> {
        let who = who.trim().trim_start_matches('@');
        if who.is_empty() {
            return None;
        }
        self.convos
            .iter()
            .filter(|c| !c.data.group)
            .find(|c| c.data.address == who || c.data.nick.eq_ignore_ascii_case(who))
            .map(|c| c.data.address.clone())
    }
}

fn picker_key(picker: &mut Picker, key: KeyEvent) -> Option<Vec<PathBuf>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Up => picker.up(),
        KeyCode::Down => picker.down(),
        KeyCode::Char('p') if ctrl => picker.up(),
        KeyCode::Char('n') if ctrl => picker.down(),
        KeyCode::PageUp => picker.page(-10),
        KeyCode::PageDown => picker.page(10),
        KeyCode::Left => picker.parent(),
        KeyCode::Char('~') if picker.filter.is_empty() => picker.home(),
        KeyCode::Char(' ') => picker.toggle_mark(),
        KeyCode::Right | KeyCode::Enter | KeyCode::Tab => {
            if let Outcome::Chosen(paths) = picker.choose() {
                return Some(paths);
            }
        }
        KeyCode::Backspace => picker.backspace(),
        KeyCode::Char(c) if !ctrl => picker.type_char(c),
        _ => {}
    }
    None
}

/// The next entry from `from` in direction `step` that a person can act
/// on (not a notice). `step` 0 searches forward from `from` inclusive.
fn selectable(convo: &Convo, from: usize, step: isize) -> Option<usize> {
    let entries = &convo.data.entries;
    let ok = |i: usize| !matches!(entries[i].body, Body::Notice(_));
    match step {
        0 => (from..entries.len()).find(|&i| ok(i)),
        s if s < 0 => (0..from.min(entries.len())).rev().find(|&i| ok(i)),
        _ => (from + 1..entries.len()).find(|&i| ok(i)),
    }
}

fn quoted(q: Quote, me: &str) -> Quoted {
    Quoted {
        id: q.id,
        mine: q.sender == me,
        text: q.text,
    }
}

fn notice(text: String) -> Entry {
    Entry {
        id: String::new(),
        mine: false,
        at_ms: now_ms(),
        body: Body::Notice(text),
        status: Status::Sent,
        hops: 0,
        reply: None,
        read: true,
        sender: None,
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(60).collect();
    if line.chars().count() > 60 || text.lines().count() > 1 {
        out.push('…');
    }
    out
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn expand_tilde(text: &str) -> PathBuf {
    match (text.strip_prefix("~/"), picker::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if text == "~" => picker::home_dir().unwrap_or_default(),
        _ => PathBuf::from(text),
    }
}

/// Reads pasted text as the paths a terminal pastes for dropped files:
/// shell-escaped or quoted, space separated, or `file://` URLs. `None`
/// unless every word names an existing file.
fn dropped_paths(text: &str) -> Option<Vec<PathBuf>> {
    let text = text.trim();
    if text.is_empty() || text.len() > 64 * 1024 {
        return None;
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    let mut started = false;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\\') => {
                word.extend(chars.next());
                started = true;
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (_, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    let paths: Vec<PathBuf> = words
        .iter()
        .map(|w| match w.strip_prefix("file://") {
            Some(rest) => PathBuf::from(percent_decode(rest)),
            None => expand_tilde(w),
        })
        .collect();
    let all_files = !paths.is_empty() && paths.iter().all(|p| p.is_absolute() || w_relative(p));
    (all_files && paths.iter().all(|p| p.is_file())).then_some(paths)
}

/// Relative paths count only when they plainly look like one, so pasting a
/// single word that happens to match a file here stays text.
fn w_relative(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with("./") || s.starts_with("../")
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open_path(path: &Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Shows the file in its folder.
fn reveal_path(path: &Path) {
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    } else if let Some(dir) = path.parent() {
        open_path(dir);
    }
}

/// OSC 52: the terminal puts it on the system clipboard, even over SSH.
fn copy_to_clipboard(text: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dropped_paths() {
        let dir = std::env::temp_dir().join(format!("hopchat-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("my file.txt");
        let b = dir.join("b.png");
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "x").unwrap();
        let escaped = format!("{} {}", a.display().to_string().replace(' ', "\\ "), b.display());
        assert_eq!(dropped_paths(&escaped), Some(vec![a.clone(), b.clone()]));
        let quoted = format!("'{}'", a.display());
        assert_eq!(dropped_paths(&quoted), Some(vec![a.clone()]));
        let url = format!("file://{}", a.display().to_string().replace(' ', "%20"));
        assert_eq!(dropped_paths(&url), Some(vec![a]));
        assert_eq!(dropped_paths("hello there"), None);
        assert_eq!(dropped_paths(&format!("see {}", b.display())), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn text(id: &str, mine: bool, body: &str, status: Status) -> Entry {
        Entry {
            id: id.into(),
            mine,
            at_ms: now_ms(),
            body: Body::Text(body.into()),
            status,
            hops: 1,
            reply: None,
            read: true,
            sender: None,
        }
    }

    fn test_app() -> App {
        let (home, _) = std::sync::mpsc::channel();
        App::new("alice".into(), "off1alice000000000000".into(), None, PathBuf::from("/tmp/dl"), home)
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn renders_the_devices_panel() {
        use crate::home::{DeviceView, Kind, ModelState, State, View};
        let mut app = test_app();
        let send = |_: Command| {};
        app.apply(
            Update::Group { id: "g1".into(), name: "house".into(), members: vec!["off1alice000000000000".into()] },
            &send,
        );
        app.selected = Some("g1".into());
        let device = |name: &str, kind, ip: [u8; 4], added, state, detail: &str| DeviceView {
            id: name.into(),
            kind,
            name: name.into(),
            ip: ip.into(),
            added,
            state,
            detail: detail.into(),
        };
        app.home_view = View {
            devices: vec![
                device("Desk lamp", Kind::Light, [192, 168, 1, 10], true, State::Ready, "on · 100% · focus"),
                device("TV", Kind::Tv, [192, 168, 1, 30], false, State::Pairing { until: Instant::now() + Duration::from_secs(48) }, ""),
                device("Light 2", Kind::Light, [192, 168, 1, 11], false, State::New, "Wiz · colour"),
            ],
            scanning: true,
            model: "qwen2.5:7b".into(),
            model_state: ModelState::NoServer,
            groups: Vec::new(),
        };
        app.mode = Mode::Devices(Devices { selected: 1, ..Default::default() });
        let screen = screen(&mut app);
        println!("{screen}");
        for needle in [
            "Devices", "scanning", "Your devices", "Desk lamp", "on · 100% · focus", "Found on this network",
            "waiting for the TV · 4", "it's asking whether to allow hopchat", "Wiz · colour",
            "ollama serve", "house", "g lets this group in",
        ] {
            assert!(screen.contains(needle), "missing {needle:?}");
        }
    }

    #[test]
    fn renders_a_group_with_senders() {
        let mut app = test_app();
        let send = |_: Command| {};
        app.apply(Update::PeerSeen { address: "off1bob00000000000000".into(), nick: "bob".into() }, &send);
        app.apply(
            Update::Group {
                id: "g1".into(),
                name: "house".into(),
                members: vec!["off1alice000000000000".into(), "off1bob00000000000000".into()],
            },
            &send,
        );
        app.apply(
            Update::GroupIncoming {
                group: "g1".into(),
                from: "off1bob00000000000000".into(),
                id: "m1".into(),
                text: "@home dim the lights".into(),
                reply_to: None,
            },
            &send,
        );
        app.apply(
            Update::GroupQueued { group: "g1".into(), id: "m2".into(), text: "🏠 light is now on at 30%".into(), reply: None },
            &send,
        );
        app.selected = Some("g1".into());
        let screen = screen(&mut app);
        println!("{screen}");
        for needle in ["# house", "2 members", "bob", "@home dim the lights", "you", "light is now on at 30%"] {
            assert!(screen.contains(needle), "missing {needle:?}");
        }
        assert!(app.find_peer("BOB").is_some());
        assert!(app.find_peer("house").is_none(), "a group is not a person to invite");
    }

    #[test]
    fn renders_a_conversation() {
        let mut app = test_app();
        let convo = app.convo_mut("off1bob00000000000000");
        convo.data.nick = "bob".into();
        convo.online = true;
        convo.secure = true;
        convo.push(text("m1", false, "hey, can you send me the deck?", Status::Sent));
        let mut reply = text("m2", true, "sure, one sec", Status::Read);
        reply.reply = Some(Quoted { id: "m1".into(), mine: false, text: "hey, can you send me the deck?".into() });
        convo.push(reply);
        convo.push(Entry {
            id: "f1".into(),
            mine: true,
            at_ms: now_ms(),
            body: Body::File {
                file_id: "f1".into(),
                name: "q3-deck.pdf".into(),
                size: 4_200_000,
                path: "/tmp/q3-deck.pdf".into(),
                transfer: Transfer::Moving { done: 40, total: 100 },
            },
            status: Status::Queued,
            hops: 0,
            reply: None,
            read: true,
            sender: None,
        });
        convo.typing_until = Some(Instant::now() + Duration::from_secs(5));
        app.convo_mut("off1carol0000000000000").data.nick = "carol".into();
        app.selected = Some("off1bob00000000000000".into());
        app.editor.set("looks good");

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        println!("{screen}");
        for needle in ["bob", "carol", "Today", "sure, one sec", "✓✓ read", "q3-deck.pdf", "40%", "bob is typing", "looks good"] {
            assert!(screen.contains(needle), "missing {needle:?}");
        }
    }
}
