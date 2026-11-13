//! What the names inside a downloaded payload claim about the event, judged against the stored
//! event and the part that was picked.
//!
//! A payload name is not a release title, so this is a scanner of its own over the words of a
//! fragment. It looks only for positive claims, anywhere in the fragment, and ignores everything
//! it does not recognise. It does not read the league out of the fragment and does not call the
//! release matcher.
//!
//! # What it recognises (and, by design, nothing else)
//!
//! A name in a form outside this list claims nothing and is tolerated: that is a stated residual.
//!
//! - **Dates.** Numeric with a four-digit year in any order and with `.`, `-`, `_`, space or no
//!   separator, the year first, in the middle or last (`2026.01.01`, `01.2026.01`, `01-01-2026`,
//!   `20260101`, `01202601`, `01012026`, `01_02_2026`); numeric with a two-digit year
//!   (`01.01.26`); English month names and three-letter abbreviations as day, month, year or
//!   month, day, year (`1 January 2026`, `Jan.01.2026`; a year before the month's name is not
//!   recognised). A season span (`2026-27`, `2026-2027`) is never a date. Where day and month
//!   are ambiguous
//!   the name contradicts only if NO reading falls within a day of the event (outside the
//!   tournament, for golf). Not dates: a token labelled as a checksum or id (`CRC32.20240101`,
//!   `[ABCD1234]`, a hex run), anything inside a resolution, codec or audio token, a lone year.
//! - **Card numbers and serials** (fight cards): every `<league or series word> <number>` in the
//!   name, not only the first (`UFC.334.UFC.333` contradicts), found in each path component alone
//!   and across a boundary only where a component ends with a league or series word and the next
//!   is a number and nothing else (`UFC/333/video.mkv`). A stored name without a number cannot be
//!   compared, so one number is tolerated there; two different ones are not.
//! - **Pairs.** `vs`, `v`, `versus`, `x` and a spaced hyphen between two name runs, judged side by
//!   side, each side first cleaned of league, series, date, part, round and technical words at
//!   either end. A club that resolves to another team of the league contradicts, even if the
//!   other side is right; a fighter does when the last word of the side is the surname (the last
//!   word of a stored participant's name) of a fighter in another stored event of the league
//!   within a year and not of this event (`Gane.vs.Lopes`, `Oliveira.x.Lopes`). A side that
//!   resolves to nobody, given names and initials included, is tolerated. Absence of shared
//!   words is never a contradiction by itself.
//! - **Parts** (fight cards only; golf has rounds): English part words and common variants
//!   (prelims, prelim, preliminary card, preliminaries, early prelims, early preliminary card,
//!   main card, main event as a single bout, full event, full card, complete) and the Spanish and
//!   Portuguese forms (preliminares, cartelera preliminar, cartelera principal, estelar, card
//!   principal, card preliminar). Any part other than the one picked contradicts, whole event
//!   included in both directions, and two different parts in one name contradict. For golf a round
//!   number must be the round picked. For team sports `Round 12`, `Matchweek 9` and the like are
//!   a competition round, compared with the stored round when there is one and tolerated
//!   otherwise, never a part.
//! - **Non-event markers**: the single shoulder words and the multi-word sequences the release
//!   parser has (`Press Conference`, `Weigh In`, `Post Fight`, ...) over adjacent words across
//!   any separator, non-event words, and season-and-episode patterns (`S01E02`, `S01.E02`,
//!   `S01 E02`, `1x02`, `Episode 2`, `Ep 2`).
//! - **Other competitions** (catalogue aliases that are not part of the event's own name), other
//!   sports' labels (soccer, basketball, golf, mma), and, for golf, a known major that is not the
//!   event's own.
//!
//! Component boundaries are hard: a date, a season or a pair never runs from one path component
//! into the next. Every folder of every entry is judged (a folder holding only text files too);
//! the base name only of a video. Videos are counted over every extension the code knows as
//! video, importable or not. A `Sample/` folder holding a video is a second video and refuses in
//! this cut.

use super::Part;
use super::catalog::{self, Sport};
use super::data::{self, EventRow, TeamRow};
use super::names;
use super::reason::{Claim, Reason};
use super::release;
use anyhow::Result;
use rusqlite::Connection;
use time::{Date, OffsetDateTime};

/// What a payload's names are judged against besides the event itself: the league's known teams
/// and the fighters of the league's other stored events within a year either side.
#[derive(Debug, Default, Clone)]
pub struct Known {
    pub teams: Vec<TeamRow>,
    /// The SURNAMES of each other event's fighters, one list per event: the last word of each
    /// stored participant name (the whole of a one-word name). Given names and middle words never
    /// identify a fighter.
    pub others: Vec<Vec<String>>,
}

/// A word that is not part of a name: a rematch ordinal (digits or `II`, `III`, `IV`, as in
/// "Kasanganay 2") or a generational suffix (`Jr`, `Sr`; phase C's `suffix_of` list). These come
/// after a surname and are taken off before it is read.
fn after_surname(w: &str) -> bool {
    is_num(w, 1, 2) || matches!(w, "ii" | "iii" | "iv" | "jr" | "sr")
}

/// A participant's words with trailing ordinals and suffixes taken off.
fn trimmed(words: &[String]) -> &[String] {
    let mut n = words.len();
    while n > 1 && after_surname(&words[n - 1]) {
        n -= 1;
    }
    &words[..n]
}

/// The surnames of the participants an event's name gives ("UFC Fight Night: Josh Emmett vs.
/// Kevin Vallejos" is `emmett` and `vallejos`): the name after any colon, split at `vs`/`v`; in
/// each part a trailing rematch ordinal ("Kasanganay 2", "Kasanganay II") and generational suffix
/// ("Volkanovski Jr") are taken off and the LAST word is the surname. For a multi-word surname
/// ("dos Santos", "de la Rosa", "Cortes Acosta") that is its last word (`santos`, `rosa`,
/// `acosta`); the picked event's own fighters never contradict by any of their words (see
/// `participant_words`).
pub fn surnames_of(name: &str) -> Vec<String> {
    let after = name.rsplit_once(':').map_or(name, |(_, x)| x);
    let words = names::tokens(after);
    let mut out = vec![];
    for part in words.split(|w| matches!(w.as_str(), "vs" | "v" | "versus")) {
        if let Some(last) = trimmed(part).last()
            && last.len() >= 3
        {
            out.push(last.clone());
        }
    }
    out
}

/// Every word of the participants an event's name gives: the picked event's own fighters pass
/// by any of these.
pub fn participant_words(name: &str) -> Vec<String> {
    let after = name.rsplit_once(':').map_or(name, |(_, x)| x);
    names::tokens(after)
}

/// Loads `Known` for an event, once per import.
pub fn known_for(c: &Connection, ev: &EventRow) -> Result<Known> {
    let teams = data::teams_of(c, &ev.league)?;
    let rfc = &time::format_description::well_known::Rfc3339;
    let mut others = vec![];
    if let Ok(start) = OffsetDateTime::parse(&ev.starts_at, rfc) {
        let (lo, hi) = (
            crate::import::rfc3339(start - time::Duration::days(365)),
            crate::import::rfc3339(start + time::Duration::days(365)),
        );
        let mut stmt = c.prepare(
            "SELECT name FROM sport_events WHERE league = ?1 AND id != ?2 AND starts_at_utc >= ?3 AND starts_at_utc <= ?4 LIMIT 3000",
        )?;
        let rows = stmt.query_map(rusqlite::params![ev.league, ev.id, lo, hi], |r| {
            r.get::<_, String>(0)
        })?;
        for name in rows {
            let w = surnames_of(&name?);
            if !w.is_empty() {
                others.push(w);
            }
        }
    }
    Ok(Known { teams, others })
}

/// A word that can tell people apart: not a stop word, not a number, not a short code.
fn distinctive(x: &str) -> bool {
    x.len() >= 3 && !STOP.contains(&x) && !x.bytes().all(|b| b.is_ascii_digit())
}

/// Words that say nothing about who plays.
const STOP: &[&str] = &[
    "ufc",
    "fight",
    "night",
    "vs",
    "versus",
    "the",
    "of",
    "and",
    "card",
    "main",
    "prelims",
    "early",
    "full",
    "event",
    "pfl",
    "bellator",
    "one",
    "championship",
    "los",
    "las",
    "del",
    "new",
    "city",
    "united",
];

/// The majors the words name, longest phrase first at one place, by the same identities the
/// matcher uses (`matcher::claims::major_of`): "The Open", "Open Championship" and "British Open"
/// are one, "U.S. Open" and "US Open" are one, "Masters" and "Masters Tournament" are one.
fn majors_in(words: &[String]) -> Vec<&'static str> {
    let mut out = vec![];
    let mut i = 0;
    while i < words.len() {
        let mut hit = None;
        for len in (1..=4.min(words.len() - i)).rev() {
            if let Some(m) = super::matcher::claims::major_of(&words[i..i + len]) {
                hit = Some((len, m));
                break;
            }
        }
        match hit {
            Some((len, m)) => {
                out.push(m);
                i += len;
            }
            None => i += 1,
        }
    }
    out
}

fn is_num(s: &str, min: usize, max: usize) -> bool {
    s.len() >= min && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit())
}

/// Removes what is an id or a checksum, not a name: a label (`crc32`, `md5`, `id` ...) with the
/// word after it, and a hex run that mixes letters and digits (`ABCD1234`).
fn without_ids(words: Vec<String>) -> Vec<String> {
    const LABELS: &[&str] = &[
        "crc", "crc32", "md5", "sha1", "sha256", "hash", "id", "imdb", "tmdb", "tvdb", "checksum",
    ];
    let mut out = vec![];
    let mut skip_next = false;
    for w in words {
        if skip_next {
            skip_next = false;
            continue;
        }
        if LABELS.contains(&w.as_str()) {
            skip_next = true;
            continue;
        }
        let hex = w.len() >= 8
            && w.bytes().all(|b| b.is_ascii_hexdigit())
            && w.bytes().any(|b| b.is_ascii_digit())
            && w.bytes().any(|b| b.is_ascii_alphabetic());
        if !hex {
            out.push(w);
        }
    }
    out
}

fn is_episode(w: &[&str]) -> bool {
    let sxe = |s: &str| {
        let Some(rest) = s.strip_prefix('s') else {
            return false;
        };
        let Some((a, b)) = rest.split_once('e') else {
            return false;
        };
        is_num(a, 1, 2) && is_num(b.split('e').next().unwrap_or(""), 1, 3)
    };
    let season = |s: &str| s.strip_prefix('s').is_some_and(|r| is_num(r, 1, 2));
    let episode = |s: &str| s.strip_prefix('e').is_some_and(|r| is_num(r, 1, 3));
    let nxm = |s: &str| {
        s.split_once('x')
            .is_some_and(|(a, b)| is_num(a, 1, 2) && is_num(b, 2, 3))
    };
    w.iter().enumerate().any(|(i, x)| {
        sxe(x)
            || nxm(x)
            || (season(x) && w.get(i + 1).is_some_and(|n| episode(n)))
            || (matches!(*x, "episode" | "ep" | "episodes")
                && w.get(i + 1).is_some_and(|n| is_num(n, 1, 3)))
            || (x.starts_with("ep") && is_num(&x[2..], 1, 3))
    })
}

fn month_num(s: &str) -> Option<u8> {
    Some(match s {
        "jan" | "january" => 1,
        "feb" | "february" => 2,
        "mar" | "march" => 3,
        "apr" | "april" => 4,
        "may" => 5,
        "jun" | "june" => 6,
        "jul" | "july" => 7,
        "aug" | "august" => 8,
        "sep" | "sept" | "september" => 9,
        "oct" | "october" => 10,
        "nov" | "november" => 11,
        "dec" | "december" => 12,
        _ => return None,
    })
}

fn date_of(y: i32, m: u32, d: u32) -> Option<Date> {
    if !(1990..=2099).contains(&y) {
        return None;
    }
    Date::from_calendar_date(
        y,
        time::Month::try_from(u8::try_from(m).ok()?).ok()?,
        u8::try_from(d).ok()?,
    )
    .ok()
}

/// Removes season spans ("2026 27", "2026 2027"), which are never dates.
fn without_seasons(w: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut i = 0;
    while i < w.len() {
        if is_num(w[i], 4, 4)
            && let Ok(y) = w[i].parse::<u32>()
            && let Some(n) = w.get(i + 1)
            && ((is_num(n, 2, 2) && n.parse::<u32>().ok() == Some((y + 1) % 100))
                || (is_num(n, 4, 4) && n.parse::<u32>().ok() == Some(y + 1)))
        {
            i += 2;
            continue;
        }
        out.push(w[i].to_string());
        i += 1;
    }
    out
}

/// Every date the words write, each as the list of its valid readings (ambiguous ones have more
/// than one). Three numbers where one has four digits (the year, first, in the middle or last)
/// and the other two are a day and a month in either order; the same with a two-digit year (and
/// `YY MM DD`); eight digits in one word with the year first, in the middle or last (`YYYYMMDD`,
/// `DDYYYYMM`, `DDMMYYYY` and the swapped day/month forms); and a month's name with a day and a
/// four-digit year in either order. Season spans are removed first.
fn dates_in(w: &[&str]) -> Vec<Vec<Date>> {
    let owned = without_seasons(w);
    let w: Vec<&str> = owned.iter().map(String::as_str).collect();
    let n = |s: &str| s.parse::<u32>().unwrap_or(0);
    let mut out: Vec<Vec<Date>> = vec![];
    for i in 0..w.len() {
        let x = w[i];
        let at = |k: usize| w.get(i + k).copied();
        let mut readings: Vec<Date> = vec![];
        // The year and the other two (a day and a month, either way).
        let mut with_year = |y: u32, p: u32, q: u32| {
            readings.extend(date_of(y as i32, p, q));
            readings.extend(date_of(y as i32, q, p));
        };
        if is_num(x, 8, 8) {
            let seg = |a: usize, b: usize| n(&x[a..b]);
            with_year(seg(0, 4), seg(4, 6), seg(6, 8)); // YYYYMMDD, YYYYDDMM
            with_year(seg(2, 6), seg(0, 2), seg(6, 8)); // DDYYYYMM, MMYYYYDD
            with_year(seg(4, 8), seg(0, 2), seg(2, 4)); // DDMMYYYY, MMDDYYYY
        }
        if let (true, Some(b), Some(c)) = (is_num(x, 1, 4), at(1), at(2))
            && is_num(b, 1, 4)
            && is_num(c, 1, 4)
        {
            let lens = (x.len(), b.len(), c.len());
            match lens {
                (4, 1..=2, 1..=2) => with_year(n(x), n(b), n(c)),
                (1..=2, 4, 1..=2) => with_year(n(b), n(x), n(c)),
                (1..=2, 1..=2, 4) => with_year(n(c), n(x), n(b)),
                (1..=2, 1..=2, 2) => {
                    let y = 2000 + n(c);
                    with_year(y, n(x), n(b));
                    // YY MM DD (the first two digits are the year)
                    if lens.0 == 2 {
                        with_year(2000 + n(x), n(b), n(c));
                    }
                }
                _ => {}
            }
        }
        // Day, month's name, year; month's name, day, year.
        if let (true, Some(m), Some(y)) = (
            is_num(x, 1, 2),
            at(1).and_then(month_num),
            at(2).filter(|y| is_num(y, 4, 4)),
        ) {
            readings.extend(date_of(n(y) as i32, u32::from(m), n(x)));
        }
        if let (Some(m), Some(d), Some(y)) = (
            month_num(x),
            at(1).filter(|d| is_num(d, 1, 2)),
            at(2).filter(|y| is_num(y, 4, 4)),
        ) {
            readings.extend(date_of(n(y) as i32, u32::from(m), n(d)));
        }
        if !readings.is_empty() {
            out.push(readings);
        }
    }
    out
}

/// Every `<league or series word> <number>` in the words.
fn card_numbers(w: &[&str], aliases: &[&str]) -> Vec<u32> {
    let mut leads: Vec<Vec<String>> = aliases.iter().map(|a| crate::title::tokens(a)).collect();
    leads.push(vec!["fight".into(), "night".into()]);
    let mut out = vec![];
    for i in 0..w.len() {
        for lead in &leads {
            let n = lead.len();
            if n == 0 || i + n >= w.len() {
                continue;
            }
            if w[i..i + n].iter().zip(lead).all(|(a, b)| *a == b.as_str()) && is_num(w[i + n], 1, 3)
            {
                out.extend(w[i + n].parse::<u32>().ok());
            }
        }
    }
    out
}

/// A part word or phrase and what it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Say {
    Part(Part),
    Bout,
}

/// The part phrases of a fight card, longest first at one place (English, Spanish, Portuguese).
fn card_parts(w: &[&str]) -> Vec<Say> {
    const PRELIM: &[&str] = &["prelims", "prelim", "preliminaries", "preliminares"];
    let mut out = vec![];
    let mut i = 0;
    while i < w.len() {
        let at = |k: usize| w.get(i + k).copied().unwrap_or("");
        let prelim = |s: &str| PRELIM.contains(&s);
        let prelim_card = |k: usize| at(k) == "preliminary" && at(k + 1) == "card";
        let (len, say) = match w[i] {
            "early" if prelim(at(1)) => (2, Say::Part(Part::EarlyPrelims)),
            "early" if prelim_card(1) => (3, Say::Part(Part::EarlyPrelims)),
            "main" if at(1) == "card" && at(2) == "and" && prelim(at(3)) => {
                (4, Say::Part(Part::Whole))
            }
            "main" if at(1) == "card" && at(2) == "and" && prelim_card(3) => {
                (5, Say::Part(Part::Whole))
            }
            "main" if at(1) == "card" && prelim(at(2)) => (3, Say::Part(Part::Whole)),
            "main" if at(1) == "card" => (2, Say::Part(Part::Main)),
            "main" => (if at(1) == "event" { 2 } else { 1 }, Say::Bout),
            "co" if at(1) == "main" => (2, Say::Bout),
            "comain" | "bout" | "bouts" => (1, Say::Bout),
            "full" if at(1) == "fight" => (2, Say::Bout),
            "fight" if is_num(at(1), 1, 2) => (2, Say::Bout),
            "full" if matches!(at(1), "event" | "card") => (2, Say::Part(Part::Whole)),
            "full" | "complete" => (1, Say::Part(Part::Whole)),
            "whole" if at(1) == "event" => (2, Say::Part(Part::Whole)),
            "preliminary" if at(1) == "card" => (2, Say::Part(Part::Prelims)),
            p if prelim(p) => (1, Say::Part(Part::Prelims)),
            "cartelera" if at(1) == "preliminar" => (2, Say::Part(Part::Prelims)),
            "cartelera" if at(1) == "principal" => (2, Say::Part(Part::Main)),
            "card" if at(1) == "preliminar" => (2, Say::Part(Part::Prelims)),
            "card" if at(1) == "principal" => (2, Say::Part(Part::Main)),
            "estelar" => (1, Say::Part(Part::Main)),
            _ => {
                i += 1;
                continue;
            }
        };
        out.push(say);
        i += len;
    }
    out
}

/// Whether the words say something of a different sport.
fn label_sport(w: &[&str]) -> Option<Sport> {
    w.iter().find_map(|x| match *x {
        "soccer" => Some(Sport::Soccer),
        "basketball" => Some(Sport::Basketball),
        "golf" => Some(Sport::Golf),
        "mma" => Some(Sport::Mma),
        _ => None,
    })
}

const TECH: &[&str] = &[
    "480p", "576p", "720p", "1080p", "2160p", "4k", "web", "dl", "webrip", "hdtv", "bluray",
    "x264", "x265", "h264", "h265", "hevc", "aac", "ac3", "mkv", "mp4", "avi", "ts", "hd", "uhd",
    "proper", "repack", "internal", "remux", "dd5", "ddp5", "fps", "60fps",
];

/// Words at the ends of a side that are not part of a name: league words, series words, months,
/// part words, round words and technical words.
fn noise(x: &str) -> bool {
    static ALIAS: std::sync::LazyLock<std::collections::HashSet<String>> =
        std::sync::LazyLock::new(|| {
            catalog::LEAGUES
                .iter()
                .flat_map(|l| l.aliases.iter())
                .flat_map(|a| crate::title::tokens(a))
                .filter(|t| t.len() >= 2)
                .collect()
        });
    const WORDS: &[&str] = &[
        "fight",
        "night",
        "main",
        "card",
        "prelims",
        "prelim",
        "early",
        "full",
        "event",
        "complete",
        "preliminary",
        "preliminaries",
        "round",
        "rd",
        "matchweek",
        "matchday",
        "md",
        "mw",
        "week",
        "wk",
        "game",
        "leg",
        "group",
        "stage",
        "day",
        "quarter",
        "semi",
        "final",
        "finals",
        "qf",
        "sf",
        "playoffs",
        "playoff",
        "season",
        "the",
    ];
    TECH.contains(&x) || WORDS.contains(&x) || month_num(x).is_some() || ALIAS.contains(x)
}

/// A side with what is not a name taken off both ends: league words, series words, months, part
/// words, round words and technical words. A round word takes its letter with it ("Group H", in
/// front of the name or behind it). At most three words are kept, those nearest the separator.
fn clean_side(side: &[String], left_side: bool) -> Vec<String> {
    let round = |x: &str| {
        matches!(
            x,
            "round"
                | "rd"
                | "matchweek"
                | "matchday"
                | "md"
                | "mw"
                | "week"
                | "wk"
                | "game"
                | "leg"
                | "group"
                | "stage"
                | "day"
        )
    };
    let mut v: Vec<String> = side.to_vec();
    loop {
        let before = v.len();
        // the front
        if let Some(x) = v.first().cloned()
            && noise(&x)
        {
            v.remove(0);
            if round(&x) && v.first().is_some_and(|n| n.len() == 1) {
                v.remove(0);
            }
        }
        // the back: an ordinal or a suffix, a noise word, or a round word and its letter
        if let Some(x) = v.last().cloned() {
            if (v.len() > 1 && after_surname(&x)) || noise(&x) {
                v.pop();
            } else if x.len() == 1 && v.len() >= 2 && round(&v[v.len() - 2]) {
                v.truncate(v.len() - 2);
            }
        }
        if v.len() == before {
            break;
        }
    }
    if left_side {
        let n = v.len();
        v.split_off(n.saturating_sub(3))
    } else {
        v.truncate(3);
        v
    }
}

/// The two sides of every pair separator in the words: up to five words each way, stopping at a
/// technical word or a number (cleaned by `clean_side`).
fn pairs_of(w: &[&str]) -> Vec<(Vec<String>, Vec<String>)> {
    let stop = |x: &str| TECH.contains(&x) || x.bytes().all(|b| b.is_ascii_digit());
    let mut out = vec![];
    for (i, x) in w.iter().enumerate() {
        if !matches!(*x, "vs" | "versus" | "v" | "x") {
            continue;
        }
        let mut left: Vec<String> = w[..i]
            .iter()
            .rev()
            .take_while(|y| !stop(y))
            .take(5)
            .map(|y| y.to_string())
            .collect();
        left.reverse();
        let right: Vec<String> = w[i + 1..]
            .iter()
            .take_while(|y| !stop(y))
            .take(5)
            .map(|y| y.to_string())
            .collect();
        if !left.is_empty() && !right.is_empty() {
            out.push((left, right));
        }
    }
    out
}

/// What one name inside a payload claims about this event, compared with the stored event and the
/// part picked (see the module's list). `None`: nothing in it contradicts.
pub fn payload_claims(ev: &EventRow, known: &Known, name: &str, expected: Part) -> Option<Reason> {
    let league = catalog::find(&ev.league)?;
    // (a spaced hyphen between two runs is a pair separator)
    let text = name.replace(" - ", " vs ").replace(" \u{2013} ", " vs ");
    let words = without_ids(crate::title::tokens(&text));
    if words.is_empty() {
        return None;
    }
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let says = |claim: Claim| Some(Reason::NamesNotEvent { claim });
    let sport = league.sport;

    // Markers: shows, sequences of words, games, episodes.
    if w.iter().any(|x| release::SHOULDER_WORDS.contains(x)) {
        return says(Claim::Show);
    }
    for (seq, _) in release::SHOULDER_SEQUENCES {
        if w.windows(seq.len()).any(|x| x == *seq) {
            return says(Claim::Show);
        }
    }
    if w.iter().any(|x| release::NON_EVENT_WORDS.contains(x)) {
        return says(Claim::NonEvent);
    }
    if is_episode(&w) {
        return says(Claim::Episode);
    }

    // Parts, by sport.
    match sport {
        Sport::Mma => {
            let mut claimed: Vec<Part> = vec![];
            for s in card_parts(&w) {
                match s {
                    Say::Bout => return says(Claim::OneFight),
                    Say::Part(p) if !claimed.contains(&p) => claimed.push(p),
                    Say::Part(_) => {}
                }
            }
            if claimed.len() > 1 {
                return says(Claim::TwoParts);
            }
            if let Some(p) = claimed.first()
                && *p != expected
            {
                return Some(Reason::WrongPart {
                    said: (*p).into(),
                    expected: expected.into(),
                });
            }
        }
        Sport::Golf => {
            let (hints, _) = release::scan_parts(&w, true);
            let mut rounds: Vec<Part> = vec![];
            for h in hints {
                if let release::PartHint::Round(n) = h
                    && !rounds.contains(&Part::Round(n))
                {
                    rounds.push(Part::Round(n));
                }
            }
            if rounds.len() > 1 {
                return says(Claim::TwoRounds);
            }
            if let Some(p) = rounds.first()
                && *p != expected
            {
                return Some(Reason::WrongPart {
                    said: (*p).into(),
                    expected: expected.into(),
                });
            }
        }
        Sport::Soccer | Sport::Basketball | Sport::RugbyUnion | Sport::RugbyLeague => {
            // A round of the competition, not a part: compared with the stored one when there
            // is one, tolerated otherwise.
            let stored: Option<u32> = ev
                .round
                .as_deref()
                .and_then(|r| {
                    r.split(|c: char| !c.is_ascii_digit())
                        .find(|d| !d.is_empty())
                })
                .and_then(|d| d.parse().ok());
            if let Some(m) = stored {
                for (i, x) in w.iter().enumerate() {
                    if matches!(*x, "round" | "matchweek" | "gameweek" | "week" | "mw")
                        && let Some(n) = w
                            .get(i + 1)
                            .filter(|n| is_num(n, 1, 2))
                            .and_then(|n| n.parse::<u32>().ok())
                        && n != m
                    {
                        return says(Claim::AnotherRound);
                    }
                }
            }
        }
    }

    // Dates.
    let rfc = &time::format_description::well_known::Rfc3339;
    if let Ok(start) = OffsetDateTime::parse(&ev.starts_at, rfc) {
        let first = start.date() - time::Duration::days(1);
        let last = ev
            .ends_at
            .as_deref()
            .and_then(|e| OffsetDateTime::parse(e, rfc).ok())
            .map_or(start.date(), |e| e.date())
            + time::Duration::days(1);
        if dates_in(&w)
            .iter()
            .any(|readings| readings.iter().all(|d| *d < first || *d > last))
        {
            return says(Claim::AnotherDay);
        }
    }

    // Other competitions and sports.
    let own: Vec<String> = names::tokens(&ev.name);
    let inside_event = |seq: &[String]| !seq.is_empty() && own.windows(seq.len()).any(|x| x == seq);
    for l in catalog::LEAGUES.iter().filter(|l| l.slug != league.slug) {
        for a in l.aliases {
            let seq = crate::title::tokens(a);
            if seq.is_empty() || inside_event(&seq) {
                continue;
            }
            if league.aliases.iter().any(|m| {
                let mine = crate::title::tokens(m);
                mine.windows(seq.len()).any(|x| x == seq.as_slice())
            }) {
                continue;
            }
            if w.windows(seq.len()).any(|x| x == seq.as_slice()) {
                return says(Claim::AnotherCompetition);
            }
        }
    }
    if let Some(s) = label_sport(&w)
        && s != sport
    {
        return says(Claim::AnotherSport);
    }
    if sport == Sport::Golf {
        let mine = majors_in(&own);
        if majors_in(&words).iter().any(|m| !mine.contains(m)) {
            return says(Claim::AnotherTournament);
        }
    }

    // Card numbers: every one.
    if sport == Sport::Mma {
        let theirs = card_numbers(&w, league.aliases);
        let own_w: Vec<&str> = own.iter().map(String::as_str).collect();
        let stored = card_numbers(&own_w, league.aliases);
        match stored.first() {
            Some(m) if theirs.iter().any(|n| n != m) => return says(Claim::AnotherCardNumber),
            None if theirs.windows(2).any(|p| p[0] != p[1]) => {
                return says(Claim::TwoCardNumbers);
            }
            _ => {}
        }
    }

    // Pairs, side by side. Each side is first cleaned of what is not a name at either end (league
    // words, series words, dates, part words, rounds and technical words), then judged alone.
    let pairs = pairs_of(&w);
    if !pairs.is_empty() {
        let keys = names::team_keys(&known.teams, ev);
        let mine: Vec<&str> = [&ev.home_id, &ev.away_id]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        let this_words = participant_words(&ev.name);
        for (l, r) in &pairs {
            for side in [clean_side(l, true), clean_side(r, false)] {
                if side.is_empty() {
                    continue;
                }
                if league.teams {
                    // A club: by the matcher's own team resolution. One that is another team of
                    // the league is a contradiction; one that resolves to nobody is tolerated.
                    if let names::Resolved::Team(id) = names::resolve_side(&side, &keys, false)
                        && !mine.is_empty()
                        && !mine.contains(&id.as_str())
                    {
                        return says(Claim::AnotherTeam);
                    }
                } else if let Some(surname) = side.last()
                    && distinctive(surname)
                    && !this_words.contains(surname)
                    && known.others.iter().any(|o| o.contains(surname))
                {
                    // A fighter: only a surname identifies one. The last word of the side is
                    // another stored fighter's surname and none of this event's.
                    return says(Claim::OtherFighters);
                }
            }
        }
    }
    None
}

/// The names of a payload judged together.
#[derive(Debug, PartialEq, Eq)]
pub enum PayloadCheck {
    /// Nothing in it contradicts the event and part, and it holds at most one video.
    Fine,
    /// Something in it says it is not this event or part; the sentence says what.
    Contradicts(Reason),
    /// More than one video anywhere in it: this cut files exactly one video.
    SeveralVideos,
}

/// Whether a file name has an extension the code knows as video, importable or not.
fn is_video(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, x)| {
        let x = x.to_ascii_lowercase();
        crate::ingest::VIDEO_EXTENSIONS.contains(&x.as_str())
            || crate::ingest::OTHER_VIDEO.contains(&x.as_str())
    })
}

fn stem(n: &str) -> &str {
    n.rsplit_once('.')
        .filter(|(_, x)| is_video(&format!("a.{x}")))
        .map_or(n, |(s, _)| s)
}

/// Whether the words end with a league's own word or a series word ("UFC", "Fight Night").
fn ends_with_lead(words: &[String]) -> bool {
    let mut leads: Vec<Vec<String>> = catalog::LEAGUES
        .iter()
        .flat_map(|l| l.aliases.iter().map(|a| crate::title::tokens(a)))
        .collect();
    leads.push(vec!["fight".into(), "night".into()]);
    leads
        .iter()
        .any(|l| !l.is_empty() && words.len() >= l.len() && words[words.len() - l.len()..] == l[..])
}

/// Judges the names of a payload: the torrent's own name, every folder of every entry (a folder
/// that holds only text files too), and every video's name. Component boundaries are hard: a
/// date, a season or a pair never runs from one component into the next. The one thing read
/// across a boundary is a league or series word at the end of one component and a number that is
/// the whole of the next (`UFC/333/video.mkv` is `UFC 333`). The videos are counted over the
/// whole payload.
pub fn judge_payload(
    ev: &EventRow,
    known: &Known,
    torrent_name: Option<&str>,
    files: &[String],
    expected: Part,
) -> PayloadCheck {
    let mut names: Vec<String> = torrent_name.iter().map(|n| n.to_string()).collect();
    let mut videos = 0;
    for f in files {
        let parts: Vec<&str> = f.split('/').filter(|c| !c.is_empty()).collect();
        let Some((last, dirs)) = parts.split_last() else {
            continue;
        };
        // (folders of every entry; the base name only of a video)
        names.extend(dirs.iter().map(|d| d.to_string()));
        let video = is_video(f);
        if video {
            videos += 1;
            names.push(stem(last).to_string());
        }
        let comps: Vec<String> = dirs
            .iter()
            .map(|d| d.to_string())
            .chain(video.then(|| stem(last).to_string()))
            .collect();
        for pair in comps.windows(2) {
            let (a, b) = (
                crate::title::tokens(&pair[0]),
                crate::title::tokens(&pair[1]),
            );
            if ends_with_lead(&a) && b.len() == 1 && is_num(&b[0], 1, 3) {
                names.push(format!("{} {}", pair[0], pair[1]));
            }
        }
    }
    names.sort();
    names.dedup();
    for n in &names {
        if let Some(why) = payload_claims(ev, known, n, expected) {
            return PayloadCheck::Contradicts(why);
        }
    }
    if videos > 1 {
        return PayloadCheck::SeveralVideos;
    }
    PayloadCheck::Fine
}
