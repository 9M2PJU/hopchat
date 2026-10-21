//! The look: soft pastels on dusk ink. Colour carries meaning (who said
//! it, what state it is in) and little else; nothing flashes, nothing
//! glows, and the few things that move do so gently.

use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Span};

pub const BG: Color = Color::Rgb(25, 23, 36);
pub const PANEL: Color = Color::Rgb(31, 29, 46);
pub const RAISED: Color = Color::Rgb(44, 41, 64);
pub const SHADOW: Color = Color::Rgb(17, 16, 26);
pub const TEXT: Color = Color::Rgb(224, 222, 244);

/// Yours: your name, your messages, the accent.
pub const IRIS: Color = Color::Rgb(196, 167, 231);
pub const FOAM: Color = Color::Rgb(156, 207, 216);
pub const GOLD: Color = Color::Rgb(246, 193, 119);
pub const SAGE: Color = Color::Rgb(166, 209, 137);
pub const ROSE: Color = Color::Rgb(235, 188, 186);
pub const PEACH: Color = Color::Rgb(239, 159, 118);
pub const RED: Color = Color::Rgb(235, 111, 146);
pub const MUTED: Color = Color::Rgb(144, 140, 170);
pub const FAINT: Color = Color::Rgb(86, 82, 110);

/// Everyone but you gets one of these, the same one everywhere.
const PEOPLE: [Color; 7] = [
    FOAM,
    SAGE,
    GOLD,
    ROSE,
    PEACH,
    Color::Rgb(137, 180, 250),
    Color::Rgb(148, 226, 213),
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

/// `c` faded in from the background, `over_ms` long from `from_ms`, as
/// it is `t` ms in: for things that arrive rather than appear.
pub fn fade_in(c: Color, t: i64, from_ms: i64, over_ms: i64) -> Color {
    mix(BG, c, (t - from_ms) as f64 / over_ms.max(1) as f64)
}

/// The quiet line under headers.
pub fn rule(width: usize) -> Line<'static> {
    Line::from("─".repeat(width).fg(mix(FAINT, BG, 0.35)))
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
                        return "●".fg(IRIS);
                    }
                    for r in rings {
                        if (d - r * reach).abs() < 0.35 {
                            return "·".fg(mix(FOAM, BG, 0.25 + r * 0.75));
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
        assert_ne!(person("off1bob"), IRIS, "iris is yours");
    }

    #[test]
    fn mixing_lands_between() {
        assert_eq!(mix(Color::Rgb(0, 0, 0), Color::Rgb(200, 100, 50), 0.5), Color::Rgb(100, 50, 25));
        assert_eq!(fade_in(IRIS, 500, 0, 400), IRIS);
        assert_eq!(fade_in(IRIS, 0, 100, 400), BG);
    }
}
