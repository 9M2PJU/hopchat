//! Todos and reminders: things to do, some of them at a time.
//!
//! A todo is a task with no time; a reminder is one with a time, and maybe
//! a repeat. When a reminder comes due it is said in Me (see [`crate::me`])
//! and, if it doesn't repeat, waits under "Due" until it is checked off or
//! snoozed. Like Me, none of it goes on the network: it is kept sealed on
//! this machine beside the history.
//!
//! Times are written the way people say them, before or after the task:
//! "in 20m stretch", "call mom at 5pm", "tomorrow 9am dentist",
//! "standup every weekday at 9:30". Text with no time in it is a todo.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Timelike, Weekday};
use serde::{Deserialize, Serialize};

/// Done tasks are kept this long, so a slip of the finger can be undone.
const KEEP_DONE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// The longest task kept.
pub const TEXT_MAX: usize = 200;
/// How long `s` puts a reminder off.
pub const SNOOZE_MS: i64 = 10 * 60 * 1000;

/// How a reminder comes round again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Repeat {
    /// Every so many minutes (hours are minutes too).
    Minutes(u32),
    /// Every so many days, at the same time of day.
    Days(u32),
    /// Monday to Friday, at the same time of day.
    Weekdays,
}

impl Repeat {
    /// The first time after `now`, stepping from `due`.
    fn next_after(self, due: DateTime<Local>, now: DateTime<Local>) -> DateTime<Local> {
        let mut at = due;
        // Bounded: a reminder last due years ago still lands in a moment.
        for _ in 0..100_000 {
            if at > now {
                return at;
            }
            at = match self {
                Repeat::Minutes(n) => at + Duration::minutes(n.max(1) as i64),
                Repeat::Days(n) => same_time(at, at.date_naive() + Duration::days(n.max(1) as i64)),
                Repeat::Weekdays => {
                    let mut day = at.date_naive().succ_opt().unwrap_or(at.date_naive());
                    while matches!(day.weekday(), Weekday::Sat | Weekday::Sun) {
                        day = day.succ_opt().unwrap_or(day);
                    }
                    same_time(at, day)
                }
            };
        }
        now + Duration::minutes(1)
    }

    pub fn describe(self) -> String {
        match self {
            Repeat::Minutes(n) if n % (24 * 60) == 0 => Repeat::Days(n / (24 * 60)).describe(),
            Repeat::Minutes(60) => "hourly".into(),
            Repeat::Minutes(n) if n % 60 == 0 => format!("every {}h", n / 60),
            Repeat::Minutes(n) => format!("every {n}m"),
            Repeat::Days(1) => "daily".into(),
            Repeat::Days(7) => "weekly".into(),
            Repeat::Days(n) if n % 7 == 0 => format!("every {} weeks", n / 7),
            Repeat::Days(n) => format!("every {n} days"),
            Repeat::Weekdays => "weekdays".into(),
        }
    }
}

/// The conversation a task was made in, to go back to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    pub address: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub text: String,
    pub created_ms: i64,
    #[serde(default)]
    pub due_ms: Option<i64>,
    #[serde(default)]
    pub repeat: Option<Repeat>,
    /// A one-off reminder that has gone off, and waits to be checked off.
    #[serde(default)]
    pub fired: bool,
    #[serde(default)]
    pub done_ms: Option<i64>,
    #[serde(default)]
    pub origin: Option<Origin>,
}

impl Task {
    pub fn done(&self) -> bool {
        self.done_ms.is_some()
    }

    pub fn reminder(&self) -> bool {
        self.due_ms.is_some()
    }

    /// Went off and still waits for you.
    pub fn due(&self) -> bool {
        self.fired && !self.done()
    }
}

/// Where a task sits in the list, top to bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Due,
    Upcoming,
    Todo,
    Done,
}

impl Section {
    pub fn of(task: &Task) -> Section {
        match () {
            _ if task.done() => Section::Done,
            _ if task.fired => Section::Due,
            _ if task.reminder() => Section::Upcoming,
            _ => Section::Todo,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Section::Due => "Due",
            Section::Upcoming => "Reminders",
            Section::Todo => "Todo",
            Section::Done => "Done",
        }
    }
}

/// Text read as a task: what to do, and when.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub text: String,
    pub due: Option<DateTime<Local>>,
    pub repeat: Option<Repeat>,
}

impl Draft {
    /// Reads `input`, finding a time at its start or end. A time it can't
    /// read stays part of the text, so "meet at the cafe" is a todo.
    pub fn read(input: &str, now: DateTime<Local>) -> Draft {
        let input: String = input.split_whitespace().collect::<Vec<_>>().join(" ");
        let words: Vec<&str> = input.split(' ').filter(|w| !w.is_empty()).collect();
        let todo = || Draft {
            text: clean(&words),
            due: None,
            repeat: None,
        };
        if words.is_empty() {
            return todo();
        }
        // "remind me to …" says nothing about the task.
        let lead = match words.first().map(|w| w.to_lowercase()) {
            Some(w) if w == "remind" && words.get(1).is_some_and(|w| w.eq_ignore_ascii_case("me")) => 2,
            Some(w) if w == "me" => 1,
            _ => 0,
        };
        let words = &words[lead..];
        // The longest time phrase wins, at the start, then at the end; a
        // task needs a word left over.
        for take in (1..words.len()).rev() {
            if let Some((due, repeat)) = when(&words[..take], now) {
                let text = strip_joiners(&words[take..]);
                if !text.is_empty() {
                    return Draft { text: clean(text), due: Some(due), repeat };
                }
            }
        }
        for skip in 1..words.len() {
            if let Some((due, repeat)) = when(&words[skip..], now) {
                let text = strip_joiners(&words[..skip]);
                if !text.is_empty() {
                    return Draft { text: clean(text), due: Some(due), repeat };
                }
            }
        }
        Draft { text: clean(words), due: None, repeat: None }
    }
}

fn clean(words: &[&str]) -> String {
    let text = words.join(" ");
    let text = text.trim_end_matches([',', ':', ';', '-']);
    text.chars().filter(|c| !c.is_control()).take(TEXT_MAX).collect()
}

/// Drops the little words between a time and a task: "in 5m *to* stretch",
/// "remind me *to* …". A task left ending in "in" or "at" was split in the
/// wrong place, and comes back empty.
fn strip_joiners<'a, 'b>(words: &'b [&'a str]) -> &'b [&'a str] {
    let lead = |w: &str| matches!(w.to_lowercase().as_str(), "to" | "that" | "about" | "-" | "—" | "," | ":");
    let tail = |w: &str| matches!(w, "-" | "—" | "," | ":");
    let dangling = |w: &str| matches!(w.to_lowercase().as_str(), "in" | "at" | "on" | "by" | "every");
    let mut words = words;
    while words.first().is_some_and(|w| lead(w)) {
        words = &words[1..];
    }
    while words.last().is_some_and(|w| tail(w)) {
        words = &words[..words.len() - 1];
    }
    if words.last().is_some_and(|w| dangling(w)) {
        return &[];
    }
    words
}

/// A whole phrase read as a time: every word must be part of it.
pub fn when(words: &[&str], now: DateTime<Local>) -> Option<(DateTime<Local>, Option<Repeat>)> {
    let words: Vec<String> = words
        .iter()
        .map(|w| w.trim_matches(|c: char| matches!(c, ',' | '.' | '!' | '?' | ';' | ':')).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return None;
    }
    let mut day: Option<NaiveDate> = None;
    let mut weekday: Option<Weekday> = None;
    let mut time: Option<NaiveTime> = None;
    // A clock with no am or pm, "at 5": which half of the day is a guess.
    let mut loose = false;
    // A time of day from "morning", "evening" …, used when no clock is given.
    let mut part: Option<NaiveTime> = None;
    let mut after: Option<Duration> = None;
    let mut repeat: Option<Repeat> = None;
    let today = now.date_naive();
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        match w {
            "in" => {
                let (d, used) = duration(&words[i + 1..])?;
                if after.replace(d).is_some() {
                    return None;
                }
                i += 1 + used;
                continue;
            }
            "at" | "@" => {
                let (t, used, guess) = clock(&words[i + 1..], true)?;
                set_time(&mut time, t)?;
                loose = guess;
                i += 1 + used;
                continue;
            }
            "on" | "this" | "next" if month_day(&words[i + 1..], today.year()).is_some() => {}
            "on" | "this" | "next" => {
                // Only before a day or a part of the day.
                let next = words.get(i + 1)?;
                if weekday_of(next).is_none() && part_of_day(next).is_none() {
                    return None;
                }
            }
            "every" | "each" => {
                let rest = &words[i + 1..];
                let (r, used, on) = repetition(rest)?;
                if repeat.replace(r).is_some() {
                    return None;
                }
                if let Some(on) = on {
                    weekday = Some(on);
                }
                i += 1 + used;
                continue;
            }
            "daily" | "nightly" => {
                repeat.get_or_insert(Repeat::Days(1));
                if w == "nightly" {
                    part.get_or_insert(hm(21, 0));
                }
            }
            "weekly" => {
                repeat.get_or_insert(Repeat::Days(7));
            }
            "hourly" => {
                repeat.get_or_insert(Repeat::Minutes(60));
            }
            "today" => set_day(&mut day, today)?,
            "tonight" => {
                set_day(&mut day, today)?;
                part = Some(hm(20, 0));
            }
            "tomorrow" | "tmrw" | "tmr" | "tomorow" => set_day(&mut day, today.succ_opt()?)?,
            "noon" | "midday" => set_time(&mut time, hm(12, 0))?,
            "midnight" => set_time(&mut time, hm(0, 0))?,
            _ if weekday_of(w).is_some() => {
                // "sat" and "sun" are words too: alone they need an "on".
                let said = i > 0 && matches!(words[i - 1].as_str(), "on" | "next" | "this" | "every" | "each");
                if matches!(w, "sat" | "sun") && !said && words.len() == 1 {
                    return None;
                }
                if weekday.replace(weekday_of(w)?).is_some() {
                    return None;
                }
            }
            _ if part_of_day(w).is_some() => part = part_of_day(w),
            _ => {
                // A bare clock needs am/pm or a colon to be one; a bare
                // number is just a number ("buy 5 apples").
                if let Some((date, used)) = month_day(&words[i..], today.year()) {
                    let date = if date < today { date.with_year(date.year() + 1)? } else { date };
                    set_day(&mut day, date)?;
                    i += used;
                    continue;
                }
                if let Some((t, used, guess)) = clock(&words[i..], false) {
                    set_time(&mut time, t)?;
                    loose = guess;
                    i += used;
                    continue;
                }
                if let Some((d, used)) = duration(&words[i..])
                    && (words[i].chars().any(|c| c.is_alphabetic())
                        || words.get(i + 1).is_some_and(|u| unit(u).is_some()))
                {
                    if after.replace(d).is_some() {
                        return None;
                    }
                    i += used;
                    continue;
                }
                return None;
            }
        }
        i += 1;
    }

    if let Some(d) = after {
        // "in 20m" is a moment, not a day: it doesn't mix with one.
        if day.is_some() || weekday.is_some() || time.is_some() || part.is_some() {
            return None;
        }
        if d <= Duration::zero() || d > Duration::days(3650) {
            return None;
        }
        return Some((now + d, repeat));
    }
    if day.is_none() && weekday.is_none() && time.is_none() && part.is_none() {
        // "every 2 hours" starts now; "daily" alone starts this time tomorrow.
        return match repeat {
            Some(Repeat::Minutes(n)) => Some((now + Duration::minutes(n as i64), repeat)),
            Some(r) => Some((r.next_after(now, now), repeat)),
            None => None,
        };
    }
    if day.is_some() && weekday.is_some() {
        return None;
    }
    // "today" alone is no time at all.
    if day == Some(today) && time.is_none() && part.is_none() {
        return None;
    }
    let explicit_day = day.is_some() || weekday.is_some();
    let date = match (day, weekday) {
        (Some(d), _) => d,
        (None, Some(wd)) => {
            // The next one: "friday" on a friday is next week's, unless a
            // time later today is given.
            let ahead = (wd.num_days_from_monday() as i64 - today.weekday().num_days_from_monday() as i64)
                .rem_euclid(7);
            let ahead = if ahead == 0 {
                match time.or(part) {
                    Some(t) if t > now.time() => 0,
                    _ => 7,
                }
            } else {
                ahead
            };
            today + Duration::days(ahead)
        }
        _ => today,
    };
    let at = time.or(part).unwrap_or(if explicit_day { hm(9, 0) } else { now.time() });
    let pm = |t: NaiveTime| NaiveTime::from_hms_opt(t.hour() + 12, t.minute(), 0).unwrap_or(t);
    let mut at = match (time, part) {
        // "7 tonight", "8 in the evening": a bare hour after noon.
        (Some(t), Some(p)) if t.hour() < 12 && p.hour() >= 15 && t.hour() != 0 => pm(t),
        _ => at,
    };
    if loose && part.is_none() && (1..12).contains(&at.hour()) {
        if at.hour() <= 6 {
            // Nobody means 3 in the morning by "at 3".
            at = pm(at);
        } else if !explicit_day && repeat.is_none() && local_at(date, at)? <= now && local_at(date, pm(at))? > now {
            // "at 9" said after nine in the morning means tonight.
            at = pm(at);
        }
    }
    let mut due = local_at(date, at)?;
    if !explicit_day && due <= now {
        // A time already gone today means tomorrow's, as people mean it.
        due = local_at(date.succ_opt()?, at)?;
    }
    if repeat == Some(Repeat::Weekdays) {
        while matches!(due.weekday(), Weekday::Sat | Weekday::Sun) {
            due = local_at(due.date_naive().succ_opt()?, at)?;
        }
    }
    Some((due, repeat))
}

fn set_day(day: &mut Option<NaiveDate>, to: NaiveDate) -> Option<()> {
    match day.replace(to) {
        Some(_) => None,
        None => Some(()),
    }
}

fn set_time(time: &mut Option<NaiveTime>, to: NaiveTime) -> Option<()> {
    match time.replace(to) {
        Some(_) => None,
        None => Some(()),
    }
}

fn hm(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap_or_default()
}

/// The date at the time, here; a time skipped by a clock change moves on
/// an hour.
fn local_at(date: NaiveDate, time: NaiveTime) -> Option<DateTime<Local>> {
    let naive = date.and_time(time);
    Local
        .from_local_datetime(&naive)
        .earliest()
        .or_else(|| Local.from_local_datetime(&(naive + Duration::hours(1))).earliest())
}

fn same_time(at: DateTime<Local>, date: NaiveDate) -> DateTime<Local> {
    local_at(date, at.time()).unwrap_or(at + Duration::days(1))
}

fn weekday_of(w: &str) -> Option<Weekday> {
    Some(match w.trim_end_matches('s') {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "weds" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

fn part_of_day(w: &str) -> Option<NaiveTime> {
    Some(match w {
        "morning" => hm(9, 0),
        "afternoon" => hm(15, 0),
        "evening" => hm(18, 0),
        "night" => hm(21, 0),
        _ => return None,
    })
}

/// Minutes in one of `w`.
fn unit(w: &str) -> Option<i64> {
    Some(match w {
        "s" | "sec" | "secs" | "second" | "seconds" => 0,
        "m" | "min" | "mins" | "minute" | "minutes" => 1,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60,
        "d" | "day" | "days" => 24 * 60,
        "w" | "wk" | "wks" | "week" | "weeks" => 7 * 24 * 60,
        _ => return None,
    })
}

/// "20m", "1h30m", "2 hours", "an hour", "1 hour and 15 minutes": the
/// length, and how many words it took.
fn duration(words: &[String]) -> Option<(Duration, usize)> {
    let mut total = Duration::zero();
    let mut i = 0;
    let mut any = false;
    while i < words.len() {
        let w = words[i].as_str();
        if any && (w == "and" || w == "&") {
            if words.get(i + 1).is_some_and(|n| amount_start(n)) {
                i += 1;
                continue;
            }
            break;
        }
        if w == "half" && words.get(i + 1).is_some_and(|n| n == "an") && words.get(i + 2).is_some_and(|n| n == "hour") {
            total += Duration::minutes(30);
            i += 3;
            any = true;
            continue;
        }
        let Some((amount, rest)) = number(w) else { break };
        // "1h30m": the units are inside the word.
        if !rest.is_empty() {
            let Some(d) = packed(amount, rest) else { break };
            total += d;
            i += 1;
            any = true;
            continue;
        }
        let Some(per) = words.get(i + 1).and_then(|u| unit(u)) else { break };
        total += of(amount, per, words[i + 1].starts_with('s'));
        i += 2;
        any = true;
    }
    (any && total > Duration::zero()).then_some((total, i))
}

fn amount_start(w: &str) -> bool {
    w == "a" || w == "an" || w.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// `amount` of a unit `per` minutes long; seconds are `per` 0.
fn of(amount: f64, per: i64, seconds: bool) -> Duration {
    if per == 0 && seconds {
        Duration::seconds(amount.round() as i64)
    } else {
        Duration::seconds((amount * per as f64 * 60.0).round() as i64)
    }
}

/// "a", "an", "2", "1.5", or a number with its units after it ("90s",
/// "1h30m"): the number, and what follows it.
fn number(w: &str) -> Option<(f64, &str)> {
    if w == "a" || w == "an" {
        return Some((1.0, ""));
    }
    let end = w.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(w.len());
    let n: f64 = w[..end].parse().ok()?;
    Some((n, &w[end..]))
}

/// The rest of "1h30m" after its first number.
fn packed(first: f64, rest: &str) -> Option<Duration> {
    let mut total = Duration::zero();
    let mut amount = first;
    let mut rest = rest;
    loop {
        let end = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let name = &rest[..end];
        let per = unit(name)?;
        total += of(amount, per, name.starts_with('s'));
        rest = &rest[end..];
        if rest.is_empty() {
            return Some(total);
        }
        let (n, tail) = number(rest)?;
        if tail.is_empty() {
            return None;
        }
        amount = n;
        rest = tail;
    }
}

/// "5pm", "5:30", "17:30", "5 pm", and after "at" a bare "5": the time,
/// how many words it took, and whether it's a guess which half of the day.
fn clock(words: &[String], after_at: bool) -> Option<(NaiveTime, usize, bool)> {
    let w = words.first()?.as_str();
    if let Some(t) = match w {
        "noon" | "midday" => Some(hm(12, 0)),
        "midnight" => Some(hm(0, 0)),
        _ => None,
    } {
        return Some((t, 1, false));
    }
    let (body, mut meridiem) = match () {
        _ if w.ends_with("am") || w.ends_with("pm") => (&w[..w.len() - 2], Some(&w[w.len() - 2..])),
        _ if w.ends_with('a') || w.ends_with('p') => (&w[..w.len() - 1], Some(&w[w.len() - 1..])),
        _ => (w, None),
    };
    let mut used = 1;
    if meridiem.is_none()
        && let Some(next) = words.get(1).map(|n| n.as_str())
        && matches!(next, "am" | "pm" | "a.m" | "p.m" | "a.m." | "p.m.")
    {
        meridiem = Some(&next[..1]);
        used = 2;
    }
    let (h, m) = match body.split_once([':', '.']) {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok().filter(|_| m.len() == 2)?),
        None if body.len() == 4 && meridiem.is_none() && after_at => {
            (body[..2].parse::<u32>().ok()?, body[2..].parse::<u32>().ok()?)
        }
        None if !body.is_empty() && body.len() <= 2 => (body.parse::<u32>().ok()?, 0),
        None => return None,
    };
    let colon = body.contains([':', '.']);
    if meridiem.is_none() && !colon && !after_at {
        return None;
    }
    if m > 59 {
        return None;
    }
    let guess = meridiem.is_none() && (1..=12).contains(&h);
    let h = match meridiem.map(|s| s.starts_with('p')) {
        Some(_) if !(1..=12).contains(&h) => return None,
        Some(true) => h % 12 + 12,
        Some(false) => h % 12,
        None if h > 23 => return None,
        None => h,
    };
    Some((NaiveTime::from_hms_opt(h, m, 0)?, used, guess))
}

fn month_of(w: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "january", "february", "march", "april", "may", "june", "july", "august", "september", "october",
        "november", "december",
    ];
    let w = w.trim_end_matches('.');
    if w.len() < 3 {
        return None;
    }
    // "may" is a word too, so only in full; "sept" is common.
    let at = MONTHS
        .iter()
        .position(|m| *m == w || (w.len() >= 3 && w != "may" && m.starts_with(w) && (w.len() == 3 || w == "sept")))?;
    Some(at as u32 + 1)
}

/// "3", "3rd", "21st": a day of the month.
fn day_number(w: &str) -> Option<u32> {
    let w = ["st", "nd", "rd", "th"].iter().find_map(|s| w.strip_suffix(s)).unwrap_or(w);
    w.parse::<u32>().ok().filter(|d| (1..=31).contains(d))
}

/// "nov 3", "3 november", "3rd of nov": this year's, and the words taken.
fn month_day(words: &[String], year: i32) -> Option<(NaiveDate, usize)> {
    let first = words.first()?;
    if let Some(month) = month_of(first) {
        let d = day_number(words.get(1)?)?;
        return Some((NaiveDate::from_ymd_opt(year, month, d)?, 2));
    }
    let d = day_number(first)?;
    let (month, used) = match words.get(1).map(String::as_str) {
        Some("of") => (month_of(words.get(2)?)?, 3),
        Some(m) => (month_of(m)?, 2),
        None => return None,
    };
    Some((NaiveDate::from_ymd_opt(year, month, d)?, used))
}

/// What follows "every": "day", "2 hours", "monday", "weekday": the
/// repeat, the words taken, and the weekday it falls on if it names one.
fn repetition(words: &[String]) -> Option<(Repeat, usize, Option<Weekday>)> {
    let w = words.first()?.as_str();
    match w {
        "day" | "morning" | "evening" | "night" | "afternoon" => return Some((Repeat::Days(1), usize::from(w == "day"), None)),
        "week" => return Some((Repeat::Days(7), 1, None)),
        "hour" => return Some((Repeat::Minutes(60), 1, None)),
        "minute" => return Some((Repeat::Minutes(1), 1, None)),
        "weekday" | "weekdays" => return Some((Repeat::Weekdays, 1, None)),
        _ => {}
    }
    if let Some(wd) = weekday_of(w) {
        return Some((Repeat::Days(7), 1, Some(wd)));
    }
    let (d, used) = duration(words)?;
    let minutes = d.num_minutes();
    if minutes < 1 {
        return None;
    }
    let repeat = if minutes % (24 * 60) == 0 {
        Repeat::Days((minutes / (24 * 60)) as u32)
    } else {
        Repeat::Minutes(minutes as u32)
    };
    Some((repeat, used, None))
}

/// "in 5m", "in 2h 10m", "in 3 days": how long until `ms`, roughly.
pub fn until(ms: i64, now_ms: i64) -> String {
    let left = ms - now_ms;
    if left <= 0 {
        let ago = (now_ms - ms) / 60_000;
        return match ago {
            0 => "now".into(),
            m if m < 60 => format!("{m}m ago"),
            m if m < 24 * 60 => format!("{}h ago", m / 60),
            m => format!("{}d ago", m / (24 * 60)),
        };
    }
    if left < 60_000 {
        return format!("in {}s", (left + 999) / 1000);
    }
    let minutes = (left + 59_999) / 60_000;
    match minutes {
        m if m < 60 => format!("in {m}m"),
        m if m < 24 * 60 && m % 60 == 0 => format!("in {}h", m / 60),
        m if m < 24 * 60 => format!("in {}h {}m", m / 60, m % 60),
        m if m < 7 * 24 * 60 && (m / 60) % 24 > 0 => format!("in {}d {}h", m / (24 * 60), (m / 60) % 24),
        m => format!("in {}d", m / (24 * 60)),
    }
}

/// "17:30", "tomorrow 09:00", "Fri 09:00", "3 Nov 09:00": when, readably.
pub fn when_label(ms: i64, now: DateTime<Local>) -> String {
    let Some(at) = Local.timestamp_millis_opt(ms).single() else {
        return String::new();
    };
    let day = at.date_naive();
    let today = now.date_naive();
    let time = at.format("%H:%M");
    match (day - today).num_days() {
        0 => format!("today {time}"),
        1 => format!("tomorrow {time}"),
        -1 => format!("yesterday {time}"),
        2..=6 => format!("{} {time}", at.format("%a")),
        _ if day.year() == today.year() => format!("{} {time}", at.format("%-d %b")),
        _ => format!("{} {time}", at.format("%-d %b %Y")),
    }
}

/// Every task, in the order they were made.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Tasks {
    pub items: Vec<Task>,
    #[serde(skip)]
    pub dirty: bool,
}

/// A reminder that just went off.
#[derive(Clone, Debug, PartialEq)]
pub struct Fired {
    pub id: String,
    pub text: String,
    pub origin: Option<Origin>,
    pub due_ms: i64,
}

impl Tasks {
    pub fn add(&mut self, draft: Draft, origin: Option<Origin>, now_ms: i64) -> &Task {
        self.items.push(Task {
            id: new_id(),
            text: draft.text,
            created_ms: now_ms,
            due_ms: draft.due.map(|d| d.timestamp_millis()),
            repeat: draft.repeat,
            fired: false,
            done_ms: None,
            origin,
        });
        self.dirty = true;
        self.items.last().unwrap()
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.items.iter().find(|t| t.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.dirty = true;
        self.items.iter_mut().find(|t| t.id == id)
    }

    /// Rewrites a task from new text, keeping where it came from.
    pub fn edit(&mut self, id: &str, draft: Draft) {
        if let Some(task) = self.get_mut(id) {
            task.text = draft.text;
            task.due_ms = draft.due.map(|d| d.timestamp_millis());
            task.repeat = draft.repeat;
            task.fired = false;
            task.done_ms = None;
        }
    }

    /// Checks a task off, or back on.
    pub fn toggle(&mut self, id: &str, now_ms: i64) -> Option<bool> {
        let task = self.get_mut(id)?;
        if task.done_ms.take().is_none() {
            task.done_ms = Some(now_ms);
        } else if task.due_ms.is_some_and(|d| d > now_ms) {
            task.fired = false;
        }
        Some(task.done())
    }

    /// Puts a reminder off for a while from now; a todo becomes one.
    pub fn snooze(&mut self, id: &str, now_ms: i64) -> Option<i64> {
        let task = self.get_mut(id)?;
        let at = now_ms + SNOOZE_MS;
        task.due_ms = Some(at);
        task.fired = false;
        task.done_ms = None;
        // A snoozed repeat comes back this once, then on its old beat:
        // snoozing it again moves only this time.
        Some(at)
    }

    pub fn remove(&mut self, id: &str) -> Option<Task> {
        let at = self.items.iter().position(|t| t.id == id)?;
        self.dirty = true;
        Some(self.items.remove(at))
    }

    /// Puts back a task that was removed, where it was.
    pub fn restore(&mut self, task: Task) {
        self.dirty = true;
        self.items.push(task);
    }

    /// Reminders whose time has come. One-offs wait under Due; repeats move
    /// on to their next time.
    pub fn fire(&mut self, now: DateTime<Local>) -> Vec<Fired> {
        let now_ms = now.timestamp_millis();
        let mut out = Vec::new();
        for task in &mut self.items {
            let Some(due) = task.due_ms else { continue };
            if task.done() || task.fired || due > now_ms {
                continue;
            }
            out.push(Fired {
                id: task.id.clone(),
                text: task.text.clone(),
                origin: task.origin.clone(),
                due_ms: due,
            });
            match (task.repeat, Local.timestamp_millis_opt(due).single()) {
                (Some(repeat), Some(at)) => task.due_ms = Some(repeat.next_after(at, now).timestamp_millis()),
                _ => task.fired = true,
            }
        }
        if !out.is_empty() {
            self.dirty = true;
        }
        out
    }

    /// When the next reminder goes off, if any.
    pub fn next(&self) -> Option<&Task> {
        self.items
            .iter()
            .filter(|t| !t.done() && !t.fired && t.due_ms.is_some())
            .min_by_key(|t| t.due_ms)
    }

    pub fn due_count(&self) -> usize {
        self.items.iter().filter(|t| t.due()).count()
    }

    pub fn open_count(&self) -> usize {
        self.items.iter().filter(|t| !t.done()).count()
    }

    /// The list as shown: due first, then reminders soonest first, todos
    /// oldest first, and what's done, latest first.
    pub fn ordered(&self) -> Vec<&Task> {
        let mut list: Vec<&Task> = self.items.iter().collect();
        list.sort_by(|a, b| {
            let (sa, sb) = (Section::of(a), Section::of(b));
            sa.cmp(&sb).then_with(|| match sa {
                Section::Due | Section::Upcoming => a.due_ms.cmp(&b.due_ms),
                Section::Todo => a.created_ms.cmp(&b.created_ms),
                Section::Done => b.done_ms.cmp(&a.done_ms),
            })
        });
        list
    }

    /// Forgets what was done long ago.
    pub fn prune(&mut self, now_ms: i64) {
        let before = self.items.len();
        self.items.retain(|t| t.done_ms.is_none_or(|d| now_ms - d < KEEP_DONE_MS));
        if self.items.len() != before {
            self.dirty = true;
        }
    }

    pub fn clear_done(&mut self) -> usize {
        let before = self.items.len();
        self.items.retain(|t| !t.done());
        let gone = before - self.items.len();
        if gone > 0 {
            self.dirty = true;
        }
        gone
    }
}

fn new_id() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("t_{hex}")
}

/// The text that would make `task` again, for editing it.
pub fn as_text(task: &Task, now: DateTime<Local>) -> String {
    let Some(due) = task.due_ms.and_then(|ms| Local.timestamp_millis_opt(ms).single()) else {
        return task.text.clone();
    };
    let time = if due.minute() == 0 {
        due.format("%-I%P").to_string()
    } else {
        due.format("%-I:%M%P").to_string()
    };
    let day = match (due.date_naive() - now.date_naive()).num_days() {
        0 => String::new(),
        1 => "tomorrow ".into(),
        2..=6 => format!("{} ", due.format("%A").to_string().to_lowercase()),
        _ => String::new(),
    };
    match task.repeat {
        Some(Repeat::Days(1)) => format!("{} every day at {time}", task.text),
        Some(Repeat::Days(7)) => format!("{} every {} at {time}", task.text, due.format("%A").to_string().to_lowercase()),
        Some(Repeat::Weekdays) => format!("{} every weekday at {time}", task.text),
        Some(Repeat::Minutes(n)) if n % 60 == 0 => format!("{} every {} hours", task.text, n / 60),
        Some(Repeat::Minutes(n)) => format!("{} every {n} minutes", task.text),
        Some(Repeat::Days(n)) => format!("{} every {n} days", task.text),
        None if due <= now => task.text.clone(),
        None if (due.date_naive() - now.date_naive()).num_days() > 6 => {
            format!("{} on {} at {time}", task.text, due.format("%-d %b").to_string().to_lowercase())
        }
        None => format!("{} {day}at {time}", task.text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wednesday 21 October 2026, 14:00 local.
    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 21, 14, 0, 0).unwrap()
    }

    fn at(d: u32, h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, d, h, m, 0).unwrap()
    }

    fn read(s: &str) -> Draft {
        Draft::read(s, now())
    }

    #[test]
    fn plain_text_is_a_todo() {
        for s in ["buy milk", "buy 5 apples", "meet at the cafe", "on the way", "call mom in a bit", "read chapter 10"] {
            let d = read(s);
            assert_eq!((d.text.as_str(), d.due), (s, None), "{s}");
        }
    }

    #[test]
    fn times_before_or_after_the_task() {
        let cases = [
            ("in 20m stretch", "stretch", now() + Duration::minutes(20)),
            ("20 minutes stretch", "stretch", now() + Duration::minutes(20)),
            ("stretch in 1h30m", "stretch", now() + Duration::minutes(90)),
            ("stretch in an hour", "stretch", now() + Duration::hours(1)),
            ("in 1 hour and 15 minutes to stretch", "stretch", now() + Duration::minutes(75)),
            ("call mom at 5pm", "call mom", at(21, 17, 0)),
            ("call mom at 5", "call mom", at(21, 17, 0)),
            ("call mom at 9", "call mom", at(21, 21, 0)),
            ("dentist on nov 3 at 4pm", "dentist", Local.with_ymd_and_hms(2026, 11, 3, 16, 0, 0).unwrap()),
            ("3rd of november, dentist", "dentist", Local.with_ymd_and_hms(2026, 11, 3, 9, 0, 0).unwrap()),
            ("call mom 5:30pm", "call mom", at(21, 17, 30)),
            ("at 17:30 call mom", "call mom", at(21, 17, 30)),
            ("call mom at noon", "call mom", at(22, 12, 0)),
            ("tomorrow 9am dentist", "dentist", at(22, 9, 0)),
            ("dentist tomorrow at 9", "dentist", at(22, 9, 0)),
            ("dentist tomorrow", "dentist", at(22, 9, 0)),
            ("tomorrow morning: dentist", "dentist", at(22, 9, 0)),
            ("bins tonight", "bins", at(21, 20, 0)),
            ("bins at 7 tonight", "bins", at(21, 19, 0)),
            ("pay rent on friday", "pay rent", at(23, 9, 0)),
            ("pay rent next fri 6pm", "pay rent", at(23, 18, 0)),
            ("wed 3pm review", "review", at(21, 15, 0)),
            ("wed 1pm review", "review", at(28, 13, 0)),
            ("remind me to water plants this evening", "water plants", at(21, 18, 0)),
            ("me to call bob in 5 min", "call bob", now() + Duration::minutes(5)),
        ];
        for (input, text, due) in cases {
            let d = read(input);
            assert_eq!(d.text, text, "{input}");
            assert_eq!(d.due, Some(due), "{input}");
            assert_eq!(d.repeat, None, "{input}");
        }
    }

    #[test]
    fn repeats() {
        let cases = [
            ("standup every weekday at 9:30", "standup", at(22, 9, 30), Repeat::Weekdays),
            ("every day at 8am vitamins", "vitamins", at(22, 8, 0), Repeat::Days(1)),
            ("vitamins daily at 8", "vitamins", at(22, 8, 0), Repeat::Days(1)),
            ("drink water every 2 hours", "drink water", now() + Duration::hours(2), Repeat::Minutes(120)),
            ("bins every monday at 7pm", "bins", at(26, 19, 0), Repeat::Days(7)),
            ("stretch every 45m", "stretch", now() + Duration::minutes(45), Repeat::Minutes(45)),
        ];
        for (input, text, due, repeat) in cases {
            let d = read(input);
            assert_eq!((d.text.as_str(), d.due, d.repeat), (text, Some(due), Some(repeat)), "{input}");
        }
    }

    #[test]
    fn nonsense_times_stay_text() {
        for s in ["at 25pm call", "in 0m call", "in soon call", "call at 13pm", "call tomorrow today", "the cat sat", "may i ask"] {
            assert_eq!(read(s).due, None, "{s}");
        }
        // A time with nothing to do is not a task.
        assert_eq!(read("in 5m").text, "in 5m");
        assert_eq!(read("in 5m").due, None);
    }

    #[test]
    fn reminders_fire_once_and_repeats_move_on() {
        let mut tasks = Tasks::default();
        tasks.add(read("stretch in 10m"), None, 0);
        tasks.add(read("water every day at 2:05pm"), None, 0);
        tasks.add(read("buy milk"), None, 0);
        assert!(tasks.fire(now()).is_empty());
        assert_eq!(tasks.next().unwrap().text, "water");

        let later = now() + Duration::minutes(11);
        let mut fired: Vec<String> = tasks.fire(later).into_iter().map(|f| f.text).collect();
        fired.sort();
        assert_eq!(fired, ["stretch", "water"]);
        assert!(tasks.fire(later).is_empty(), "each goes off once");
        assert_eq!(tasks.due_count(), 1, "the one-off waits; the repeat moved on");
        let water = tasks.items.iter().find(|t| t.text == "water").unwrap();
        assert_eq!(water.due_ms, Some(at(22, 14, 5).timestamp_millis()));

        let stretch = tasks.items.iter().find(|t| t.text == "stretch").unwrap().id.clone();
        let snoozed = tasks.snooze(&stretch, later.timestamp_millis()).unwrap();
        assert_eq!(tasks.due_count(), 0);
        assert!(tasks.fire(later + Duration::minutes(9)).is_empty());
        let again = tasks.fire(Local.timestamp_millis_opt(snoozed).unwrap());
        assert_eq!(again.len(), 1);
        assert_eq!(tasks.toggle(&stretch, 0), Some(true));
        assert_eq!(tasks.due_count(), 0);
        let order: Vec<&str> = tasks.ordered().iter().map(|t| t.text.as_str()).collect();
        assert_eq!(order, ["water", "buy milk", "stretch"]);
    }

    #[test]
    fn missed_repeats_skip_to_the_next_one() {
        let mut tasks = Tasks::default();
        tasks.add(read("every day at 9am vitamins"), None, 0);
        let week_later = now() + Duration::days(7);
        assert_eq!(tasks.fire(week_later).len(), 1, "missed days go off once, not seven times");
        assert_eq!(tasks.items[0].due_ms, Some(at(29, 9, 0).timestamp_millis()));
    }

    #[test]
    fn editing_round_trips_the_text() {
        let mut tasks = Tasks::default();
        for s in ["call mom at 5:30pm", "dentist tomorrow at 9am", "standup every weekday at 9:30am", "buy milk", "dentist on nov 3 at 4pm"] {
            let task = tasks.add(read(s), None, 0).clone();
            let again = read(&as_text(&task, now()));
            assert_eq!((again.text, again.due.map(|d| d.timestamp_millis()), again.repeat), (task.text, task.due_ms, task.repeat), "{s}");
        }
    }

    #[test]
    fn labels_read_naturally() {
        let n = now().timestamp_millis();
        assert_eq!(until(n + 5 * 60_000, n), "in 5m");
        assert_eq!(until(n + 8_000, n), "in 8s");
        assert_eq!(until(n + 90 * 60_000, n), "in 1h 30m");
        assert_eq!(until(n + 2 * 60 * 60_000, n), "in 2h");
        assert_eq!(until(n - 3 * 60_000, n), "3m ago");
        assert_eq!(until(n + 41 * 60 * 60_000, n), "in 1d 17h");
        assert_eq!(until(n + 10 * 24 * 60 * 60_000, n), "in 10d");
        assert_eq!(when_label(at(21, 17, 0).timestamp_millis(), now()), "today 17:00");
        assert_eq!(when_label(at(22, 9, 0).timestamp_millis(), now()), "tomorrow 09:00");
        assert_eq!(when_label(at(24, 9, 0).timestamp_millis(), now()), "Sat 09:00");
    }
}
