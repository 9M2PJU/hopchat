//! Drawing: everything here reads the app and paints; nothing changes
//! state except the remembered hit areas and the scroll clamp.

use chrono::{DateTime, Datelike, Local, TimeZone};
use offline_protocol::Event;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::devices::{self, Devices, Primary};
use super::groups::{Candidate, Focus, GroupPanel};
use super::picker::{self, Picker};
use crate::home::{self, DeviceView, ModelState, State};
use super::{App, Convo, Mode};
use crate::history::{Body, Entry, Status, Transfer};

pub const ACCENT: Color = Color::Rgb(255, 170, 60);
pub const THEIRS: Color = Color::Rgb(110, 200, 230);
pub const OK: Color = Color::Rgb(120, 210, 120);
pub const WARN: Color = Color::Rgb(230, 200, 90);
pub const ERR: Color = Color::Rgb(235, 100, 100);
pub const MUTED: Color = Color::Rgb(120, 120, 130);
pub const FAINT: Color = Color::Rgb(70, 70, 80);
pub const READ: Color = Color::Rgb(100, 170, 255);
const SELECTED_BG: Color = Color::Rgb(45, 45, 58);
const SIDEBAR: u16 = 30;
/// A new header is drawn when the same sender pauses this long.
const GROUP_GAP_MS: i64 = 5 * 60 * 1000;
const INDENT: &str = "  ";

pub const COMMANDS: &[(&str, &str)] = &[
    ("/file", "attach and send files (path, or no path for the picker)"),
    ("/downloads", "open the folder received files are saved in"),
    ("/info", "show who this peer is and how they are connected"),
    ("/group", "start a group and pick who's in it (also Ctrl-G) · /group NAME"),
    ("/invite", "add people to this group · /invite NICK"),
    ("/members", "list who is in this group"),
    ("/leave", "leave this group"),
    ("/devices", "find, pair and test your smart devices (also Ctrl-D)"),
    ("/home", "/home on|off lets this group use your devices · /home add IP"),
    ("/me", "show and copy your own address"),
    ("/clear", "delete this conversation from this device"),
    ("/log", "show or hide the protocol event log"),
    ("/help", "keys and commands"),
    ("/quit", "leave"),
];

/// Commands whose name starts with what is typed, while the name is still
/// being typed.
pub fn command_matches(input: &str) -> Vec<(&'static str, &'static str)> {
    if input.contains(' ') || input.contains('\n') {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .copied()
        .filter(|(name, _)| name.starts_with(input))
        .collect()
}

impl App {
    pub fn draw(&mut self, frame: &mut Frame) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).areas(frame.area());
        let narrow = body.width < 70;
        let side_width = if narrow { 0 } else { SIDEBAR };
        let log_width = if self.show_log && body.width > 110 { 46 } else { 0 };
        let [side, main, log] = Layout::horizontal([
            Constraint::Length(side_width),
            Constraint::Min(30),
            Constraint::Length(log_width),
        ])
        .areas(body);

        if side_width > 0 {
            self.draw_sidebar(frame, side);
        } else {
            self.peer_rows.clear();
        }
        self.draw_main(frame, main);
        if log_width > 0 {
            self.draw_log(frame, log);
        }
        self.draw_footer(frame, footer);

        match &self.mode {
            Mode::Picker(picker) => draw_picker(frame, picker),
            Mode::Devices(panel) => self.draw_devices(frame, panel),
            Mode::Groups(panel) => self.draw_groups(frame, panel),
            Mode::Help => draw_help(frame),
            _ => {}
        }
    }

    fn draw_sidebar(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::new()
            .borders(Borders::RIGHT)
            .border_style(Style::new().fg(FAINT));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [head, list, me] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .areas(inner);

        frame.render_widget(
            Line::from(vec![
                " ◆ ".fg(ACCENT).bold(),
                "hopchat".bold(),
                "  offline · e2e".fg(MUTED),
            ]),
            head,
        );

        let online = self.convos.iter().filter(|c| c.online).count();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    " ".into(),
                    self.me_nick.clone().fg(ACCENT).bold(),
                    format!(" · {}", short(&self.me_address)).fg(MUTED),
                ]),
                Line::from(format!(" {online} nearby").fg(MUTED)),
            ]),
            me,
        );

        let [new_group, list] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(list);
        let hover = matches!(self.mode, Mode::Groups(_));
        let row = Rect::new(new_group.x, new_group.y, new_group.width, 1);
        let label = Line::from(vec![
            if hover { "▌".fg(ACCENT) } else { " ".into() },
            "+ ".fg(ACCENT).bold(),
            "New group".into(),
        ]);
        frame.render_widget(label, row);
        frame.render_widget(Line::from("Ctrl-G ".fg(FAINT)).alignment(Alignment::Right), row);
        self.new_group_row = row;

        self.peer_rows.clear();
        let ordered: Vec<&Convo> = self.ordered();
        if ordered.is_empty() {
            let lines = vec![
                Line::from(""),
                Line::from(" Looking for people…".fg(MUTED)),
                Line::from(""),
                Line::from(" Run hopchat on another".fg(FAINT)),
                Line::from(" device on this network.".fg(FAINT)),
                Line::from(" No internet needed.".fg(FAINT)),
            ];
            frame.render_widget(Paragraph::new(lines), list);
            return;
        }

        let width = list.width as usize;
        let mut y = list.y;
        let mut rows = Vec::new();
        for convo in &ordered {
            if y + 2 > list.y + list.height {
                break;
            }
            let selected = self.selected.as_deref() == Some(&convo.data.address);
            let style = if selected {
                Style::new().bg(SELECTED_BG)
            } else {
                Style::new()
            };
            let dot = match (convo.data.group, convo.online, convo.secure) {
                (true, true, _) => "#".fg(ACCENT).bold(),
                (true, false, _) => "#".fg(FAINT),
                (false, true, true) => "●".fg(OK),
                (false, true, false) => "◐".fg(WARN),
                (false, false, _) => "○".fg(FAINT),
            };
            let marker = if selected { "▌".fg(ACCENT) } else { " ".into() };
            let mut name = Span::raw(clip(&convo.data.nick, width.saturating_sub(10)));
            name = if selected || convo.unread > 0 {
                name.bold()
            } else {
                name
            };
            let badge = if convo.unread > 0 {
                format!(" {} ", convo.unread.min(99))
            } else {
                String::new()
            };
            let used = 3 + UnicodeWidthStr::width(name.content.as_ref());
            let pad = width.saturating_sub(used + badge.width());
            let badge = if convo.unread > 0 {
                Span::styled(badge, Style::new().fg(Color::Black).bg(ACCENT).bold())
            } else {
                badge.fg(FAINT)
            };
            let top = Line::from(vec![marker, dot, " ".into(), name, " ".repeat(pad).into(), badge]);

            let preview = if convo.typing() {
                "typing…".fg(ACCENT).italic()
            } else {
                let text = convo
                    .data
                    .entries
                    .iter()
                    .rev()
                    .find(|e| !matches!(e.body, Body::Notice(_)))
                    .map(|e| {
                        let who = match &e.sender {
                            _ if e.mine => "you: ".to_string(),
                            Some(sender) => format!("{}: ", self.nick_of(sender)),
                            None => String::new(),
                        };
                        format!("{who}{}", entry_summary(e))
                    })
                    .unwrap_or_else(|| short(&convo.data.address));
                clip(&text.replace('\n', " "), width.saturating_sub(4)).fg(MUTED)
            };
            let bottom = Line::from(vec!["   ".into(), preview]);

            let row = Rect::new(list.x, y, list.width, 2);
            frame.render_widget(Paragraph::new(vec![top, bottom]).style(style), row);
            rows.push((row, convo.data.address.clone()));
            y += 3;
        }
        self.peer_rows = rows;
    }

    fn draw_main(&mut self, frame: &mut Frame, area: Rect) {
        let composer_height = self.composer_height(area.width.saturating_sub(4) as usize);
        let [header, history, composer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(composer_height),
        ])
        .areas(area);
        self.chat_area = history;

        self.draw_header(frame, header);
        self.draw_history(frame, history);
        self.draw_composer(frame, composer);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let block = Block::new()
            .borders(Borders::BOTTOM)
            .border_style(Style::new().fg(FAINT));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(convo) = self.current() else {
            frame.render_widget(Line::from("  no conversation yet".fg(MUTED)), inner);
            return;
        };
        let state = match (convo.online, convo.secure) {
            _ if convo.data.group => {
                let n = convo.data.members.len();
                let members = format!("{n} member{}", if n == 1 { "" } else { "s" });
                if convo.online {
                    vec!["# ".fg(ACCENT), members.fg(OK), "  🔒 MLS group".fg(MUTED)]
                } else {
                    vec!["# ".fg(FAINT), "you left this group".fg(MUTED)]
                }
            }
            (true, true) => vec!["● ".fg(OK), "online".fg(OK), "  🔒 end-to-end encrypted".fg(MUTED)],
            (true, false) => vec!["◐ ".fg(WARN), "securing the connection…".fg(WARN)],
            (false, _) => vec!["○ ".fg(FAINT), "offline · messages wait for them".fg(MUTED)],
        };
        let mut spans = vec!["  ".into(), convo.data.nick.clone().bold(), "   ".into()];
        spans.extend(state);
        frame.render_widget(Line::from(spans), inner);
        frame.render_widget(
            Line::from(format!("{}  ", short(&convo.data.address)).fg(FAINT))
                .alignment(Alignment::Right),
            inner,
        );
    }

    fn draw_history(&mut self, frame: &mut Frame, area: Rect) {
        let Some(convo) = self.current() else {
            let lines = vec![
                Line::from(""),
                Line::from("Nobody here yet.".fg(MUTED)).centered(),
                Line::from(""),
                Line::from("hopchat finds people on your local network by itself —".fg(FAINT)).centered(),
                Line::from("start it on another laptop and they'll appear on the left.".fg(FAINT)).centered(),
            ];
            frame.render_widget(Paragraph::new(lines), area);
            return;
        };

        let width = area.width.saturating_sub(2).max(10) as usize;
        let selected = match self.mode {
            Mode::Select(i) => Some(i),
            _ => None,
        };
        let reply_to = self.reply.as_deref();
        let mut rows: Vec<(Line<'static>, Option<usize>)> = Vec::new();
        let mut prev: Option<&Entry> = None;
        for (i, entry) in convo.data.entries.iter().enumerate() {
            if prev.is_none_or(|p| !same_day(p.at_ms, entry.at_ms)) {
                rows.push((day_separator(entry.at_ms, width), None));
            }
            let lines = self.entry_lines(convo, entry, prev, width);
            prev = Some(entry);
            let highlight = selected == Some(i) || (reply_to == Some(entry.id.as_str()) && !entry.id.is_empty());
            for line in lines {
                let line = if highlight { highlight_line(line, width) } else { line };
                rows.push((line, Some(i)));
            }
        }
        if convo.typing() {
            rows.push((Line::from(""), None));
            rows.push((
                Line::from(vec![INDENT.into(), format!("{} is typing…", convo.data.nick).fg(MUTED).italic()]),
                None,
            ));
        }
        if convo.data.entries.is_empty() {
            rows.push((Line::from(""), None));
            let hint = if convo.data.group {
                "A new group. /invite picks people to add; Ctrl-D, then g, lets them use your devices."
            } else if convo.secure {
                "Say hello — or drop a file here."
            } else {
                "Messages you write now are delivered when they're reachable."
            };
            rows.push((Line::from(hint.fg(MUTED)).centered(), None));
        }

        let height = area.height as usize;
        let total = rows.len();
        let max_scroll = total.saturating_sub(height);
        // Keep the selected message in view.
        if let Some(sel) = selected {
            let first = rows.iter().position(|(_, i)| *i == Some(sel)).unwrap_or(0);
            let last = rows.iter().rposition(|(_, i)| *i == Some(sel)).unwrap_or(0);
            let bottom = total - self.scroll.min(max_scroll);
            let top = bottom.saturating_sub(height);
            if last >= bottom {
                self.scroll = total - (last + 1);
            } else if first < top {
                self.scroll = total - (first + height).min(total);
            }
        }
        self.scroll = self.scroll.min(max_scroll);
        let end = total - self.scroll;
        let start = end.saturating_sub(height);
        let visible: Vec<Line> = rows[start..end].iter().map(|(l, _)| l.clone()).collect();
        // Anchor to the bottom, like every chat.
        let pad = height.saturating_sub(visible.len()) as u16;
        let inner = Rect::new(area.x + 1, area.y + pad, area.width.saturating_sub(2), area.height - pad);
        frame.render_widget(Paragraph::new(visible), inner);

        if self.scroll > 0 {
            let label = format!(" ↓ {} more · Esc to jump down ", self.scroll);
            let w = label.width() as u16;
            let at = Rect::new(area.x + area.width.saturating_sub(w + 1), area.y + area.height - 1, w, 1);
            frame.render_widget(Clear, at);
            frame.render_widget(Line::from(label.fg(Color::Black).bg(ACCENT)), at);
        }
    }

    fn entry_lines(
        &self,
        convo: &Convo,
        entry: &Entry,
        prev: Option<&Entry>,
        width: usize,
    ) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if let Body::Notice(text) = &entry.body {
            out.push(Line::from(""));
            for row in wrap(text, width.saturating_sub(8)) {
                out.push(Line::from(format!("· {row} ·").fg(MUTED).italic()).centered());
            }
            return out;
        }
        let continues = prev.is_some_and(|p| {
            p.mine == entry.mine
                && p.sender == entry.sender
                && !matches!(p.body, Body::Notice(_))
                && entry.at_ms - p.at_ms < GROUP_GAP_MS
                && same_day(p.at_ms, entry.at_ms)
        });
        if !continues {
            if prev.is_some() {
                out.push(Line::from(""));
            }
            let (name, color) = match &entry.sender {
                _ if entry.mine => ("you".to_string(), ACCENT),
                Some(sender) => (self.nick_of(sender), THEIRS),
                None => (convo.data.nick.clone(), THEIRS),
            };
            let mut header = vec![name.fg(color).bold(), "  ".into(), clock(entry.at_ms).fg(FAINT)];
            if !entry.mine && entry.hops > 1 {
                header.push(format!("  via {} hops", entry.hops).fg(FAINT));
            }
            out.push(Line::from(header));
        }

        if let Some(q) = &entry.reply {
            let who = match () {
                _ if q.mine => "you".to_string(),
                _ if convo.data.group => "reply".to_string(),
                _ => convo.data.nick.clone(),
            };
            let text = clip(&q.text.replace('\n', " "), width.saturating_sub(who.width() + 8));
            out.push(Line::from(vec![
                INDENT.into(),
                "╭ ".fg(FAINT),
                who.fg(MUTED).bold(),
                format!(" {text}").fg(MUTED),
            ]));
        }

        let tick = entry.mine.then(|| tick(&entry.status));
        match &entry.body {
            Body::Text(text) => {
                let rows = wrap(text, width.saturating_sub(INDENT.len()));
                let n = rows.len();
                for (i, row) in rows.into_iter().enumerate() {
                    let mut spans = vec![INDENT.into(), Span::raw(row.clone())];
                    if i + 1 == n
                        && let Some(tick) = &tick
                    {
                        let used = INDENT.len() + row.width();
                        let need = tick.content.width() + 2;
                        if used + need <= width {
                            spans.push(" ".repeat(width - used - need + 2).into());
                            spans.push(tick.clone());
                        } else {
                            out.push(Line::from(spans));
                            let pad = width.saturating_sub(tick.content.width());
                            out.push(Line::from(vec![" ".repeat(pad).into(), tick.clone()]));
                            continue;
                        }
                    }
                    out.push(Line::from(spans));
                }
            }
            Body::File {
                name,
                size,
                path,
                transfer,
                ..
            } => {
                let icon = file_icon(name);
                let size_text = human_size(*size);
                let name_room = width.saturating_sub(size_text.width() + 10);
                out.push(Line::from(vec![
                    INDENT.into(),
                    "┃ ".fg(if entry.mine { ACCENT } else { THEIRS }),
                    format!("{icon} ").into(),
                    clip(name, name_room).bold(),
                    format!("  {size_text}").fg(MUTED),
                ]));
                let mut detail = vec![INDENT.into(), "┃ ".fg(if entry.mine { ACCENT } else { THEIRS })];
                match transfer {
                    Transfer::Queued => {
                        let why = if convo.online && convo.secure {
                            "starting…"
                        } else {
                            "waiting for them to come online"
                        };
                        detail.push(format!("◷ {why}").fg(MUTED));
                    }
                    Transfer::Moving { done, total } => {
                        let fraction = if *total == 0 { 0.0 } else { *done as f64 / *total as f64 };
                        let bar_width = (width.saturating_sub(30)).clamp(10, 30);
                        detail.extend(progress_bar(fraction, bar_width));
                        detail.push(format!(" {:>3}%", (fraction * 100.0) as u32).bold());
                        if let Some(rate) = self.rate(&entry.id, *size, fraction) {
                            detail.push(format!("  {rate}").fg(MUTED));
                        }
                    }
                    Transfer::Done if entry.mine => {
                        detail.push("sent".fg(MUTED));
                        if let Some(tick) = &tick {
                            detail.push("  ".into());
                            detail.push(tick.clone());
                        }
                    }
                    Transfer::Done => {
                        let saved = clip(&picker::tilde(path), width.saturating_sub(24));
                        detail.push("saved to ".fg(MUTED));
                        detail.push(saved.fg(MUTED).underlined());
                        detail.push("  ↑ o to open".fg(FAINT));
                    }
                    Transfer::Failed(reason) => {
                        detail.push(format!("✗ {reason}").fg(ERR));
                        if entry.mine {
                            detail.push("  ↑ R to retry".fg(FAINT));
                        }
                    }
                    Transfer::Interrupted => {
                        detail.push("interrupted when hopchat closed".fg(WARN));
                        detail.push("  ↑ R to retry".fg(FAINT));
                    }
                }
                out.push(Line::from(detail));
            }
            Body::Notice(_) => {}
        }
        out
    }

    fn rate(&self, id: &str, size: u64, fraction: f64) -> Option<String> {
        let started = self.started.get(id)?;
        let secs = started.elapsed().as_secs_f64();
        if secs < 0.5 || fraction <= 0.0 {
            return None;
        }
        let rate = size as f64 * fraction / secs;
        let left = (size as f64 * (1.0 - fraction) / rate).ceil() as u64;
        Some(format!("{}/s · {}s left", human_size(rate as u64), left))
    }

    fn composer_height(&self, width: usize) -> u16 {
        let (rows, _) = self.editor.layout(width.max(10));
        let extra = !self.attachments.is_empty() as usize + self.reply.is_some() as usize;
        (rows.len().clamp(1, 8) + extra + 2) as u16
    }

    fn draw_composer(&self, frame: &mut Frame, area: Rect) {
        let focused = matches!(self.mode, Mode::Normal);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if focused { MUTED } else { FAINT }));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        if let Some(target) = self.reply_target() {
            let who = if target.mine {
                "yourself".to_string()
            } else {
                self.current().map(|c| c.data.nick.clone()).unwrap_or_default()
            };
            let text = clip(&entry_summary(target).replace('\n', " "), (inner.width as usize).saturating_sub(who.width() + 28));
            lines.push(Line::from(vec![
                "↪ replying to ".fg(MUTED),
                who.fg(ACCENT),
                format!("  {text}").fg(MUTED),
                "  Esc cancels".fg(FAINT),
            ]));
        }
        if !self.attachments.is_empty() {
            let mut spans = Vec::new();
            for path in &self.attachments {
                let name = super::file_name(path);
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                spans.push(
                    format!(" {} {} · {} ", file_icon(&name), clip(&name, 24), human_size(size))
                        .fg(Color::Black)
                        .bg(ACCENT),
                );
                spans.push(" ".into());
            }
            spans.push("⌫ removes".fg(FAINT));
            lines.push(Line::from(spans));
        }

        let text_width = inner.width.saturating_sub(2) as usize;
        let (rows, (cx, cy)) = self.editor.layout(text_width);
        let first_row = cy.saturating_sub(7);
        let top = lines.len() as u16;
        if self.editor.is_empty() {
            let who = self.current().map(|c| c.data.nick.clone());
            let hint = match (&who, self.attachments.is_empty()) {
                (Some(_), false) => "add a message, or just press Enter to send".to_string(),
                (Some(nick), true) => format!("Message {nick}   ·   Ctrl-O attach · ↑ select · / commands"),
                (None, _) => "Waiting for someone to talk to   ·   F1 help".to_string(),
            };
            lines.push(Line::from(vec!["› ".fg(ACCENT).bold(), hint.fg(FAINT)]));
        } else {
            for (i, row) in rows.iter().enumerate().skip(first_row).take(8) {
                let prompt = if i == 0 { "› ".fg(ACCENT).bold() } else { "  ".into() };
                let styled = if self.editor.text().starts_with('/') && i == 0 {
                    Span::raw(row.clone()).fg(ACCENT)
                } else {
                    Span::raw(row.clone())
                };
                lines.push(Line::from(vec![prompt, styled]));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
        if focused {
            frame.set_cursor_position(Position::new(
                inner.x + 2 + cx as u16,
                inner.y + top + (cy - first_row) as u16,
            ));
        }

        // Command suggestions float above the composer.
        if focused && self.editor.text().starts_with('/') {
            let matches = command_matches(self.editor.text());
            if !matches.is_empty() {
                let h = matches.len() as u16 + 2;
                let w = area.width.min(64);
                let popup = Rect::new(area.x, area.y.saturating_sub(h), w, h);
                let lines: Vec<Line> = matches
                    .iter()
                    .enumerate()
                    .map(|(i, (name, help))| {
                        let name = format!(" {name:<11}");
                        let name = if i == 0 { name.fg(ACCENT).bold() } else { name.fg(ACCENT) };
                        Line::from(vec![name, help.fg(MUTED)])
                    })
                    .collect();
                frame.render_widget(Clear, popup);
                frame.render_widget(
                    Paragraph::new(lines).block(
                        Block::bordered()
                            .border_type(BorderType::Rounded)
                            .border_style(Style::new().fg(FAINT))
                            .title(" Tab completes ".fg(FAINT)),
                    ),
                    popup,
                );
            }
        }
    }

    fn draw_log(&self, frame: &mut Frame, area: Rect) {
        let block = Block::new()
            .borders(Borders::LEFT)
            .border_style(Style::new().fg(FAINT))
            .title(" protocol ".fg(MUTED));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let height = inner.height as usize;
        let skip = self.log.len().saturating_sub(height);
        let lines: Vec<Line> = self.log.iter().skip(skip).cloned().collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let keys: &[(&str, &str)] = match self.mode {
            Mode::Select(_) => &[
                ("↑↓", "move"),
                ("r", "reply"),
                ("o", "open"),
                ("f", "show in folder"),
                ("y", "copy"),
                ("R", "retry"),
                ("Esc", "back"),
            ],
            Mode::Picker(_) => &[
                ("↑↓", "move"),
                ("Enter", "open/choose"),
                ("Space", "mark"),
                ("←", "up a folder"),
                ("~", "home"),
                ("type", "filter"),
                ("Esc", "cancel"),
            ],
            Mode::Help => &[("any key", "close")],
            Mode::Devices(ref panel) if panel.renaming.is_some() => {
                &[("type", "a name"), ("Enter", "save"), ("Esc", "cancel")]
            }
            Mode::Groups(ref panel) if panel.focus == Focus::Name => &[
                ("type", "a name"),
                ("↓", "pick people"),
                ("Enter", "create"),
                ("Esc", "cancel"),
            ],
            Mode::Groups(ref panel) => &[
                ("↑↓", "move"),
                ("Space", "pick"),
                ("Enter", if panel.target.is_some() { "add them" } else { "create" }),
                ("Esc", "cancel"),
            ],
            Mode::Devices(_) => &[
                ("↑↓", "move"),
                ("Enter", "add/pair/test"),
                ("n", "rename"),
                ("x", "forget"),
                ("r", "rescan"),
                ("g", "access"),
                ("Esc", "close"),
            ],
            Mode::Normal => &[
                ("Enter", "send"),
                ("Ctrl-O", "attach"),
                ("Ctrl-G", "new group"),
                ("Ctrl-D", "devices"),
                ("Tab", "next chat"),
                ("↑", "select"),
                ("F1", "help"),
            ],
        };
        let mut spans = vec![" ".into()];
        for (key, what) in keys {
            spans.push(key.to_string().fg(ACCENT).bold());
            spans.push(format!(" {what}  ").fg(MUTED));
        }
        frame.render_widget(Line::from(spans), area);

        // Right side: what's happening.
        let mut right: Vec<Span> = Vec::new();
        if let Some((text, error)) = self.flash_text() {
            right.push(clip(text, 60).fg(if error { ERR } else { OK }));
            right.push("  ".into());
        }
        for incoming in self.incoming.values().take(2) {
            let pct = (incoming.done * 100).checked_div(incoming.total).unwrap_or(0);
            right.push(
                format!("⇣ {} {}% of {}  ", clip(&incoming.name, 18), pct, human_size(incoming.size))
                    .fg(THEIRS),
            );
        }
        if !self.show_log {
            right.push("Ctrl-E log ".fg(FAINT));
        }
        let line = Line::from(right).alignment(Alignment::Right);
        let width = line.width() as u16;
        let at = Rect::new(area.x + area.width.saturating_sub(width), area.y, width.min(area.width), 1);
        frame.render_widget(Clear, at);
        frame.render_widget(line, at);
    }
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner() -> &'static str {
    let ms = chrono::Utc::now().timestamp_millis();
    SPINNER[(ms / 90) as usize % SPINNER.len()]
}

impl App {
    fn draw_devices(&self, frame: &mut Frame, panel: &Devices) {
        let view = &self.home_view;
        let area = centered(frame.area(), 78, 82);
        frame.render_widget(Clear, area);
        let status = if view.scanning {
            format!(" {} scanning ", spinner()).fg(ACCENT)
        } else {
            " r rescans ".fg(MUTED)
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(" Devices ".fg(ACCENT).bold())
            .title(Line::from(status).alignment(Alignment::Right));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [list, help, footer] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .areas(inner);

        let width = list.width as usize;
        let selected = panel.selected.min(view.devices.len().saturating_sub(1));
        let mut lines: Vec<Line> = Vec::new();
        let mut selected_row = 0;
        let added = view.devices.iter().filter(|d| d.added).count();
        for (i, d) in view.devices.iter().enumerate() {
            if i == 0 && d.added {
                lines.push(Line::from(" Your devices".fg(MUTED).bold()));
            }
            if i == added {
                if added > 0 {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(" Found on this network".fg(MUTED).bold()));
            }
            if i == selected {
                selected_row = lines.len();
            }
            lines.push(device_line(d, i == selected, panel, width));
        }
        if view.devices.is_empty() {
            lines.push(Line::from(""));
            if view.scanning {
                lines.push(Line::from(format!("  {} Looking for Wiz lights and LG TVs on this network…", spinner()).fg(MUTED)));
            } else {
                lines.push(Line::from("  Nothing found.".fg(MUTED)));
                lines.push(Line::from(""));
                lines.push(Line::from("  Devices have to be on the same network as this machine, and switched on.".fg(FAINT)));
                lines.push(Line::from("  r scans again · /home add IP adds one by its address".fg(FAINT)));
            }
        } else if view.scanning && !view.devices.iter().any(|d| !d.added) {
            lines.push(Line::from(""));
            lines.push(Line::from(format!("  {} looking for more…", spinner()).fg(FAINT)));
        }
        if !view.scanning && !view.devices.iter().any(|d| d.kind == home::Kind::Tv) {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                "  📺 No TV? ".fg(MUTED),
                "A TV only shows up while it's switched on. Turn it on, then ".fg(FAINT),
                "r".fg(ACCENT).bold(),
                ".".fg(FAINT),
            ]));
        }
        // Keep the selection in view.
        let height = list.height as usize;
        let skip = (selected_row + 2).saturating_sub(height);
        let lines: Vec<Line> = lines.into_iter().skip(skip).collect();
        frame.render_widget(Paragraph::new(lines), list);

        let current = panel.current(view);
        frame.render_widget(Paragraph::new(device_help(current, panel)), help);

        frame.render_widget(Paragraph::new(self.devices_footer(width)), footer);
    }

    fn devices_footer(&self, width: usize) -> Vec<Line<'static>> {
        let view = &self.home_view;
        let rule = Line::from("─".repeat(width).fg(FAINT));
        let model = match view.model_state {
            ModelState::Ready => vec!["● ".fg(OK), view.model.clone().bold(), " ready".fg(MUTED)],
            ModelState::Missing => vec![
                "◐ ".fg(WARN),
                format!("{} isn't downloaded · ", view.model).fg(WARN),
                format!("ollama pull {}", view.model).fg(ACCENT),
            ],
            ModelState::NoServer => vec![
                "○ ".fg(ERR),
                "Ollama isn't running · start it with ".fg(WARN),
                "ollama serve".fg(ACCENT),
            ],
            ModelState::Unknown => vec![format!("{} ", spinner()).fg(MUTED), "checking…".fg(MUTED)],
        };
        let mut model_line = vec!["  Model   ".fg(MUTED)];
        model_line.extend(model);
        let access = match self.home_here() {
            Some((_, name, true)) => vec![
                "# ".fg(ACCENT),
                name.bold(),
                " can use them".fg(OK),
                "  · g switches off".fg(FAINT),
            ],
            Some((_, name, false)) => vec![
                "# ".fg(FAINT),
                name.bold(),
                " can't use them".fg(MUTED),
                "  · g lets this group in".fg(ACCENT),
            ],
            None => {
                let n = view.groups.len();
                vec![
                    format!("on in {n} group{}", if n == 1 { "" } else { "s" }).fg(MUTED),
                    "  · open a group, then g to let it in".fg(FAINT),
                ]
            }
        };
        let mut access_line = vec!["  Access  ".fg(MUTED)];
        access_line.extend(access);
        vec![rule, Line::from(model_line), Line::from(access_line)]
    }
}

impl App {
    fn draw_groups(&self, frame: &mut Frame, panel: &GroupPanel) {
        let people = self.candidates(panel);
        let area = centered(frame.area(), 60, 70);
        frame.render_widget(Clear, area);
        let title = match &panel.target {
            Some((_, name)) => format!(" Add people to #{name} "),
            None => " New group ".to_string(),
        };
        let picked = people.iter().filter(|p| panel.is_picked(&p.address)).count();
        let count = if picked == 0 {
            " Space picks people ".fg(MUTED)
        } else {
            format!(" {picked} picked ").fg(ACCENT)
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(title.fg(ACCENT).bold())
            .title(Line::from(count).alignment(Alignment::Right));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let name_height = if panel.target.is_some() { 0 } else { 2 };
        let [name, list, help] = Layout::vertical([
            Constraint::Length(name_height),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .areas(inner);

        if panel.target.is_none() {
            let naming = panel.focus == Focus::Name;
            let field = if panel.name.is_empty() && !naming {
                "untitled".fg(FAINT)
            } else if panel.name.is_empty() {
                "e.g. house, weekend trip".fg(FAINT)
            } else {
                Span::raw(panel.name.clone()).bold()
            };
            let label = if naming { " Name  › ".fg(ACCENT).bold() } else { " Name  › ".fg(MUTED) };
            frame.render_widget(Line::from(vec![label, "#".fg(ACCENT), field]), name);
            if naming {
                let x = name.x + 10 + panel.name.width() as u16;
                frame.set_cursor_position(Position::new(x.min(name.right().saturating_sub(1)), name.y));
            }
        }

        let width = list.width as usize;
        let mut lines = vec![Line::from(" People".fg(MUTED).bold())];
        let in_list = panel.focus == Focus::People;
        for (i, person) in people.iter().enumerate() {
            lines.push(candidate_line(person, panel, in_list && i == panel.cursor, width));
        }
        if people.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from("  Nobody nearby yet.".fg(MUTED)));
            lines.push(Line::from("  You can still create it now and add people later with /invite.".fg(FAINT)));
        }
        let height = list.height as usize;
        let skip = (panel.cursor + 3).saturating_sub(height);
        let lines: Vec<Line> = lines.into_iter().skip(skip).collect();
        frame.render_widget(Paragraph::new(lines), list);

        let key = |k: &str| k.to_string().fg(ACCENT).bold();
        let say = |t: &str| t.to_string().fg(MUTED);
        let names: Vec<String> = people
            .iter()
            .filter(|p| panel.is_picked(&p.address))
            .map(|p| p.nick.clone())
            .collect();
        let who = if names.is_empty() { "just you".to_string() } else { format!("you, {}", names.join(", ")) };
        let first = match (panel.warning, &panel.target) {
            (Some(warning), _) => Line::from(vec!["  ".into(), warning.fg(WARN)]),
            (None, Some(_)) => Line::from(vec!["  ".into(), key("Space"), say(" picks someone · "), key("Enter"), say(" invites them")]),
            (None, None) if panel.focus == Focus::Name => Line::from(vec!["  ".into(), say("Name it, then "), key("↓"), say(" to pick people · "), key("Enter"), say(" creates it")]),
            (None, None) => Line::from(vec!["  ".into(), key("Space"), say(" picks · "), key("↑"), say(" back to the name · "), key("Enter"), say(" creates it")]),
        };
        let second = match &panel.target {
            Some(_) => Line::from(vec!["  ".into(), clip(&format!("Adding: {}", if names.is_empty() { "nobody yet".into() } else { names.join(", ") }), width.saturating_sub(4)).fg(FAINT)]),
            None => Line::from(vec!["  ".into(), clip(&format!("In it: {who}"), width.saturating_sub(4)).fg(FAINT)]),
        };
        frame.render_widget(Paragraph::new(vec![Line::from(""), first, second]), help);
    }
}

fn candidate_line(person: &Candidate, panel: &GroupPanel, selected: bool, width: usize) -> Line<'static> {
    let marker = if selected { "▌".fg(ACCENT) } else { " ".into() };
    let pickable = person.pickable();
    let check = match () {
        _ if person.member => "[✓]".fg(FAINT),
        _ if panel.is_picked(&person.address) => "[✓]".fg(ACCENT).bold(),
        _ if pickable => "[ ]".fg(MUTED),
        _ => "[ ]".fg(FAINT),
    };
    let (dot, state) = match (person.member, person.online, person.secure) {
        (true, _, _) => ("●".fg(FAINT), "already in the group".fg(FAINT)),
        (false, true, true) => ("●".fg(OK), "online".fg(MUTED)),
        (false, true, false) => ("◐".fg(WARN), "connecting…".fg(WARN)),
        (false, false, _) => ("○".fg(FAINT), "offline · can't be added now".fg(FAINT)),
    };
    let name = clip(&person.nick, 20);
    let pad = 22usize.saturating_sub(name.width());
    let name = if !pickable {
        name.fg(FAINT)
    } else if selected {
        name.bold()
    } else {
        name.into()
    };
    // The address tells two people with one nick apart, when there's room.
    let address = if width >= 66 { short(&person.address) } else { String::new() };
    let line = Line::from(vec![
        marker,
        " ".into(),
        check,
        " ".into(),
        dot,
        " ".into(),
        name,
        " ".repeat(pad).into(),
        state,
        "  ".into(),
        address.fg(FAINT),
    ]);
    if selected { line.style(Style::new().bg(SELECTED_BG)) } else { line }
}

fn device_line(d: &DeviceView, selected: bool, panel: &Devices, width: usize) -> Line<'static> {
    let icon = match d.kind {
        home::Kind::Light => "💡",
        home::Kind::Tv => "📺",
    };
    let (dot, state) = match &d.state {
        State::Ready => ("●".fg(OK), d.detail.clone().fg(Color::Reset)),
        State::Offline => ("○".fg(FAINT), d.detail.clone().fg(MUTED)),
        State::Checking => (spinner().fg(MUTED), "checking…".fg(MUTED)),
        State::New => ("✦".fg(ACCENT), d.detail.clone().fg(MUTED)),
        State::Pairing { until } => {
            let left = until.saturating_duration_since(std::time::Instant::now()).as_secs();
            (spinner().fg(WARN), format!("waiting for the TV · {left}s").fg(WARN))
        }
        State::Failed(_) => ("✗".fg(ERR), "didn't work".fg(ERR)),
    };
    let marker = if selected { "▌".fg(ACCENT) } else { " ".into() };
    let name = match &panel.renaming {
        Some(typed) if selected => Span::from(format!("{typed}▏")).fg(ACCENT).bold(),
        _ if selected => Span::from(clip(&d.name, 22)).bold(),
        _ => Span::from(clip(&d.name, 22)),
    };
    let ip = d.ip.to_string();
    let used = 6 + name.content.width().max(22) + 2 + 15 + 2;
    let state = Span::styled(clip(&state.content, width.saturating_sub(used + 2)), state.style);
    let name_pad = 22usize.saturating_sub(name.content.width());
    let line = Line::from(vec![
        marker,
        " ".into(),
        format!("{icon} ").into(),
        name,
        " ".repeat(name_pad + 2).into(),
        format!("{ip:<15}").fg(FAINT),
        "  ".into(),
        dot,
        " ".into(),
        state,
    ]);
    if selected { line.style(Style::new().bg(SELECTED_BG)) } else { line }
}

/// Two lines under the list saying what the selected device needs.
fn device_help(d: Option<&DeviceView>, panel: &Devices) -> Vec<Line<'static>> {
    let key = |k: &str| k.to_string().fg(ACCENT).bold();
    let say = |t: &str| t.to_string().fg(MUTED);
    let Some(d) = d else { return Vec::new() };
    if panel.renaming.is_some() {
        return vec![Line::from(""), Line::from(vec!["  ".into(), say("Type a name people will use, like "), "Desk lamp".bold(), say(". "), key("Enter"), say(" saves.")])];
    }
    if panel.confirm_forget {
        return vec![Line::from(""), Line::from(vec!["  ".into(), format!("Forget {}? ", d.name).fg(WARN), key("x"), say(" again to forget, anything else keeps it.")])];
    }
    let lines = match (&d.state, devices::primary(d)) {
        (State::Pairing { .. }, _) => vec![
            Line::from(vec!["  ".into(), "Look at the TV: it's asking whether to allow hopchat.".fg(WARN).bold()]),
            Line::from(vec!["  ".into(), say("Choose "), "Allow".bold(), say(" with the remote. Nothing to do here.")]),
        ],
        (State::Failed(why), _) => vec![
            Line::from(vec!["  ".into(), clip(why, 90).fg(ERR)]),
            Line::from(vec!["  ".into(), key("Enter"), say(" tries again")]),
        ],
        (_, Primary::Pair) => vec![
            Line::from(vec!["  ".into(), key("Enter"), say(" pairs it. The TV will ask to allow hopchat; have the remote ready.")]),
            Line::from(vec!["  ".into(), say("For \"turn the TV on\" to work, enable "), "Turn on via Wi-Fi".bold(), say(" in its settings.")]),
        ],
        (_, Primary::Add) => vec![
            Line::from(vec!["  ".into(), key("Enter"), say(" adds it. "), key("n"), say(" names it first, e.g. Kitchen.")]),
            Line::from(vec!["  ".into(), say("Not sure which light this is? Add it, then "), key("t"), say(" blinks it.")]),
        ],
        (State::Offline, _) => vec![
            Line::from(vec!["  ".into(), say("Not answering. Is it switched on, and on this network?")]),
            Line::from(vec!["  ".into(), key("r"), say(" checks again · "), key("x"), say(" forgets it")]),
        ],
        _ => {
            let test = match d.kind {
                home::Kind::Light => " blinks it so you can find it",
                home::Kind::Tv => " puts a hello on the screen",
            };
            vec![
                Line::from(vec!["  ".into(), key("Enter"), say(test), say(" · "), key("n"), say(" renames · "), key("x"), say(" forgets")]),
                Line::from(vec!["  ".into(), say("In a group, people ask for it by name: "), match d.kind {
                    home::Kind::Light => format!("@home dim the {}", d.name.to_lowercase()),
                    home::Kind::Tv => format!("@home put netflix on the {}", d.name.to_lowercase()),
                }.fg(THEIRS)]),
            ]
        }
    };
    let mut out = vec![Line::from("")];
    out.extend(lines);
    out.truncate(3);
    out
}

fn draw_picker(frame: &mut Frame, picker: &Picker) {
    let area = centered(frame.area(), 76, 80);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(" Attach files ".fg(ACCENT).bold())
        .title_bottom(
            Line::from(if picker.marked.is_empty() {
                " Space marks several ".to_string()
            } else {
                format!(" {} marked · Enter sends them ", picker.marked.len())
            })
            .fg(MUTED),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [path, filter, list] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .areas(inner);

    frame.render_widget(
        Line::from(vec![" 📁 ".into(), picker::tilde(&picker.dir).bold()]),
        path,
    );
    let filter_line = if picker.filter.is_empty() {
        Line::from(vec![" 🔍 ".into(), "type to filter · . shows hidden files".fg(FAINT)])
    } else {
        Line::from(vec![" 🔍 ".into(), picker.filter.clone().fg(ACCENT)])
    };
    frame.render_widget(filter_line, filter);

    if let Some(error) = &picker.error {
        frame.render_widget(Line::from(format!("  {error}").fg(ERR)), list);
        return;
    }
    let visible = picker.visible();
    if visible.is_empty() {
        frame.render_widget(Line::from("  nothing here".fg(MUTED)), list);
        return;
    }
    let height = list.height as usize;
    let start = picker.selected.saturating_sub(height.saturating_sub(1));
    let width = list.width as usize;
    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(i, entry)| {
            let mark = if picker.is_marked(entry) { "◆ ".fg(ACCENT) } else { "  ".into() };
            let (icon, name, size) = if entry.dir {
                ("📁".to_string(), format!("{}/", entry.name), String::new())
            } else {
                (file_icon(&entry.name).to_string(), entry.name.clone(), human_size(entry.size))
            };
            let name = clip(&name, width.saturating_sub(18));
            let pad = width.saturating_sub(6 + name.width() + size.width());
            let line = Line::from(vec![
                mark,
                format!("{icon} ").into(),
                if entry.dir { name.bold() } else { name.into() },
                " ".repeat(pad).into(),
                size.fg(MUTED),
            ]);
            if i == picker.selected {
                line.style(Style::new().bg(SELECTED_BG))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list);
}

fn draw_help(frame: &mut Frame) {
    let area = centered(frame.area(), 70, 86);
    frame.render_widget(Clear, area);
    let key = |k: &str, what: &str| {
        Line::from(vec![format!("  {k:<16}").fg(ACCENT).bold(), what.to_string().fg(Color::Reset)])
    };
    let head = |t: &str| Line::from(format!(" {t}").fg(MUTED).bold());
    let mut lines = vec![
        Line::from(""),
        head("Chatting"),
        key("Enter", "send (with any attached files)"),
        key("Shift/Alt-Enter", "new line (Ctrl-J works everywhere)"),
        key("Tab · Shift-Tab", "next / previous conversation (also Ctrl-N/P)"),
        key("Alt-1 … 9", "jump to a conversation"),
        key("PgUp · PgDn", "scroll history (or the mouse wheel)"),
        key("Esc", "cancel reply · jump to newest"),
        Line::from(""),
        head("Files"),
        key("Ctrl-O", "browse for files to attach"),
        key("drag & drop", "drop files on the window to attach them"),
        key("/file PATH", "send a file by path"),
        Line::from(""),
        head("Messages  (↑ on an empty line)"),
        key("r · Enter", "reply"),
        key("o · f", "open a file · show it in its folder"),
        key("y", "copy text to the clipboard"),
        key("R", "retry a failed send"),
        Line::from(""),
        head("Editing"),
        key("Ctrl-A · Ctrl-E", "start / end of line"),
        key("Ctrl-W · Ctrl-U", "delete word / to start of line"),
        key("Alt-← · Alt-→", "move by word"),
        Line::from(""),
        head("Elsewhere"),
        key("Ctrl-G", "new group: name it, pick people, Enter"),
        key("Ctrl-D", "devices: find, pair and test them"),
        key("Ctrl-E", "protocol event log"),
        key("Ctrl-C · Ctrl-Q", "quit"),
        Line::from(""),
        Line::from(" Commands".fg(MUTED).bold()),
    ];
    for (name, what) in COMMANDS {
        lines.push(key(name, what));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "  To select text with the mouse, hold Shift (Option in iTerm/Terminal).".fg(FAINT),
    ));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(" hopchat · help ".fg(ACCENT).bold())
        .title_bottom(" ✓ on the wire · ✓✓ delivered · ✓✓ blue: read ".fg(MUTED));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn centered(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let w = (area.width * width_pct / 100).max(40).min(area.width);
    let h = (area.height * height_pct / 100).max(10).min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn progress_bar(fraction: f64, width: usize) -> Vec<Span<'static>> {
    let filled = ((fraction.clamp(0.0, 1.0)) * width as f64).round() as usize;
    vec![
        "━".repeat(filled).fg(ACCENT),
        "━".repeat(width - filled).fg(FAINT),
    ]
}

fn highlight_line(line: Line<'static>, width: usize) -> Line<'static> {
    let used = line.width();
    let mut line = line;
    if used < width + 2 {
        line.spans.push(" ".repeat(width + 2 - used).into());
    }
    line.style(Style::new().bg(SELECTED_BG))
}

fn tick(status: &Status) -> Span<'static> {
    match status {
        Status::Queued => "◷".fg(FAINT),
        Status::Waiting(_) => "◷ waiting".fg(FAINT),
        Status::Retrying(n) => format!("↻ retry {n}").fg(WARN),
        Status::Sent => "✓".fg(MUTED),
        Status::Delivered { hops, ms } if *hops > 1 => format!("✓✓ {hops} hops").fg(OK),
        Status::Delivered { ms, .. } if *ms > 0 => format!("✓✓ {ms}ms").fg(OK),
        Status::Delivered { .. } => "✓✓".fg(OK),
        Status::Read => "✓✓ read".fg(READ),
        Status::Failed(reason) => format!("✗ {}", clip(reason, 30)).fg(ERR),
    }
}

/// A one-line description of an entry, for previews and quotes.
pub fn entry_summary(entry: &Entry) -> String {
    match &entry.body {
        Body::Text(text) => text.clone(),
        Body::File { name, .. } => format!("{} {name}", file_icon(name)),
        Body::Notice(text) => text.clone(),
    }
}

fn file_icon(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "bmp" | "svg" => "🖼",
        "mp4" | "mov" | "mkv" | "webm" | "avi" => "🎬",
        "mp3" | "m4a" | "wav" | "flac" | "ogg" | "opus" | "aac" => "🎵",
        "zip" | "tar" | "gz" | "tgz" | "xz" | "7z" | "rar" | "zst" => "📦",
        "pdf" => "📕",
        "rs" | "py" | "js" | "ts" | "go" | "c" | "h" | "cpp" | "java" | "sh" | "toml" | "json"
        | "yaml" | "yml" => "📜",
        _ => "📄",
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

fn local(ms: i64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ms).single().unwrap_or_else(Local::now)
}

fn clock(ms: i64) -> String {
    local(ms).format("%H:%M").to_string()
}

fn same_day(a: i64, b: i64) -> bool {
    local(a).date_naive() == local(b).date_naive()
}

fn day_separator(ms: i64, width: usize) -> Line<'static> {
    let day = local(ms).date_naive();
    let today = Local::now().date_naive();
    let label = if day == today {
        "Today".to_string()
    } else if today.pred_opt() == Some(day) {
        "Yesterday".to_string()
    } else if day.year() == today.year() {
        local(ms).format("%a %-d %b").to_string()
    } else {
        local(ms).format("%-d %b %Y").to_string()
    };
    let label = format!(" {label} ");
    let label_width = label.width();
    let side = width.saturating_sub(label_width) / 2;
    Line::from(vec![
        "─".repeat(side).fg(FAINT),
        label.fg(MUTED),
        "─".repeat(width.saturating_sub(side + label_width)).fg(FAINT),
    ])
}

/// Soft-wraps on spaces where it can, hard-wraps words wider than a row.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        let mut row_width = 0;
        for word in paragraph.split_inclusive(' ') {
            let w = word.width();
            if row_width + w.saturating_sub(word.ends_with(' ') as usize) > width && !row.is_empty() {
                rows.push(row.trim_end().to_string());
                row = String::new();
                row_width = 0;
            }
            if w > width {
                for c in word.chars() {
                    let cw = c.width().unwrap_or(0);
                    if row_width + cw > width {
                        rows.push(std::mem::take(&mut row));
                        row_width = 0;
                    }
                    row.push(c);
                    row_width += cw;
                }
            } else {
                row.push_str(word);
                row_width += w;
            }
        }
        rows.push(row.trim_end().to_string());
    }
    rows
}

/// Shortens to `max` columns with an ellipsis.
pub fn clip(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in text.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// `off1qz8…k3d` style: enough to tell addresses apart at a glance.
pub fn short(address: &str) -> String {
    let n = address.chars().count();
    if n <= 12 {
        return address.to_string();
    }
    let head: String = address.chars().take(7).collect();
    let tail: String = address.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

pub fn log_line(at: &str, color: Color, text: String) -> Line<'static> {
    Line::from(vec![
        format!("{at} ").fg(FAINT),
        Span::styled(text, Style::new().fg(color)),
    ])
}

/// One protocol-log line per event worth watching; the rest are noise here.
pub fn describe(event: &Event, nick: impl Fn(&str) -> String) -> Option<Line<'static>> {
    let (color, text) = match event {
        Event::MessageSent { message_id, .. } => (Color::Gray, format!("sent       {}", id8(message_id))),
        Event::MessageReceived {
            sender,
            hop_count,
            transport,
            encrypted,
            ..
        } => (
            THEIRS,
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
        } => (OK, format!("ack        {} {latency_ms}ms", id8(message_id))),
        Event::MessageDeferred { message_id, reason, .. } => {
            (MUTED, format!("deferred   {} {reason}", id8(message_id)))
        }
        Event::MessageRetrying {
            message_id,
            retry_count,
            ..
        } => (WARN, format!("retry #{retry_count}   {}", id8(message_id))),
        Event::MessageFailed { message_id, reason, .. }
        | Event::MessageUndeliverable { message_id, reason, .. } => {
            (ERR, format!("failed     {} {reason}", id8(message_id)))
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
            (ERR, format!("mls failed {} {reason}", nick(peer_id)))
        }
        Event::SecurityWarning { peer_id, reason, .. } => {
            (WARN, format!("warning    {} {reason}", nick(peer_id)))
        }
        _ => return None,
    };
    let at = Local::now().format("%H:%M:%S").to_string();
    Some(log_line(&at, color, text))
}

fn id8(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words() {
        assert_eq!(wrap("hello there world", 11), ["hello there", "world"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(25 * 1024 * 1024), "25 MB");
    }
}
