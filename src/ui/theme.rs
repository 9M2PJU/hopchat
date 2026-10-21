//! The look: soft pastels on dusk ink. Colour carries meaning (who said
//! it, what state it is in) and little else; nothing flashes, nothing
//! glows, and the few things that move do so gently.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Span};

/// Every colour hopchat draws with. Two of them exist, dusk and dawn,
/// and the same names mean the same things in both.
pub struct Palette {
    pub bg: Color,
    pub panel: Color,
    pub raised: Color,
    pub shadow: Color,
    pub text: Color,
    /// Yours: your name, your messages, the accent.
    pub iris: Color,
    pub foam: Color,
    pub gold: Color,
    pub sage: Color,
    pub rose: Color,
    pub peach: Color,
    pub red: Color,
    pub muted: Color,
    pub faint: Color,
    /// Everyone but you gets one of these, the same one everywhere.
    people: [Color; 7],
}

const DARK: Palette = Palette {
    bg: Color::Rgb(25, 23, 36),
    panel: Color::Rgb(31, 29, 46),
    raised: Color::Rgb(44, 41, 64),
    shadow: Color::Rgb(17, 16, 26),
    text: Color::Rgb(224, 222, 244),
    iris: Color::Rgb(196, 167, 231),
    foam: Color::Rgb(156, 207, 216),
    gold: Color::Rgb(246, 193, 119),
    sage: Color::Rgb(166, 209, 137),
    rose: Color::Rgb(235, 188, 186),
    peach: Color::Rgb(239, 159, 118),
    red: Color::Rgb(235, 111, 146),
    muted: Color::Rgb(144, 140, 170),
    faint: Color::Rgb(86, 82, 110),
    people: [
        Color::Rgb(156, 207, 216),
        Color::Rgb(166, 209, 137),
        Color::Rgb(246, 193, 119),
        Color::Rgb(235, 188, 186),
        Color::Rgb(239, 159, 118),
        Color::Rgb(137, 180, 250),
        Color::Rgb(148, 226, 213),
    ],
};

/// Dusk's pastels deepened until they read on paper.
const LIGHT: Palette = Palette {
    bg: Color::Rgb(250, 246, 240),
    panel: Color::Rgb(243, 237, 229),
    raised: Color::Rgb(230, 222, 212),
    shadow: Color::Rgb(218, 210, 200),
    text: Color::Rgb(70, 66, 98),
    iris: Color::Rgb(124, 96, 166),
    foam: Color::Rgb(46, 124, 138),
    gold: Color::Rgb(176, 112, 20),
    sage: Color::Rgb(76, 128, 62),
    rose: Color::Rgb(184, 92, 98),
    peach: Color::Rgb(194, 98, 46),
    red: Color::Rgb(186, 52, 92),
    muted: Color::Rgb(116, 111, 140),
    faint: Color::Rgb(168, 162, 180),
    people: [
        Color::Rgb(46, 124, 138),
        Color::Rgb(76, 128, 62),
        Color::Rgb(176, 112, 20),
        Color::Rgb(184, 92, 98),
        Color::Rgb(194, 98, 46),
        Color::Rgb(52, 98, 176),
        Color::Rgb(30, 128, 112),
    ],
};

static LIGHT_ON: AtomicBool = AtomicBool::new(false);
/// Where the choice is kept, so the next start remembers it.
static SAVED: OnceLock<PathBuf> = OnceLock::new();

/// The palette in use.
pub fn palette() -> &'static Palette {
    if LIGHT_ON.load(Ordering::Relaxed) { &LIGHT } else { &DARK }
}

pub fn is_light() -> bool {
    LIGHT_ON.load(Ordering::Relaxed)
}

/// Picks the palette at start: `choice` (from --theme) if given, then
/// what was last chosen with /theme, then what the terminal says its
/// background is, and dusk otherwise.
pub fn init(saved: PathBuf, choice: Option<bool>) {
    let light = choice
        .or_else(|| std::fs::read_to_string(&saved).ok().and_then(|s| parse(s.trim())))
        .unwrap_or_else(terminal_is_light);
    LIGHT_ON.store(light, Ordering::Relaxed);
    let _ = SAVED.set(saved);
}

/// "light" or "dark" as `Some(true)` or `Some(false)`.
pub fn parse(name: &str) -> Option<bool> {
    match name {
        "light" | "dawn" => Some(true),
        "dark" | "dusk" => Some(false),
        _ => None,
    }
}

/// Switches to `light` (or dark) and remembers it.
pub fn set(light: bool) -> std::io::Result<()> {
    LIGHT_ON.store(light, Ordering::Relaxed);
    match SAVED.get() {
        Some(path) => std::fs::write(path, if light { "light\n" } else { "dark\n" }),
        None => Ok(()),
    }
}

/// `COLORFGBG` ("15;0"), set by many terminals, ends with the
/// background's colour number; 7 and 15 are the light ones.
fn terminal_is_light() -> bool {
    std::env::var("COLORFGBG")
        .ok()
        .and_then(|v| v.rsplit(';').next().and_then(|bg| bg.parse::<u8>().ok()))
        .is_some_and(|bg| bg == 7 || bg == 15)
}

pub fn bg() -> Color { palette().bg }
pub fn panel() -> Color { palette().panel }
pub fn raised() -> Color { palette().raised }
pub fn shadow() -> Color { palette().shadow }
pub fn text() -> Color { palette().text }
pub fn iris() -> Color { palette().iris }
pub fn foam() -> Color { palette().foam }
pub fn gold() -> Color { palette().gold }
pub fn sage() -> Color { palette().sage }
pub fn rose() -> Color { palette().rose }
pub fn peach() -> Color { palette().peach }
pub fn red() -> Color { palette().red }
pub fn muted() -> Color { palette().muted }
pub fn faint() -> Color { palette().faint }

/// Milliseconds since the epoch: every animation runs off the wall clock.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A person's colour, picked from their address so it never changes.
pub fn person(address: &str) -> Color {
    let hash = address
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    let people = &palette().people;
    people[(hash % people.len() as u64) as usize]
}

/// `a` blended towards `b`; `t` 0 is `a`, 1 is `b`.
pub fn mix(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let t = t.clamp(0.0, 1.0);
            let at = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
            Color::Rgb(at(r1, r2), at(g1, g2), at(b1, b2))
        }
        _ => a,
    }
}

/// A colour sunk most of the way into the background.
pub fn dim(c: Color) -> Color {
    mix(c, bg(), 0.6)
}

/// `c` faded in from the background, `over_ms` long from `from_ms`, as
/// it is `t` ms in: for things that arrive rather than appear.
pub fn fade_in(c: Color, t: i64, from_ms: i64, over_ms: i64) -> Color {
    mix(bg(), c, (t - from_ms) as f64 / over_ms.max(1) as f64)
}

/// Every divider: rules, panel edges, the day separator.
pub fn line() -> Color {
    mix(faint(), bg(), 0.35)
}

/// The quiet line under headers.
pub fn rule(width: usize) -> Line<'static> {
    Line::from("─".repeat(width).fg(line()))
}

/// Ripples leaving the middle, slowly, for while hopchat listens for
/// people on the network. `width` and `height` in cells.
pub fn ripples(width: usize, height: usize) -> Vec<Line<'static>> {
    let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
    let reach = cy.min(cx / 2.0);
    let t = now_ms().rem_euclid(4_800) as f64 / 4_800.0;
    let rings = [t, (t + 0.5) % 1.0];
    (0..height)
        .map(|y| {
            let spans: Vec<Span> = (0..width)
                .map(|x| {
                    let dx = (x as f64 + 0.5 - cx) / 2.0;
                    let dy = y as f64 + 0.5 - cy;
                    let d = (dx * dx + dy * dy).sqrt();
                    if d < 0.4 {
                        return "●".fg(iris());
                    }
                    for r in rings {
                        if (d - r * reach).abs() < 0.35 {
                            return "·".fg(mix(foam(), bg(), 0.25 + r * 0.75));
                        }
                    }
                    " ".into()
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn people_keep_their_colour() {
        assert_eq!(person("off1bob"), person("off1bob"));
        assert_ne!(person("off1bob"), iris(), "iris is yours");
    }

    #[test]
    fn mixing_lands_between() {
        assert_eq!(mix(Color::Rgb(0, 0, 0), Color::Rgb(200, 100, 50), 0.5), Color::Rgb(100, 50, 25));
        assert_eq!(fade_in(iris(), 500, 0, 400), iris());
        assert_eq!(fade_in(iris(), 0, 100, 400), bg());
    }
}
