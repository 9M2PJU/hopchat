//! The look: neon on midnight, and the handful of effects (gradients,
//! glitches, the radar) that make hopchat feel like a terminal from a
//! rainy future with no internet left.

use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};

pub const BG: Color = Color::Rgb(9, 7, 18);
pub const PANEL: Color = Color::Rgb(16, 12, 30);
pub const RAISED: Color = Color::Rgb(38, 24, 66);
pub const SHADOW: Color = Color::Rgb(3, 2, 7);
pub const TEXT: Color = Color::Rgb(222, 220, 240);

pub const MAGENTA: Color = Color::Rgb(255, 42, 136);
pub const CYAN: Color = Color::Rgb(0, 229, 255);
pub const YELLOW: Color = Color::Rgb(250, 236, 30);
pub const LIME: Color = Color::Rgb(60, 255, 140);
pub const VIOLET: Color = Color::Rgb(160, 90, 255);
pub const ORANGE: Color = Color::Rgb(255, 145, 40);
pub const RED: Color = Color::Rgb(255, 58, 88);
pub const MUTED: Color = Color::Rgb(135, 125, 175);
pub const FAINT: Color = Color::Rgb(72, 62, 108);

/// Everyone but you gets one of these, the same one everywhere.
const PEOPLE: [Color; 7] = [
    CYAN,
    LIME,
    YELLOW,
    ORANGE,
    VIOLET,
    Color::Rgb(90, 160, 255),
    Color::Rgb(0, 255, 200),
];

/// Milliseconds since the epoch: every animation runs off the wall clock.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A person's colour, picked from their address so it never changes.
pub fn person(address: &str) -> Color {
    let hash = address
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    PEOPLE[(hash % PEOPLE.len() as u64) as usize]
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
    mix(c, BG, 0.6)
}

/// The colour `t` of the way along evenly spaced stops.
pub fn along(stops: &[Color], t: f64) -> Color {
    match stops {
        [] => TEXT,
        [only] => *only,
        _ => {
            let scaled = t.clamp(0.0, 1.0) * (stops.len() - 1) as f64;
            let i = (scaled.floor() as usize).min(stops.len() - 2);
            mix(stops[i], stops[i + 1], scaled - i as f64)
        }
    }
}

/// Each character of `text` coloured along the stops.
pub fn gradient(text: &str, stops: &[Color]) -> Vec<Span<'static>> {
    let n = text.chars().count().max(2) - 1;
    text.chars()
        .enumerate()
        .map(|(i, c)| Span::styled(c.to_string(), Style::new().fg(along(stops, i as f64 / n as f64))))
        .collect()
}

/// The neon line under headers: magenta through violet to cyan.
pub fn rule(width: usize) -> Line<'static> {
    Line::from(gradient(&"━".repeat(width), &[MAGENTA, VIOLET, CYAN]))
}

/// 0 → 1 → 0 once every `period_ms`, smoothly: for things that breathe.
pub fn pulse(period_ms: i64) -> f64 {
    let phase = (now_ms().rem_euclid(period_ms)) as f64 / period_ms as f64;
    0.5 - 0.5 * (phase * std::f64::consts::TAU).cos()
}

/// Whether a blinking thing is lit right now.
pub fn blink(period_ms: i64) -> bool {
    now_ms().rem_euclid(period_ms) < period_ms / 2
}

pub fn noise(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

const NOISE: &[char] = &['▓', '▒', '░', '#', '%', '&', '$', '¥', '§', '∆', '¤', '/'];

/// `text`, with a character or two torn up for a few frames every few
/// seconds: a signal that isn't quite clean.
pub fn glitch(text: &str, at_ms: i64) -> String {
    let window = at_ms.div_euclid(4_200);
    if at_ms.rem_euclid(4_200) > 140 {
        return text.to_string();
    }
    let frame = noise(window as u64 ^ (at_ms / 45) as u64);
    let n = text.chars().count().max(1) as u64;
    let a = frame % n;
    let b = (frame >> 16) % n;
    text.chars()
        .enumerate()
        .map(|(i, c)| match i as u64 {
            i if (i == a || i == b) && c != ' ' => NOISE[((frame >> 32) as usize + i as usize) % NOISE.len()],
            _ => c,
        })
        .collect()
}

/// Four bars of signal for how many people are reachable.
pub fn signal(people: usize) -> Vec<Span<'static>> {
    let lit = people.min(4);
    ["▂", "▄", "▆", "█"]
        .iter()
        .enumerate()
        .map(|(i, bar)| bar.fg(if i < lit { along(&[LIME, CYAN], i as f64 / 3.0) } else { FAINT }))
        .collect()
}

/// A sonar sweep: rings leaving the middle, for while hopchat listens for
/// people on the network. `width` and `height` in cells.
pub fn radar(width: usize, height: usize) -> Vec<Line<'static>> {
    let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
    let reach = cy.min(cx / 2.0);
    let t = now_ms().rem_euclid(2_400) as f64 / 2_400.0;
    let rings = [t, (t + 1.0 / 3.0) % 1.0, (t + 2.0 / 3.0) % 1.0];
    // The sweep arm turns once every four seconds.
    let angle = now_ms().rem_euclid(4_000) as f64 / 4_000.0 * std::f64::consts::TAU;
    (0..height)
        .map(|y| {
            let spans: Vec<Span> = (0..width)
                .map(|x| {
                    let dx = (x as f64 + 0.5 - cx) / 2.0;
                    let dy = y as f64 + 0.5 - cy;
                    let d = (dx * dx + dy * dy).sqrt();
                    if d < 0.6 {
                        return "◉".fg(MAGENTA).bold();
                    }
                    if d > reach + 0.5 {
                        return " ".into();
                    }
                    let theta = dy.atan2(dx).rem_euclid(std::f64::consts::TAU);
                    let behind = (angle - theta).rem_euclid(std::f64::consts::TAU);
                    if behind < 0.35 && d <= reach {
                        let c = mix(LIME, BG, behind / 0.5);
                        return "•".fg(c);
                    }
                    for r in rings {
                        if (d - r * reach).abs() < 0.45 {
                            return "·".fg(mix(CYAN, BG, r * 0.85));
                        }
                    }
                    if (d - reach).abs() < 0.45 {
                        return "·".fg(FAINT);
                    }
                    " ".into()
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

const H: [&str; 6] = ["██╗  ██╗", "██║  ██║", "███████║", "██╔══██║", "██║  ██║", "╚═╝  ╚═╝"];
const O: [&str; 6] = [" ██████╗ ", "██╔═══██╗", "██║   ██║", "██║   ██║", "╚██████╔╝", " ╚═════╝ "];
const P: [&str; 6] = ["██████╗ ", "██╔══██╗", "██████╔╝", "██╔═══╝ ", "██║     ", "╚═╝     "];
const C: [&str; 6] = [" ██████╗", "██╔════╝", "██║     ", "██║     ", "╚██████╗", " ╚═════╝"];
const A: [&str; 6] = [" █████╗ ", "██╔══██╗", "███████║", "██╔══██║", "██║  ██║", "╚═╝  ╚═╝"];
const T: [&str; 6] = ["████████╗", "╚══██╔══╝", "   ██║   ", "   ██║   ", "   ██║   ", "   ╚═╝   "];

/// HOPCHAT in big letters, six rows.
pub fn logo() -> Vec<String> {
    (0..6)
        .map(|row| [H, O, P, C, H, A, T].iter().map(|letter| letter[row]).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn logo_rows_line_up() {
        let rows = logo();
        assert!(rows.iter().all(|r| r.width() == rows[0].width()), "{rows:#?}");
    }

    #[test]
    fn people_keep_their_colour() {
        assert_eq!(person("off1bob"), person("off1bob"));
        assert_ne!(person("off1bob"), MAGENTA, "magenta is yours");
    }

    #[test]
    fn mixing_lands_between() {
        assert_eq!(mix(Color::Rgb(0, 0, 0), Color::Rgb(200, 100, 50), 0.5), Color::Rgb(100, 50, 25));
        assert_eq!(along(&[MAGENTA, CYAN], 1.0), CYAN);
    }
}
