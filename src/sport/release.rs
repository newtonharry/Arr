//! Reads the name of a release as sport: which competition it names, the teams or fighters, the
//! date, the card number and the part of a card, and whatever marks it as something other than
//! the event itself (a countdown, a game, an episode). Pure: no network, no store, no clock.
//!
//! The parser only reports what the title says. Whether that is a given event is decided in
//! `matcher`. A title is read left to right as
//!
//! ```text
//! [broadcaster or year]*  LEAGUE  [round, place, number, names or teams]  [part]  QUALITY...
//! ```
//!
//! and everything from the first quality word (a resolution, a source, a codec) on is the
//! tail: group, audio, frame rate. Words before the league that are not on that short list
//! make the league a *qualified* one ("2 Bundesliga", "Rugby League Super League"), which is
//! a different competition from the catalogue's.

use super::catalog::{self, League};
use crate::quality::Quality;
use crate::title;
use std::sync::LazyLock;
use time::{Date, Month, OffsetDateTime};

/// Which side separator a title uses. `Vs` covers `vs`, `v` and `versus`; `At` is the American
/// `Away @ Home` or `Away at Home`; `Dash` is a hyphen with spaces around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sep {
    Vs,
    At,
    Dash,
}

/// How a card is numbered. `Numbered` is `UFC 334`, `FightNight` is `UFC Fight Night 293` (or
/// a Fight Night without a number), `Named` is a number after a place (`PFL MENA 9`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Series {
    None,
    Numbered,
    FightNight,
    Named,
}

/// The part of an event a title says it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartHint {
    /// No part word: for a card, the title is "unlabelled".
    None,
    EarlyPrelims,
    Prelims,
    /// "Main Card".
    Main,
    /// "Full Event", "Main Card And Prelims".
    Full,
    /// "Day 4" (golf).
    Day(u8),
    /// "Round 3" or "Final Round" (golf).
    Round(u8),
    /// One fight, not the card: "Main Event", "Co-Main", "Full Fight", "Fight 3", a bare "Main".
    Bout,
}

/// A round as a release or the schedule words it. `n` is the number of a numbered round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Round {
    pub kind: RoundKind,
    pub n: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundKind {
    Round,
    Matchweek,
    Matchday,
    Week,
    Day,
    Game,
    Match,
    Leg,
    Part,
    QuarterFinal,
    SemiFinal,
    Final,
    Playoffs,
    Preseason,
    Regular,
    Group,
}

/// How a title's round stands to the round the schedule stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The same round.
    Equal,
    /// A different round of the same kind, or a different knockout stage.
    Different,
    /// Two rounds of different kinds ("Final" and "Matchweek 9"): neither equals the other.
    Incomparable,
}

impl Round {
    /// Equality is the only way a round is verified. Like is compared with like: a "Round 5"
    /// and a "Matchweek 9" say nothing about each other, and a "Grand Final" is the final.
    pub fn standing(&self, other: &Round) -> Standing {
        if self.kind == other.kind {
            return if self.n == other.n {
                Standing::Equal
            } else {
                Standing::Different
            };
        }
        if self.n.is_none()
            && other.n.is_none()
            && is_knockout(self.kind)
            && is_knockout(other.kind)
        {
            return Standing::Different;
        }
        Standing::Incomparable
    }
}

fn is_knockout(k: RoundKind) -> bool {
    matches!(
        k,
        RoundKind::QuarterFinal | RoundKind::SemiFinal | RoundKind::Final
    )
}

/// What a title asserts about the event besides who plays and when: a stage or round, a season.
/// Each is checked against the stored event (`matcher::claims`); none is a filler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assertion {
    Round(Round),
    Season(u16, u16),
}

/// The rounds, stages and seasons a run of consecutive words asserts, each with where it starts
/// and how many words it takes. `part_sport` is whether "round N", "day N" and "final round" name
/// a part of the event (a golf tournament, a fight card) and not a stage.
fn scan_assertions(w: &[&str], part_sport: bool) -> Vec<(usize, usize, Assertion)> {
    let num = |s: &str| digits(s, 1, 3);
    let round = |kind: RoundKind, n: Option<u32>| Assertion::Round(Round { kind, n });
    let mut out = vec![];
    let mut i = 0;
    while i < w.len() {
        let next = w.get(i + 1).copied();
        let found: Option<(usize, Assertion)> = match w[i] {
            "round" | "rd" | "r" if !part_sport => next
                .and_then(num)
                .map(|n| (2, round(RoundKind::Round, Some(n)))),
            "day" if !part_sport => next
                .and_then(num)
                .map(|n| (2, round(RoundKind::Day, Some(n)))),
            "matchweek" | "mw" => next
                .and_then(num)
                .map(|n| (2, round(RoundKind::Matchweek, Some(n)))),
            "matchday" | "md" => next
                .and_then(num)
                .map(|n| (2, round(RoundKind::Matchday, Some(n)))),
            "week" | "wk" => next
                .and_then(num)
                .map(|n| (2, round(RoundKind::Week, Some(n)))),
            "game" => next
                .and_then(num)
                .map(|n| (2, round(RoundKind::Game, Some(n)))),
            "g" => next
                .and_then(num)
                .map(|n| (2, round(RoundKind::Game, Some(n)))),
            "match" => next
                .and_then(num)
                .map(|n| (2, round(RoundKind::Match, Some(n)))),
            "leg" => next
                .and_then(num)
                .map(|n| (2, round(RoundKind::Leg, Some(n)))),
            "pt" | "part" => next
                .and_then(num)
                .map(|n| (2, round(RoundKind::Part, Some(n)))),
            "quarter" | "semi" | "grand" if matches!(next, Some("final" | "finals")) => {
                let kind = match w[i] {
                    "quarter" => RoundKind::QuarterFinal,
                    "semi" => RoundKind::SemiFinal,
                    _ => RoundKind::Final,
                };
                Some((2, round(kind, None)))
            }
            "quarterfinal" | "quarterfinals" | "qf" => {
                Some((1, round(RoundKind::QuarterFinal, None)))
            }
            "semifinal" | "semifinals" | "sf" => Some((1, round(RoundKind::SemiFinal, None))),
            "final" | "finals" if !(part_sport && next == Some("round")) => {
                Some((1, round(RoundKind::Final, None)))
            }
            "playoffs" | "playoff" | "po" => Some((1, round(RoundKind::Playoffs, None))),
            "play" if matches!(next, Some("offs" | "off")) => {
                Some((2, round(RoundKind::Playoffs, None)))
            }
            "preseason" => Some((1, round(RoundKind::Preseason, None))),
            "pre" if next == Some("season") => Some((2, round(RoundKind::Preseason, None))),
            "regular" if next == Some("season") => Some((2, round(RoundKind::Regular, None))),
            "rs" => Some((1, round(RoundKind::Regular, None))),
            "group" if next == Some("stage") => Some((2, round(RoundKind::Group, None))),
            "group"
                if next.is_some_and(|l| l.len() == 1 && l.as_bytes()[0].is_ascii_lowercase()) =>
            {
                let n = u32::from(next.unwrap_or("a").as_bytes()[0] - b'a') + 1;
                Some((2, round(RoundKind::Group, Some(n))))
            }
            "season" => next
                .and_then(year_of)
                .map(|y| (2, Assertion::Season(y as u16, y as u16))),
            s => {
                if let Some(y) = year_of(s) {
                    Some((1, Assertion::Season(y as u16, y as u16)))
                } else {
                    // Glued forms: "round1", "matchweek9", "md10", "g7".
                    match s.find(|c: char| c.is_ascii_digit()) {
                        Some(k) if k > 0 && s[k..].bytes().all(|b| b.is_ascii_digit()) => {
                            let kind = match &s[..k] {
                                "round" | "rd" | "r" if !part_sport => Some(RoundKind::Round),
                                "matchweek" | "mw" => Some(RoundKind::Matchweek),
                                "matchday" | "md" => Some(RoundKind::Matchday),
                                "week" | "wk" => Some(RoundKind::Week),
                                "game" | "g" => Some(RoundKind::Game),
                                "leg" => Some(RoundKind::Leg),
                                _ => None,
                            };
                            kind.and_then(|k2| num(&s[k..]).map(|n| (1, round(k2, Some(n)))))
                        }
                        _ => None,
                    }
                }
            }
        };
        match found {
            Some((len, a)) => {
                out.push((i, len, a));
                i += len;
            }
            None => i += 1,
        }
    }
    out
}

/// A round named in free words (a stored label such as "Round 12", "Matchweek 9",
/// "Semi-final"). `None` for anything that is not a round.
pub fn round_of(words: &[String]) -> Option<Round> {
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    scan_assertions(&w, false)
        .into_iter()
        .find_map(|(_, _, a)| match a {
            Assertion::Round(r) => Some(r),
            Assertion::Season(..) => None,
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sides {
    pub left: Vec<String>,
    pub right: Vec<String>,
    pub sep: Sep,
}

/// One place a competition's name stands: the league, its words, and the words that stand
/// directly before and after it in the same stretch of the title (at most eight each).
#[derive(Debug, Clone)]
pub struct LeagueHit {
    pub slug: &'static str,
    pub words: Vec<String>,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SportRelease {
    pub raw: String,
    /// Every competition whose name the title carries (nested names count once).
    pub leagues: Vec<&'static League>,
    /// Each place a competition's name stands: the league and the words of that place.
    pub league_hits: Vec<LeagueHit>,
    /// Something other than a broadcaster or a year stands before the competition's name.
    pub qualified: bool,
    /// A division number right after the competition's name ("Bundesliga 2 ..."): another tier,
    /// unless it is the leading number of a participant's name ("Bundesliga 1. FC Union Berlin"),
    /// which only the stored teams can tell.
    pub tier_digit: Option<u32>,
    /// The sport labels the title carries ("Soccer", "Rugby"), checked against the event's sport.
    pub labels: Vec<&'static str>,
    pub women: bool,
    /// Youth, reserve or academy sides ("U21", "Reserves"): never the senior competition.
    /// Qualifier words of a youth, reserve or academy competition. A word that belongs to a
    /// participant's own stored name ("Junior dos Santos") is not one; `judge` decides.
    pub youth_words: Vec<String>,
    /// The dates the title carries, in the order written (day-month-year or year-month-day;
    /// never month-day-year).
    pub dates: Vec<Date>,
    /// A run that is shaped like a day-month-year date but is not a date (`01.19.2019`).
    pub date_unreadable: bool,
    /// Every stage, round and season the title asserts, each to be checked against the stored
    /// event. A word that belongs to one has `Role::Claim` with its index here.
    pub assertions: Vec<Assertion>,
    pub series: Series,
    pub number: Option<u32>,
    pub sides: Option<Sides>,
    /// How many places a side separator could stand (at most `MAX_SPLITS`); the matcher tries
    /// each with `parse_split` and keeps the one under which both sides resolve.
    pub splits: usize,
    /// Which split this reading is (`None`: the default, by position).
    pub split: Option<usize>,
    /// Every numeral after a name ("Walker 2", "Walker II").
    pub rematches: Vec<u8>,
    /// The part the title says; the one part all its part words agree on.
    pub part: PartHint,
    /// A different number stands in the card number's place too ("UFC 333 334").
    pub number_conflict: bool,
    /// "Men" or "Men's": the men's competition.
    pub men: bool,
    /// Words that make it a show about the event, not the event.
    pub shoulder: Vec<&'static str>,
    /// What marks it as a game, an update or an episode of something else.
    pub non_event: Vec<&'static str>,
    /// Words standing before the names or teams (a place, a tournament's name) that no rule
    /// accounts for. For a game they are unexplained; for a card or a tournament they are the
    /// place or the name.
    pub head: Vec<String>,
    /// The words of the competition's name as the title wrote them.
    pub alias: Vec<String>,
    /// Every word between the competition's name and the tail, in order.
    pub body: Vec<String>,
    pub quality: Quality,
    pub fps: Option<u16>,
    pub language: Option<String>,
    pub group: Option<String>,
    pub published: Option<OffsetDateTime>,
    /// The title is longer (in bytes or in words) than anything real; nothing else was read.
    pub oversized: bool,
    /// The title and its `[..]` block name different parts ("Prelims" and "Main Card").
    pub part_conflict: bool,
    /// Every word of the title and the rule that accounts for it. A word nothing accounts for
    /// is `Role::Unexplained`, and a title with one is refused.
    pub roles: Vec<(String, Role)>,
}

/// What accounts for a word of a title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A broadcaster, "the" or a year before the competition's name.
    Prefix,
    /// The competition's own name.
    League,
    /// A word of an assertion (a stage, a round, a season): `claims` checks it against the
    /// stored event. The index is into `SportRelease::assertions`.
    Claim(usize),
    /// A card's number or serial (judged by the matcher).
    Number,
    /// The series' name ("Fight Night"; judged by the matcher).
    Series,
    /// A part word ("Prelims", "Main", "Full").
    Part,
    /// A team, a fighter or a separator between them (judged by the matcher).
    Names,
    /// A place or a tournament's name before the names (judged by the matcher).
    Head,
    /// Resolution, source, codec, container, frame rate, audio.
    Format,
    /// The tag of an uploader on the compiled list of known ones (`KNOWN_UPLOADERS`), once.
    Group,
    /// The language, and the channel it came from, in the `[..]` block.
    Language,
    /// A sport label in the `[..]` block ("Soccer").
    Label,
    /// A short code right after a channel name ("DAZN-SAS"): verified only if it is the code of
    /// one of the event's own teams, which the matcher checks; until then it is as good as free.
    Feed,
    /// Text that is let through but checked against nothing: a word in the tail or a `[..]`
    /// block that is in no closed list. A match with such a word is at most `likely`.
    Free,
    /// No rule accounts for it, and the title is refused.
    Unexplained,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Prefix => "broadcaster or year before the competition",
            Role::League => "the competition's name",
            Role::Claim(_) => "stage, round or season (checked against the event)",
            Role::Number => "card number or serial (judged by the matcher)",
            Role::Series => "series name (judged by the matcher)",
            Role::Part => "part word",
            Role::Names => "team or fighter (judged by the matcher)",
            Role::Head => "place or tournament name (judged by the matcher)",
            Role::Format => "format",
            Role::Group => "uploader's tag",
            Role::Language => "language or channel",
            Role::Label => "sport label",
            Role::Feed => "a team's feed code (judged by the matcher)",
            Role::Free => "UNVERIFIED",
            Role::Unexplained => "UNEXPLAINED",
        }
    }
}

impl SportRelease {
    /// The words no rule accounts for.
    pub fn unexplained(&self) -> Vec<&str> {
        self.roles
            .iter()
            .filter(|(_, r)| *r == Role::Unexplained)
            .map(|(w, _)| w.as_str())
            .collect()
    }

    /// The words that are let through unchecked. A match that has any is at most `likely`.
    pub fn unverified(&self) -> Vec<&str> {
        self.roles
            .iter()
            .filter(|(_, r)| *r == Role::Free)
            .map(|(w, _)| w.as_str())
            .collect()
    }

    /// The team feed codes the title carries ("sas" in "DAZN-SAS").
    pub fn feed_codes(&self) -> Vec<&str> {
        self.roles
            .iter()
            .filter(|(_, r)| *r == Role::Feed)
            .map(|(w, _)| w.as_str())
            .collect()
    }

    /// Each word with the rule that accounts for it, for tests and logs.
    pub fn explain(&self) -> String {
        self.roles
            .iter()
            .map(|(w, r)| format!("{w}: {}", r.as_str()))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

// ------------------------------------------------------------------------------------ lexing

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A folded word and the punctuation that stood before it (`.`, `-`, `/`, `_`, or a space).
    W(String, char),
    Date(Date),
    Season(u16, u16),
    /// `|`, `:`, `;`, `,` or a slash with spaces: the title's own segment break.
    Bar,
    At,
    /// A hyphen with spaces around it.
    Dash,
}

/// Turns the entities and invisible characters of a web title into what a reader would see.
/// A soft hyphen or a zero-width space inside a word is dropped, so the word is whole again; any
/// other entity becomes a space. Nothing can hide a word from the checks behind one.
fn decode(raw: &str) -> String {
    let mut s = raw
        .replace("&nbsp;", " ")
        .replace("&amp;", " and ")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&shy;", "")
        .replace('\u{a0}', " ");
    s.retain(|c| {
        !matches!(
            c,
            '\u{ad}' | '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{2060}'
        )
    });
    // Any entity left: `&name;` or `&#123;`.
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&' {
            let end = (i + 1..chars.len().min(i + 10)).find(|&j| chars[j] == ';');
            if let Some(j) = end
                && j > i + 1
                && chars[i + 1..j]
                    .iter()
                    .all(|c| c.is_ascii_alphanumeric() || *c == '#')
            {
                let name: String = chars[i + 1..j].iter().collect();
                out.push(entity_char(&name).unwrap_or(' '));
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The character an entity stands for, where that is a letter or a number: `&#233;` and
/// `&eacute;` are an e, so that a word with one in it is the word it reads as.
fn entity_char(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let n = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(n).filter(|c| c.is_alphanumeric());
    }
    let letter = name.chars().next()?;
    let accent = &name[letter.len_utf8()..];
    (letter.is_ascii_alphabetic()
        && matches!(
            accent,
            "acute" | "grave" | "circ" | "uml" | "tilde" | "cedil" | "ring" | "slash"
        ))
    .then_some(letter)
}

/// The most groups (`[..]`, `{..}`) a title may have.
const MAX_GROUPS: usize = 8;

/// Splits `[..]` and `{..}` groups (group tags, the format block of forum titles) from the
/// title. `None` when there are more groups than any real title has.
fn split_groups(s: &str) -> Option<(String, Vec<String>)> {
    let (mut main, mut groups, mut depth) = (String::new(), Vec::<String>::new(), 0u32);
    for c in s.chars() {
        match c {
            '[' | '{' => {
                if depth == 0 {
                    groups.push(String::new());
                    main.push(' ');
                }
                depth += 1;
            }
            ']' | '}' if depth > 0 => depth -= 1,
            _ if depth > 0 => {
                if let Some(g) = groups.last_mut() {
                    g.push(c);
                }
            }
            _ => main.push(c),
        }
        if groups.len() > MAX_GROUPS {
            return None;
        }
    }
    Some((main, groups))
}

fn lex(s: &str) -> Vec<Tok> {
    let chars: Vec<char> = title::fold_chars(s).chars().collect();
    let (mut out, mut i, mut punct) = (vec![], 0usize, ' ');
    let space_at = |k: Option<usize>| k.is_none_or(|k| chars[k].is_whitespace());
    while i < chars.len() {
        let c = chars[i];
        if c.is_alphanumeric() {
            let start = i;
            while i < chars.len() && chars[i].is_alphanumeric() {
                i += 1;
            }
            out.push(Tok::W(chars[start..i].iter().collect(), punct));
            punct = ' ';
            continue;
        }
        let spaced = space_at(i.checked_sub(1)) && space_at((i + 1 < chars.len()).then_some(i + 1));
        match c {
            '@' => out.push(Tok::At),
            '|' | ':' | ';' | ',' => out.push(Tok::Bar),
            '/' if spaced => out.push(Tok::Bar),
            '-' if spaced => out.push(Tok::Dash),
            '.' | '-' | '/' | '_' => punct = c,
            _ => {}
        }
        i += 1;
    }
    out
}

fn digits(s: &str, min: usize, max: usize) -> Option<u32> {
    (s.len() >= min && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

fn year_of(s: &str) -> Option<i32> {
    digits(s, 4, 4)
        .filter(|y| (1900..=2099).contains(y))
        .map(|y| y as i32)
}

fn month_of(s: &str) -> Option<Month> {
    Some(match s {
        "jan" | "january" => Month::January,
        "feb" | "february" => Month::February,
        "mar" | "march" => Month::March,
        "apr" | "april" => Month::April,
        "may" => Month::May,
        "jun" | "june" => Month::June,
        "jul" | "july" => Month::July,
        "aug" | "august" => Month::August,
        "sep" | "sept" | "september" => Month::September,
        "oct" | "october" => Month::October,
        "nov" | "november" => Month::November,
        "dec" | "december" => Month::December,
        _ => return None,
    })
}

fn date_of(y: i32, m: u32, d: u32) -> Option<Date> {
    let month = Month::try_from(u8::try_from(m).ok()?).ok()?;
    Date::from_calendar_date(y, month, u8::try_from(d).ok()?).ok()
}

/// Folds runs of words that are dates or seasons into one token. Day-month-year needs `.`, `-`
/// or `/` between its numbers and is always read day first; year-month-day takes a space too.
fn fold_dates(toks: Vec<Tok>) -> (Vec<Tok>, bool) {
    let mut out: Vec<Tok> = vec![];
    let mut unreadable = false;
    let mut i = 0;
    let w = |k: usize| match toks.get(k) {
        Some(Tok::W(s, p)) => Some((s.as_str(), *p)),
        _ => None,
    };
    while i < toks.len() {
        if let Some((a, _)) = w(i) {
            // 2026-2027 and 2026-27
            if let (Some(y), Some((b, '-'))) = (year_of(a), w(i + 1)) {
                let next = digits(b, 4, 4)
                    .filter(|n| *n as i32 == y + 1)
                    .or_else(|| digits(b, 2, 2).filter(|n| *n as i32 == (y + 1) % 100));
                if next.is_some() {
                    out.push(Tok::Season(y as u16, (y + 1) as u16));
                    i += 2;
                    continue;
                }
            }
            // year month day
            if let (Some(y), Some((b, pb)), Some((c, pc))) = (year_of(a), w(i + 1), w(i + 2))
                && pb == pc
                && " .-/_".contains(pb)
                && let (Some(m), Some(d)) = (digits(b, 1, 2), digits(c, 1, 2))
                && let Some(date) = date_of(y, m, d)
            {
                out.push(Tok::Date(date));
                i += 3;
                continue;
            }
            // day month year, day first
            if let (Some(d), Some((b, pb)), Some((c, pc))) = (digits(a, 1, 2), w(i + 1), w(i + 2))
                && pb == pc
                && ".-/".contains(pb)
                && let (Some(m), Some(y)) = (digits(b, 1, 2), year_of(c))
            {
                match date_of(y, m, d) {
                    Some(date) => out.push(Tok::Date(date)),
                    None => unreadable = true,
                }
                i += 3;
                continue;
            }
            // 26 Sep 2026, Sep 26 2026
            if let (Some((b, _)), Some((c, _))) = (w(i + 1), w(i + 2)) {
                let parsed = match (digits(a, 1, 2), month_of(b), year_of(c)) {
                    (Some(d), Some(m), Some(y)) => Date::from_calendar_date(y, m, d as u8).ok(),
                    _ => match (month_of(a), digits(b, 1, 2), year_of(c)) {
                        (Some(m), Some(d), Some(y)) => Date::from_calendar_date(y, m, d as u8).ok(),
                        _ => None,
                    },
                };
                if let Some(date) = parsed {
                    out.push(Tok::Date(date));
                    i += 3;
                    continue;
                }
            }
        }
        out.push(toks[i].clone());
        i += 1;
    }
    (out, unreadable)
}

// -------------------------------------------------------------------------------- word lists

/// Channels that put their name in a title; they say nothing about which event it is.
const BROADCASTERS: &[&str] = &[
    "bbc",
    "itv",
    "sky",
    "tnt",
    "espn",
    "nbc",
    "cbs",
    "abc",
    "fox",
    "dazn",
    "viaplay",
    "bt",
    "optus",
    "stan",
    "foxtel",
    "kayo",
    "ten",
    "nine",
    "sbs",
    "peacock",
    "paramount",
    "amazon",
    "prime",
    "tsn",
    "sn",
    "rtl",
    "ard",
    "zdf",
    "canal",
    "movistar",
    "tvnz",
    "sport",
    "sports",
    "hd",
    "uk",
    "us",
    "au",
    "nz",
    "ppv",
    "usa",
];

/// Words that only join the others and assert nothing about the event. Everything that used to
/// stand here with a meaning ("final", "game", "group", "regular", "season", "round", "week",
/// "leg", "tour", "championship", "event", "card", "men", "male") is an assertion, a part word or
/// a word the event must carry itself, never a filler.
const CONNECTIVES: &[&str] = &["and", "of", "the", "vs", "v"];

/// Words that may stand at the head of a name's run without being the name (they are given
/// their own roles: the series' name, a part, a connective); they only decide where the names
/// begin.
const LEAD_WORDS: &[&str] = &[
    "fight",
    "night",
    "card",
    "event",
    "main",
    "full",
    "early",
    "prelims",
    "prelim",
    "men",
    "mens",
    "male",
    "tour",
    "championship",
    "and",
    "of",
    "the",
    "vs",
    "v",
];

const WOMEN: &[&str] = &[
    "women",
    "womens",
    "frauen",
    "ladies",
    "femenina",
    "feminin",
    "feminine",
    "femminile",
    "girls",
];

const SHOULDER_WORDS: &[&str] = &[
    "countdown",
    "embedded",
    "vlog",
    "vlogs",
    "weighin",
    "weighins",
    "preview",
    "presser",
    "highlights",
    "highlight",
    "condensed",
    "recap",
    "interview",
    "interviews",
    "documentary",
    "podcast",
    "analysis",
    "reaction",
    "reactions",
    "postfight",
    "prefight",
    "postmatch",
    "prematch",
    "faceoff",
    "faceoffs",
    "magazine",
    "inside",
    "behind",
    "unseen",
    "bloopers",
    // The same shows in other languages: Spanish, Portuguese, French, German, Italian, Dutch.
    "resumen",
    "resumo",
    "resume",
    "zusammenfassung",
    "sintesi",
    "samenvatting",
    "compacto",
];

const SHOULDER_SEQUENCES: &[(&[&str], &str)] = &[
    (&["weigh", "in"], "weigh-in"),
    (&["weigh", "ins"], "weigh-in"),
    (&["press", "conference"], "press conference"),
    (&["post", "fight"], "post-fight"),
    (&["pre", "fight"], "pre-fight"),
    (&["post", "match"], "post-match"),
    (&["pre", "match"], "pre-match"),
    (&["post", "game"], "post-game"),
    (&["pre", "game"], "pre-game"),
    (&["face", "off"], "face-off"),
    (&["behind", "the", "scenes"], "behind the scenes"),
    (&["match", "of", "the", "day"], "highlights show"),
    (&["all", "access"], "all access"),
    (&["full", "season"], "whole season"),
    (&["best", "of"], "best of"),
    (&["melhores", "momentos"], "highlights"),
    (&["extended", "cut"], "extended cut"),
    // A replay is the event itself, except where it names a studio show about it.
    (&["replay", "show"], "studio show"),
    (&["studio", "replay"], "studio show"),
    (&["studio", "show"], "studio show"),
    (&["top", "10"], "top 10"),
];

const NON_EVENT_WORDS: &[&str] = &[
    "update",
    "updates",
    "dlc",
    "dlcs",
    "crack",
    "cracked",
    "denuvo",
    "denuvowo",
    "hypervisor",
    "steam",
    "ps4",
    "ps5",
    "xbox",
    "nsw",
];

/// Words from which the tail of a title starts: resolutions, sources, codecs, containers.
fn tail_marker(s: &str) -> bool {
    matches!(
        s,
        "web"
            | "webdl"
            | "webrip"
            | "hdtv"
            | "pdtv"
            | "iptv"
            | "bluray"
            | "bdrip"
            | "hdrip"
            | "dvdrip"
            | "tvrip"
            | "hdcam"
            | "h264"
            | "h265"
            | "x264"
            | "x265"
            | "hevc"
            | "avc"
            | "xvid"
            | "divx"
            | "mkv"
            | "mp4"
            | "avi"
            | "ac3"
            | "eac3"
            | "4k"
            | "uhd"
            | "fhd"
            | "sdtv"
            | "1080i"
            | "2160p"
            | "1080p"
            | "720p"
            | "576p"
            | "480p"
            | "540p"
            | "360p"
    ) || (s.starts_with("aac") && s[3..].bytes().all(|b| b.is_ascii_digit()))
        || (s.ends_with("fps")
            && s.len() > 3
            && s[..s.len() - 3].bytes().all(|b| b.is_ascii_digit()))
}

fn part_word(s: &str) -> bool {
    matches!(s, "prelims" | "prelim" | "early" | "main" | "full")
}

/// Words of the format block and the tail of a title that carry no information about the event.
fn tail_vocab(s: &str) -> bool {
    tail_marker(s)
        || s.bytes().all(|b| b.is_ascii_digit())
        || is_language(s)
        || matches!(
            s,
            "hd" | "sd"
                | "dl"
                | "rip"
                | "h"
                | "hdr"
                | "dv"
                | "10bit"
                | "aac"
                | "ac"
                | "dd"
                | "ddp"
                | "atmos"
                | "flac"
                | "proper"
                | "repack"
                | "internal"
                | "multi"
                | "dual"
                | "tv"
                | "ts"
                | "sat"
                | "stereo"
                | "x"
                | "mp3"
                | "fps"
                | "nfo"
        )
}

const LANGUAGE_WORDS: &[&str] = &[
    "en",
    "eng",
    "english",
    "nl",
    "dutch",
    "ro",
    "romanian",
    "de",
    "german",
    "deutsch",
    "fr",
    "french",
    "es",
    "spanish",
    "it",
    "italian",
    "pt",
    "portuguese",
    "ru",
    "russian",
];

fn is_language(s: &str) -> bool {
    LANGUAGE_WORDS.contains(&s)
}

const SPORT_LABELS: &[&str] = &[
    "soccer",
    "football",
    "basketball",
    "rugby",
    "golf",
    "mma",
    "sports",
    "sport",
];

/// Whether a bare number after the word `prev` is one the format vocabulary accounts for
/// ("H 264", "AAC2 0", "DD5 1"): a codec or an audio word, then its number.
fn format_number(prev: Option<&str>) -> bool {
    prev.is_some_and(|p| {
        matches!(p, "h" | "x" | "ac" | "eac" | "mpeg")
            || ["aac", "dd", "ddp", "ac", "eac"]
                .iter()
                .any(|a| p.starts_with(a) && p[a.len()..].bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Channels and networks named in the `[..]` block of forum titles. Closed: a word that is not
/// here (or a code of at most three letters right after one, "DAZN-IND") is free text.
const CHANNEL_WORDS: &[&str] = &[
    "spectrum", "network", "plus", "tv", "sport", "sports", "extra", "premium", "hd", "uhd",
    "live", "now", "go",
];

fn channel_word(w: &str, prev: Option<&str>) -> bool {
    BROADCASTERS.contains(&w)
        || CHANNEL_WORDS.contains(&w)
        || is_language(w)
        || (prev == Some("dazn") && FEED_REGIONS.contains(&w))
}

/// The regions a broadcaster's feed is labelled with ("DAZN-IND"): a closed list. A team's feed
/// ("DAZN-SAS") is a code of one of the event's own teams: it is a `Role::Feed` word, checked
/// against the event by the matcher, and free until it is.
const FEED_REGIONS: &[&str] = &[
    "ind", "usa", "gbr", "uk", "can", "deu", "ger", "esp", "ita", "fra", "bra", "jpn", "aus",
    "nzl", "mex", "arg", "nld", "bel", "swe", "nor", "fin", "dnk", "pol", "prt", "aut", "che",
    "zaf",
];

/// The uploaders whose tags are seen on real sport titles, spelled as they write them (exact,
/// case-sensitive). This list is the only thing that makes a tag a verified word: a tag that is
/// not here is free text, however plain it looks, because the last word of a title can hide
/// anything ("Round-UP", "WEB-DL-vlogs", "WEB-DL-24").
const KNOWN_UPLOADERS: &[&str] = &[
    "AFG",
    "DARKSPORT",
    "FBB",
    "JFF",
    "MeGusta",
    "NTb",
    "nVa",
    "NX",
    "POOTLED",
    "RUNE",
    "SHREDDiE",
    "TGx",
    "TJ",
    "TJET",
    "VARYG",
    "ViCEROYS",
    "teaspoon",
];
/// Uploaders whose tag is spelled as two words ("Fight-BB"): both exactly.
const KNOWN_UPLOADER_PAIRS: &[(&str, &str)] = &[("Fight", "BB")];

/// Substrings that make a word a shoulder marker wherever they stand in it: a word that glues
/// one to something else ("ExtendedHighlights", "Highlights2026") is as much a show as the word
/// alone. Only words of six letters or more are tested, so "ko" in "Tokyo" is not.
const SHOULDER_SUBSTRINGS: &[&str] = &[
    "highlight",
    "recap",
    "resumen",
    "resumo",
    "resume",
    "zusammenfassung",
    "sintesi",
    "samenvatting",
    "summary",
    "summaries",
    "countdown",
    "embedded",
    "preview",
    "weighin",
    "presser",
    "interview",
    "documentary",
    "podcast",
    "analysis",
    "reaction",
    "postfight",
    "prefight",
    "postmatch",
    "prematch",
    "faceoff",
    "magazine",
    "bloopers",
    "unseen",
    "fullfight",
    "mainevent",
    "comain",
    "melhoresmomentos",
    "wrapup",
    "roundup",
    "digest",
    "rewind",
    "condensed",
    "compacto",
    "extended",
];

/// Whether `w` (a folded word) carries a shoulder marker inside it.
fn glued_shoulder(w: &str) -> Option<&'static str> {
    if w.len() < 6 {
        return None;
    }
    SHOULDER_SUBSTRINGS.iter().copied().find(|s| w.contains(s))
}

static ALIASES: LazyLock<Vec<(&'static League, Vec<String>)>> = LazyLock::new(|| {
    let mut v = vec![];
    for l in catalog::LEAGUES {
        for a in std::iter::once(&l.name).chain(l.aliases.iter()) {
            let t = title::tokens(a);
            if !t.is_empty() {
                v.push((l, t));
            }
        }
    }
    v
});

#[derive(Debug, Clone, Copy)]
struct Hit {
    start: usize,
    end: usize,
    league: &'static League,
}

/// The most competition names a title may carry before it is called nonsense.
const MAX_HITS: usize = 32;

/// Every place a competition's name stands in the title, longer names first at one place, nested
/// names dropped. Work is bounded: a title has at most `MAX_WORDS` words (checked before this
/// runs), each word is compared with every name once, and at most `MAX_HITS` hits are kept, so
/// the passes after that are over a short list. `None` when there are more.
fn find_hits(toks: &[Tok]) -> Option<Vec<Hit>> {
    let mut hits: Vec<Hit> = vec![];
    for i in 0..toks.len() {
        if !matches!(toks[i], Tok::W(..)) {
            continue;
        }
        for (league, alias) in ALIASES.iter() {
            let end = i + alias.len();
            if end > toks.len() {
                continue;
            }
            let same = toks[i..end]
                .iter()
                .zip(alias)
                .all(|(t, a)| matches!(t, Tok::W(s, _) if s == a));
            if same {
                hits.push(Hit {
                    start: i,
                    end,
                    league,
                });
                if hits.len() > MAX_HITS {
                    return None;
                }
            }
        }
    }
    let all = hits.clone();
    hits.retain(|h| {
        !all.iter().any(|o| {
            (o.start, o.end) != (h.start, h.end)
                && o.start <= h.start
                && h.end <= o.end
                && o.end - o.start > h.end - h.start
        })
    });
    hits.sort_by_key(|h| (h.start, std::cmp::Reverse(h.end)));
    let mut seen: Vec<(usize, usize, &str)> = vec![];
    hits.retain(|h| {
        let k = (h.start, h.end, h.league.slug);
        let fresh = !seen.contains(&k);
        seen.push(k);
        fresh
    });
    Some(hits)
}

fn contains_seq(words: &[&str], seq: &[&str]) -> bool {
    words.windows(seq.len()).any(|w| w == seq)
}

fn language_of(parsed: &[&'static str], bracket: &str) -> Option<String> {
    if let Some(first) = parsed.first() {
        let mut c = first.chars();
        return c
            .next()
            .map(|f| f.to_uppercase().collect::<String>() + c.as_str());
    }
    for w in title::tokens(bracket) {
        let l = match w.as_str() {
            "en" | "eng" | "english" => "English",
            "nl" | "dutch" => "Dutch",
            "ro" | "romanian" => "Romanian",
            "de" | "german" | "deutsch" => "German",
            "fr" | "french" => "French",
            "es" | "spanish" => "Spanish",
            "it" | "italian" => "Italian",
            "pt" | "portuguese" => "Portuguese",
            "ru" | "russian" => "Russian",
            _ => continue,
        };
        return Some(l.to_string());
    }
    None
}

// ---------------------------------------------------------------------------------------- parse

/// The longest title (in bytes) that is read at all. Real titles are under 200; a forum title
/// with its whole format block is about 170. Anything longer is refused unread.
pub const MAX_TITLE_BYTES: usize = 400;
/// The most words the language or channel part of a `[..]` block may have ("TNT Sport HD",
/// "EN/SN Spectrum"). It is free text, so it is also short.
const MAX_CHANNEL_WORDS: usize = 4;
/// The most words (title and groups together) a title may have.
const MAX_WORDS: usize = 120;

/// The most places a side separator is tried at.
pub const MAX_SPLITS: usize = 8;

fn blank(published: Option<OffsetDateTime>) -> SportRelease {
    SportRelease {
        raw: String::new(),
        leagues: vec![],
        league_hits: vec![],
        qualified: false,
        tier_digit: None,
        labels: vec![],
        women: false,
        youth_words: vec![],
        dates: vec![],
        date_unreadable: false,
        assertions: vec![],
        series: Series::None,
        number: None,
        sides: None,
        splits: 0,
        split: None,
        rematches: vec![],
        part: PartHint::None,
        number_conflict: false,
        men: false,
        shoulder: vec![],
        non_event: vec![],
        head: vec![],
        alias: vec![],
        body: vec![],
        quality: Quality::Unknown,
        fps: None,
        language: None,
        group: None,
        published,
        oversized: true,
        part_conflict: false,
        roles: vec![],
    }
}

fn words_of(toks: &[Tok]) -> Vec<&str> {
    toks.iter()
        .filter_map(|t| match t {
            Tok::W(s, _) => Some(s.as_str()),
            _ => None,
        })
        .collect()
}

/// Every part the words name, in order, and the words that named them. Phrases are tried
/// longest first ("main card and prelims" is the whole card, "main card" the main card, a bare
/// "main" one fight). Parts that differ conflict; equal ones are one. `part_sport` is whether
/// "round N", "day N" and "final round" name a part of the event (a golf tournament, a fight
/// card) and not a stage.
fn scan_parts<'a>(w: &[&'a str], part_sport: bool) -> (Vec<PartHint>, Vec<&'a str>) {
    let mut parts = vec![];
    let mut used: Vec<&'a str> = vec![];
    let mut i = 0;
    while i < w.len() {
        let at = |n: usize| w.get(i + n).copied();
        let prelim = |s: Option<&str>| matches!(s, Some("prelims" | "prelim"));
        let (len, hint) = match w[i] {
            "main" if at(1) == Some("event") => (2, PartHint::Bout),
            "main" if at(1) == Some("card") && at(2) == Some("and") && prelim(at(3)) => {
                (4, PartHint::Full)
            }
            "main" if at(1) == Some("card") && prelim(at(2)) => (3, PartHint::Full),
            "main" if at(1) == Some("card") => (2, PartHint::Main),
            // A bare "Main" is not "Main Card": it could be the main event.
            "main" => (1, PartHint::Bout),
            "co" if at(1) == Some("main") => (2, PartHint::Bout),
            "comain" | "bout" | "bouts" => (1, PartHint::Bout),
            "full" if at(1) == Some("fight") => (2, PartHint::Bout),
            "full" if matches!(at(1), Some("event" | "card")) => (2, PartHint::Full),
            "full" => (1, PartHint::Full),
            "fight" if at(1).and_then(|n| digits(n, 1, 2)).is_some() => (2, PartHint::Bout),
            "early" if prelim(at(1)) => (2, PartHint::EarlyPrelims),
            "prelims" | "prelim" => (1, PartHint::Prelims),
            "day" if part_sport => match at(1).and_then(|n| digits(n, 1, 2)) {
                Some(n) => (2, PartHint::Day(n as u8)),
                None => {
                    i += 1;
                    continue;
                }
            },
            "round" | "rd" if part_sport => match at(1).and_then(|n| digits(n, 1, 2)) {
                Some(n) => (2, PartHint::Round(n as u8)),
                None => {
                    i += 1;
                    continue;
                }
            },
            "final" if part_sport && at(1) == Some("round") => (2, PartHint::Round(4)),
            _ => {
                i += 1;
                continue;
            }
        };
        parts.push(hint);
        used.extend(&w[i..i + len]);
        i += len;
    }
    (parts, used)
}

/// The assertions of every run of consecutive words in `toks`, and which assertion each word
/// belongs to. A folded season span (`Tok::Season`) is an assertion with no word.
fn claim_tokens(
    toks: &[Tok],
    part_sport: bool,
    assertions: &mut Vec<Assertion>,
) -> Vec<Option<usize>> {
    let mut at: Vec<Option<usize>> = vec![None; toks.len()];
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Season(a, b) => {
                assertions.push(Assertion::Season(*a, *b));
                i += 1;
            }
            Tok::W(..) => {
                let start = i;
                let mut words: Vec<&str> = vec![];
                while let Some(Tok::W(w, _)) = toks.get(i) {
                    words.push(w);
                    i += 1;
                }
                for (k, len, a) in scan_assertions(&words, part_sport) {
                    for slot in &mut at[start + k..start + k + len] {
                        *slot = Some(assertions.len());
                    }
                    assertions.push(a);
                }
            }
            _ => i += 1,
        }
    }
    at
}

/// A numeral after a name: "2", "II".
fn numeral(w: &str) -> Option<u8> {
    match w {
        "ii" => Some(2),
        "iii" => Some(3),
        "iv" => Some(4),
        _ => digits(w, 1, 1).filter(|n| *n >= 2).map(|n| n as u8),
    }
}

/// Reads a release title. `published` is the indexer's publish time, when it gave one. A title
/// over [`MAX_TITLE_BYTES`] bytes or [`MAX_WORDS`] words is not read (`oversized`): the work
/// here is a few passes over at most that many words, whatever the indexer sends.
pub fn parse_split(raw: &str, published: Option<OffsetDateTime>, split: usize) -> SportRelease {
    if raw.len() > MAX_TITLE_BYTES {
        return blank(published);
    }
    parse_bounded(raw, published, Some(split)).unwrap_or_else(|| blank(published))
}

pub fn parse(raw: &str, published: Option<OffsetDateTime>) -> SportRelease {
    if raw.len() > MAX_TITLE_BYTES {
        return blank(published);
    }
    parse_bounded(raw, published, None).unwrap_or_else(|| blank(published))
}

fn parse_bounded(
    raw: &str,
    published: Option<OffsetDateTime>,
    force_split: Option<usize>,
) -> Option<SportRelease> {
    let clean = decode(raw);
    // (words are counted as written: initials joined later do not make a long title short)
    let written = clean
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .count();
    if written > MAX_WORDS {
        return None;
    }
    let (main, groups) = split_groups(&clean)?;
    let (toks, unreadable_main) = fold_dates(lex(&main));
    let mut unreadable = unreadable_main;
    let mut gtoks: Vec<Vec<Tok>> = vec![];
    let mut count = toks.len();
    for g in &groups {
        // Forum titles keep the date in the format block: `[Rugby, 05.02.2022, HDTV/1080p]`.
        let (t, u) = fold_dates(lex(g));
        unreadable |= u;
        count += t.len();
        gtoks.push(t);
    }
    if count > MAX_WORDS {
        return None;
    }
    let date_unreadable = unreadable;
    let dates: Vec<Date> = toks
        .iter()
        .chain(gtoks.iter().flatten())
        .filter_map(|t| match t {
            Tok::Date(d) => Some(*d),
            _ => None,
        })
        .collect();
    let words: Vec<&str> = words_of(&toks);
    // The title's words in the order a reader sees them, across brackets, parentheses, braces,
    // bars and separators alike: phrases ("Main [Event]", "Round | Up") are read over these,
    // before any word is given a role.
    let flat_toks = lex(&clean);
    let flat: Vec<&str> = words_of(&flat_toks);
    // Every word of the title and of its groups, with a break between them so that a phrase
    // never runs from one into the next.
    let mut all_words: Vec<&str> = words.clone();
    let mut group_words: Vec<&str> = vec![];
    for g in &gtoks {
        group_words.push("|");
        group_words.extend(words_of(g));
    }
    all_words.extend(group_words.iter().copied());

    let hits = find_hits(&toks)?;
    let mut leagues: Vec<&'static League> = vec![];
    for h in &hits {
        if !leagues.iter().any(|l| l.slug == h.league.slug) {
            leagues.push(h.league);
        }
    }
    // A competition named only in a group ("[Bundesliga]") counts too: a title that names two
    // is ambiguous, wherever the second stands.
    for g in &gtoks {
        for h in find_hits(g)? {
            if !leagues.iter().any(|l| l.slug == h.league.slug) {
                leagues.push(h.league);
            }
        }
    }
    let mut league_hits: Vec<LeagueHit> = vec![];
    let word_of = |x: &Tok| match x {
        Tok::W(w, _) => Some(w.clone()),
        _ => None,
    };
    let hit_of = |t: &[Tok], h: &Hit| -> LeagueHit {
        let words: Vec<String> = t[h.start..h.end].iter().filter_map(word_of).collect();
        let mut before: Vec<String> = t[..h.start]
            .iter()
            .rev()
            .map_while(word_of)
            .take(8)
            .collect();
        before.reverse();
        let after: Vec<String> = t[h.end..].iter().map_while(word_of).take(8).collect();
        LeagueHit {
            slug: h.league.slug,
            words,
            before,
            after,
        }
    };
    for h in &hits {
        league_hits.push(hit_of(&toks, h));
    }
    for g in &gtoks {
        for h in find_hits(g)? {
            league_hits.push(hit_of(g, &h));
        }
    }
    let primary = hits.first().copied();
    let part_sport = primary
        .is_some_and(|h| matches!(h.league.sport, catalog::Sport::Mma | catalog::Sport::Golf));
    // Every stage, round and season the title asserts, with the words that make each.
    let mut assertions: Vec<Assertion> = vec![];
    let claim_at = claim_tokens(&toks, part_sport, &mut assertions);
    let gclaim_at: Vec<Vec<Option<usize>>> = gtoks
        .iter()
        .map(|g| claim_tokens(g, part_sport, &mut assertions))
        .collect();
    let body_start = primary.map_or(0, |h| h.end);
    let tail = (body_start..toks.len())
        .find(|&i| matches!(&toks[i], Tok::W(s, _) if tail_marker(s)))
        .unwrap_or(toks.len());

    let prefix_ok = |s: &str| BROADCASTERS.contains(&s) || s == "the" || s == "men" || s == "mens";
    // A division number right after a league's name ("Premier League 2", "Bundesliga 2") is
    // another tier. (A card's number, "UFC 334", is not.)
    let tier = primary.is_some_and(|h| {
        h.league.sport != catalog::Sport::Mma
            && matches!(toks.get(h.end), Some(Tok::W(s, _)) if digits(s, 1, 2).is_some())
            // (a run of numbers is a date written with a short year, not a division)
            && !matches!(toks.get(h.end + 1), Some(Tok::W(s, _)) if digits(s, 1, 2).is_some())
    });
    let tier_digit = tier
        .then(|| primary.and_then(|h| toks.get(h.end)))
        .flatten()
        .and_then(|t| match t {
            Tok::W(s, _) => digits(s, 1, 2),
            _ => None,
        });
    let qualified = primary.is_some_and(|h| {
        toks[..h.start]
            .iter()
            .enumerate()
            .any(|(i, t)| matches!(t, Tok::W(s, _) if !prefix_ok(s) && claim_at[i].is_none()))
    });
    let men = flat.iter().any(|w| matches!(*w, "men" | "mens"));
    let women = all_words.iter().any(|w| WOMEN.contains(w));
    let youth_words: Vec<String> = all_words
        .iter()
        .filter(|w| {
            matches!(**w, "youth" | "reserves" | "academy" | "juniors" | "junior")
                || (w.len() == 3
                    && w.starts_with('u')
                    && w[1..].bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|w| w.to_string())
        .collect();

    let mut shoulder: Vec<&'static str> = vec![];
    for w in &flat {
        if let Some(s) = SHOULDER_WORDS.iter().find(|s| *s == w)
            && !shoulder.contains(s)
        {
            shoulder.push(s);
        }
    }
    for (seq, label) in SHOULDER_SEQUENCES {
        if contains_seq(&flat, seq) && !shoulder.contains(label) {
            shoulder.push(label);
        }
    }
    // ...inside a word, and across the boundary of neighbouring words, whatever the mark between
    // them ("Round-UP", "Wrap.Up", "Post Fight"): up to three words are joined and tried.
    for i in 0..flat.len() {
        let mut joined = String::new();
        for next in flat[i..].iter().take(3) {
            joined.push_str(next);
            if let Some(s) = glued_shoulder(&joined)
                && !shoulder.contains(&s)
            {
                shoulder.push(s);
            }
        }
    }
    let mut non_event: Vec<&'static str> = vec![];
    for w in &all_words {
        let k = w.len() == 4
            && w.as_bytes()[0].is_ascii_digit()
            && w.as_bytes()[1] == b'k'
            && w[2..].bytes().all(|b| b.is_ascii_digit());
        let episode = w.starts_with('s')
            && w[1..]
                .split_once('e')
                .is_some_and(|(a, b)| digits(a, 1, 3).is_some() && digits(b, 1, 4).is_some());
        let label = if k {
            "video game"
        } else if episode {
            "episode"
        } else if NON_EVENT_WORDS.contains(w) {
            "software"
        } else {
            continue;
        };
        if !non_event.contains(&label) {
            non_event.push(label);
        }
    }

    // The part of the event, read over the title's words in their visible order. Parts that
    // differ anywhere in the title conflict ("Prelims ... [Early Prelims]", "Full Event ...
    // Round 3"); equal ones are one.
    let (parts, part_words) = scan_parts(&flat, part_sport);
    // (one fight among them is what the title is about: it is refused as a single bout)
    let part = if parts.contains(&PartHint::Bout) {
        PartHint::Bout
    } else {
        parts.first().copied().unwrap_or(PartHint::None)
    };
    let part_conflict = parts.iter().any(|p| *p != part);

    // Sides.
    let body = &toks[body_start..tail];
    // Every place a side separator could stand: a standalone `v`, `vs`, `versus`, `at`, `@` or
    // a spaced hyphen between the competition and the tail. Which one splits the names is not
    // decided here (a `v` can be a letter of "P.S.V."): `matcher::judge` tries them and keeps the
    // one under which both sides resolve. At most `MAX_SPLITS` are kept, the first by position,
    // and the title's word cap bounds the scan; the default is the one chosen by position.
    let mut sep_at: Option<(usize, Sep)> = None;
    let mut dash_at: Option<usize> = None;
    let mut at_word: Option<usize> = None;
    let mut cands: Vec<(usize, Sep)> = vec![];
    for (k, t) in body.iter().enumerate() {
        match t {
            Tok::W(s, _) if matches!(s.as_str(), "vs" | "v" | "versus") => {
                if sep_at.is_none() {
                    sep_at = Some((k, Sep::Vs));
                }
                cands.push((k, Sep::Vs));
            }
            Tok::At => {
                if sep_at.is_none() {
                    sep_at = Some((k, Sep::At));
                }
                cands.push((k, Sep::At));
            }
            Tok::Dash => {
                if dash_at.is_none() {
                    dash_at = Some(k);
                }
                cands.push((k, Sep::Dash));
            }
            Tok::W(s, _) if s == "at" => {
                if at_word.is_none() {
                    at_word = Some(k);
                }
                cands.push((k, Sep::At));
            }
            _ => {}
        }
    }
    cands.truncate(MAX_SPLITS);
    // The left run before body index `k`: where it starts (after what leads the names without
    // being one: a round, a stage, a number) and its words.
    let mma = primary.is_some_and(|h| h.league.sport == catalog::Sport::Mma);
    let left_of = |k: usize| -> (usize, Vec<String>) {
        let mut start = k;
        let mut run: Vec<String> = vec![];
        for (j, t) in body[..k].iter().enumerate().rev() {
            match t {
                Tok::W(s, _) => {
                    run.push(s.clone());
                    start = j;
                }
                _ => break,
            }
        }
        run.reverse();
        let mut skip = 0;
        // Claims are settled first: a token a claim owns ("Group H", "G 7", "Round 1") is part of
        // that claim and never of a name. Of the rest, no stripping step takes a token from an
        // initials run: an alphabetic single letter next to another ("V F B Stuttgart",
        // "A J McKee") is part of the name, never a lead word. A digit is never an initial.
        let single = |w: &String| w.len() == 1 && w.as_bytes()[0].is_ascii_alphabetic();
        while run.get(skip).is_some_and(|w| {
            let owned = claim_at[body_start + start + skip].is_some();
            let next_free = claim_at
                .get(body_start + start + skip + 1)
                .is_some_and(|c| c.is_none());
            !(!owned && single(w) && next_free && run.get(skip + 1).is_some_and(single))
                && (LEAD_WORDS.contains(&w.as_str())
                // (a card's number leads its names; for a game a leading number belongs to
                // the club: "1. FC Koln", "1860 Munchen", "04 Schalke")
                || (mma && w.bytes().all(|b| b.is_ascii_digit()))
                || claim_at[body_start + start + skip].is_some())
        }) {
            skip += 1;
        }
        (start + skip, run.split_off(skip))
    };
    if let Some(i) = force_split
        && let Some(c) = cands.get(i)
    {
        sep_at = Some(*c);
    } else if sep_at.is_none() {
        if let Some(k) = dash_at.filter(|k| !left_of(*k).1.is_empty()) {
            sep_at = Some((k, Sep::Dash));
        } else if let Some(k) = at_word {
            sep_at = Some((k, Sep::At));
        }
    }
    let mut rematches: Vec<u8> = vec![];
    // (body index of the first and the last name word, plus one) for the left and right runs
    let mut left_span = (0usize, 0usize);
    let mut right_end = 0usize;
    let sides = sep_at.and_then(|(k, sep)| {
        let (lstart, mut left) = left_of(k);
        let mut right: Vec<String> = vec![];
        let mut stop = body.len();
        for (j, t) in body.iter().enumerate().skip(k + 1) {
            match t {
                Tok::W(s, _)
                    if !part_word(s)
                        // (a token a claim owns ends the names: "Thunder G 7", never part of one)
                        && claim_at[body_start + j].is_none()
                        // (a day or a round after the names ends them)
                        && !(matches!(s.as_str(), "day" | "round") && j + 1 < body.len()) =>
                {
                    right.push(s.clone());
                }
                _ => {
                    stop = j;
                    break;
                }
            }
        }
        while right
            .last()
            .is_some_and(|w| BROADCASTERS.contains(&w.as_str()))
        {
            right.pop();
        }
        if right.len() > 1
            && let Some(n) = right.last().and_then(|w| numeral(w))
        {
            rematches.push(n);
            right.pop();
        }
        if left.len() > 1
            && let Some(n) = left.last().and_then(|w| numeral(w))
        {
            rematches.push(n);
            left.pop();
        }
        left_span = (lstart, k);
        right_end = stop;
        (!left.is_empty() && !right.is_empty()).then_some(Sides { left, right, sep })
    });
    // With no two sides every word is a word of the head.
    let sep_index = sides.as_ref().and(sep_at).map(|(k, _)| k);

    // Card number. Every number in a card's number place is kept: the first is the card's, an
    // equal one is the same claim, a different one conflicts.
    let (mut series, mut number) = (Series::None, None);
    let mut number_slots: Vec<usize> = vec![];
    let mut number_conflict = false;
    let stop = sep_index.unwrap_or(body.len());
    // Only a fight card has a number of its own ("UFC 334"); for a game or a tournament a
    // bare number is an assertion's or it is nothing the title can account for.
    let is_card = primary.is_some_and(|h| h.league.sport == catalog::Sport::Mma);
    for (k, t) in body.iter().enumerate().take(if is_card { stop } else { 0 }) {
        let Tok::W(s, _) = t else { continue };
        let Some(n) = digits(s, 1, 3) else { continue };
        // (a span a participant owns is not a card number: "Dillashaw 2 vs Garbrandt")
        if sep_index.is_some() && k >= left_span.0 && k < left_span.1 {
            continue;
        }
        let prev = k.checked_sub(1).and_then(|p| match &body[p] {
            Tok::W(w, _) => Some(w.as_str()),
            _ => None,
        });
        // (a number that belongs to an assertion or to a part is theirs)
        if claim_at[body_start + k].is_some()
            || prev.is_some_and(|p| matches!(p, "round" | "rd" | "day" | "fight"))
        {
            continue;
        }
        match number {
            None => {
                number = Some(n);
                series = match prev {
                    Some("night") => Series::FightNight,
                    None => Series::Numbered,
                    Some(_) => Series::Named,
                };
                number_slots.push(k);
            }
            Some(first) if first == n => number_slots.push(k),
            Some(_) => number_conflict = true,
        }
    }
    if number.is_none() && contains_seq(&words, &["fight", "night"]) {
        series = Series::FightNight;
    }

    // Quality and format.
    let bracket_text = groups.join(" ");
    let tail_text = format!("{main} {}", bracket_text.replace('/', " "));
    let parsed = crate::release::parse(&tail_text);
    let bracket_tokens = title::tokens(&bracket_text);
    let fps = words
        .iter()
        .copied()
        .chain(bracket_tokens.iter().map(String::as_str))
        .find_map(|w| {
            w.strip_suffix("fps")
                .and_then(|n| digits(n, 2, 3))
                .map(|n| n as u16)
        });

    // Which rule accounts for each word. Three kinds: a word a closed vocabulary or the event
    // itself checks (every role but the last two); a word that is let through unchecked
    // (`Free`), which caps a match at `likely`; and a word nothing accounts for
    // (`Unexplained`), which refuses the title. The one free word that is not capped is a single
    // uploader's tag in its place at the end of the title.
    let raw_main = main.trim();
    let mut roles: Vec<(String, Role)> = vec![];
    let in_hit = |i: usize| hits.iter().any(|h| i >= h.start && i < h.end);
    let word_at = |i: usize| match toks.get(i) {
        Some(Tok::W(w, _)) => Some(w.as_str()),
        _ => None,
    };
    let last_word = toks
        .iter()
        .rposition(|t| matches!(t, Tok::W(..)))
        .unwrap_or(0);
    // The uploader's tag: the last word of the title, spelled exactly as one of the compiled
    // list of known uploaders (the text after the last hyphen of the last chunk, or the last
    // chunk itself), once the technical words have come. A tag that is not on the list is free
    // text, however plain it looks. Known two-word tags ("Fight-BB") are taken together.
    let last_chunk = raw_main
        .split_whitespace()
        .next_back()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric());
    let tag_cand = last_chunk
        .rsplit(['-', '.', '_', '/'])
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric());
    let last_text = word_at(last_word).unwrap_or("");
    let after_technical = tail < toks.len() && last_word >= tail;
    let pair_first_text = last_word
        .checked_sub(1)
        .and_then(|j| (0..=j).rev().find(|&j| matches!(toks[j], Tok::W(..))))
        .and_then(word_at);
    let pair_ok = after_technical
        && KNOWN_UPLOADER_PAIRS
            .iter()
            .any(|(a, b)| tag_cand == *b && last_chunk.ends_with(&format!("{a}-{b}")))
        && last_text == "bb"
        // (the word before is "Fight" on its own, not something glued to it)
        && pair_first_text == Some("fight");
    let single_ok = after_technical
        && KNOWN_UPLOADERS.contains(&tag_cand)
        && title::fold_chars(tag_cand) == last_text;
    let mut tag_valid = pair_ok || single_ok;
    let pair_first = last_word
        .checked_sub(1)
        .and_then(|j| (0..=j).rev().find(|&j| matches!(toks[j], Tok::W(..))));
    let group: Option<String> = tag_valid.then(|| last_text.to_string());
    let mut part_budget: Vec<&str> = part_words.clone();
    let mut take_part = |w: &str| match part_budget.iter().position(|x| *x == w) {
        Some(p) => {
            part_budget.remove(p);
            true
        }
        None => false,
    };
    // Words that only join, or say which gender's competition it is (checked against the
    // league by `judge`; "Women's" is "women" and "s").
    let neutral = |w: &str, prev: Option<&str>| {
        CONNECTIVES.contains(&w)
            || matches!(w, "men" | "mens")
            || WOMEN.contains(&w)
            || (w == "s" && prev.is_some_and(|p| matches!(p, "men" | "women")))
    };
    for (i, t) in toks.iter().enumerate() {
        let Tok::W(w, _) = t else { continue };
        let prev_w = (0..i).rev().find_map(word_at);
        let claim = claim_at[i];
        let is_digits = w.bytes().all(|b| b.is_ascii_digit());
        let role = if in_hit(i) {
            Role::League
        } else if primary.is_some_and(|h| i < h.start) {
            if let Some(c) = claim {
                Role::Claim(c)
            } else if prefix_ok(w) {
                Role::Prefix
            } else {
                Role::Unexplained
            }
        } else if i >= tail {
            if (tag_valid && i == last_word) || (pair_ok && Some(i) == pair_first) {
                Role::Group
            } else if let Some(c) = claim {
                Role::Claim(c)
            } else if is_digits {
                if format_number(prev_w) {
                    Role::Format
                } else if take_part(w) {
                    Role::Part
                } else {
                    Role::Free
                }
            } else if tail_vocab(w) {
                Role::Format
            } else if take_part(w) {
                Role::Part
            } else if BROADCASTERS.contains(&w.as_str()) {
                Role::Language
            } else if neutral(w, prev_w) {
                Role::Prefix
            } else {
                Role::Free
            }
        } else {
            let k = i - body_start;
            let is_sep = sep_index == Some(k);
            let in_left = sep_index.is_some() && k >= left_span.0 && k < left_span.1;
            let in_right = sep_index.is_some_and(|s| k > s && k < right_end);
            if is_sep || in_left || in_right {
                Role::Names
            } else if number_slots.contains(&k) {
                Role::Number
            } else if let Some(c) = claim {
                Role::Claim(c)
            } else if take_part(w) {
                Role::Part
            } else if series == Series::FightNight && matches!(w.as_str(), "fight" | "night") {
                Role::Series
            } else if is_digits {
                Role::Free
            } else if BROADCASTERS.contains(&w.as_str()) {
                Role::Language
            } else if neutral(w, prev_w) {
                Role::Prefix
            } else if sep_index.is_none_or(|s| k < s) {
                Role::Head
            } else {
                Role::Unexplained
            }
        };
        roles.push((w.clone(), role));
    }
    // The words before the names (or all of them, with none) that nothing else accounts for are
    // the place or the tournament's name: the event must carry them itself.
    let head: Vec<String> = roles
        .iter()
        .filter(|(_, r)| *r == Role::Head)
        .map(|(w, _)| w.clone())
        .collect();
    // The groups: a sport label, a format block, the language and the channel after it (closed
    // lists), or a final group that is only an uploader's tag. Anything else is free text.
    let last_group = gtoks.len().saturating_sub(1);
    let mut bracket_tag: Option<String> = None;
    let mut labels: Vec<&'static str> = vec![];
    for (gi, t) in gtoks.iter().enumerate() {
        let mut segs: Vec<Vec<(&str, Option<usize>)>> = vec![vec![]];
        for (ti, tok) in t.iter().enumerate() {
            match tok {
                Tok::W(s, _) => segs
                    .last_mut()
                    .expect("one")
                    .push((s.as_str(), gclaim_at[gi][ti])),
                Tok::Bar => segs.push(vec![]),
                _ => {}
            }
        }
        segs.retain(|s| !s.is_empty());
        let raw_group = groups[gi].trim();
        let lone = segs.len() == 1 && segs[0].len() == 1;
        let lone_tag = lone
            && gi == last_group
            && !tag_valid
            && KNOWN_UPLOADERS.contains(&raw_group)
            && title::fold_chars(raw_group) == segs[0][0].0;
        if lone_tag {
            tag_valid = true;
            bracket_tag = Some(segs[0][0].0.to_string());
        }
        let mut after_language = false;
        for seg in &segs {
            let is_lang_seg = after_language || is_language(seg[0].0);
            if is_language(seg[0].0) {
                after_language = true;
            }
            let mut prev: Option<&str> = None;
            for (w, claim) in seg {
                let w = *w;
                let role = if lone_tag {
                    Role::Group
                } else if let Some(c) = claim {
                    Role::Claim(*c)
                } else if is_lang_seg {
                    if seg.len() <= MAX_CHANNEL_WORDS && channel_word(w, prev) {
                        Role::Language
                    } else if seg.len() <= MAX_CHANNEL_WORDS
                        && prev == Some("dazn")
                        && (2..=4).contains(&w.len())
                        && w.bytes().all(|b| b.is_ascii_alphabetic())
                        && !super::espn::outcome_like(w)
                    {
                        Role::Feed
                    } else {
                        Role::Free
                    }
                } else if seg.len() == 1 && SPORT_LABELS.contains(&w) {
                    // (kept: the matcher compares it with the event's sport)
                    labels.extend(SPORT_LABELS.iter().find(|l| **l == w));
                    Role::Label
                } else if w.bytes().all(|b| b.is_ascii_digit()) {
                    if format_number(prev) {
                        Role::Format
                    } else if take_part(w) {
                        Role::Part
                    } else {
                        Role::Free
                    }
                } else if tail_vocab(w) {
                    Role::Format
                } else if take_part(w) {
                    Role::Part
                } else if BROADCASTERS.contains(&w) {
                    Role::Language
                } else if neutral(w, prev) {
                    Role::Prefix
                } else {
                    Role::Free
                };
                roles.push((w.to_string(), role));
                prev = Some(w);
            }
        }
    }

    Some(SportRelease {
        raw: raw.to_string(),
        leagues,
        league_hits,
        qualified,
        tier_digit,
        labels,
        women,
        youth_words,
        dates,
        date_unreadable,
        assertions,
        series,
        number,
        sides,
        splits: cands.len(),
        split: force_split,
        rematches,
        part,
        number_conflict,
        men,
        shoulder,
        non_event,
        head,
        alias: primary
            .map(|h| {
                toks[h.start..h.end]
                    .iter()
                    .filter_map(|t| match t {
                        Tok::W(s, _) => Some(s.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        body: body
            .iter()
            .filter_map(|t| match t {
                Tok::W(s, _) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        quality: parsed.quality,
        fps,
        language: language_of(&parsed.languages, &bracket_text),
        group: group.or(bracket_tag),
        published,
        oversized: false,
        part_conflict,
        roles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u8, day: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
    }

    #[test]
    fn dates_are_read_day_first_or_year_first() {
        for (title, want) in [
            (
                "A-League 2026 11 07 Melbourne City Vs Western Sydney Wanderers 1080p",
                d(2026, 11, 7),
            ),
            (
                "EPL.2019.01.19.Liverpool.vs.Crystal.Palace.XviD-AFG",
                d(2019, 1, 19),
            ),
            (
                "EPL - Liverpool vs Crystal Palace 19.01.2019 720p HD",
                d(2019, 1, 19),
            ),
            (
                "EPL - Crystal Palace vs Liverpool 720p HD 20.08.2018",
                d(2018, 8, 20),
            ),
            (
                "Rugby League Wigan vs Wakefield 26 Sep 2026 BBC 720p",
                d(2026, 9, 26),
            ),
            (
                "Guinness Six Nations 2022 / Round1 / Scotland. vs. England [Rugby, 05.02.2022, HDTV/1080p, MKV/H.264, EN]",
                d(2022, 2, 5),
            ),
        ] {
            assert_eq!(parse(title, None).dates, vec![want], "{title}");
        }
        // A month-first date is not a date.
        let r = parse("EPL 01.19.2019 Liverpool vs Crystal Palace", None);
        assert!(r.date_unreadable && r.dates.is_empty());
        // A version number is not a date.
        assert!(
            parse("PGA TOUR 2K25 (v2026.03.27 & All DLCs)", None)
                .dates
                .is_empty()
        );
    }

    #[test]
    fn seasons_and_rounds() {
        let r = parse(
            "EPL 2026-2027 / Matchweek 9 / 31.10.2026 / Chelsea FC vs Manchester United [Soccer, WEB-DL HD/720p/25fps MKV/H.264, EN, TNT Sport HD]",
            None,
        );
        assert_eq!(
            r.assertions,
            vec![
                Assertion::Season(2026, 2027),
                Assertion::Round(Round {
                    kind: RoundKind::Matchweek,
                    n: Some(9)
                })
            ]
        );
        assert_eq!(r.dates, vec![d(2026, 10, 31)]);
        assert_eq!(r.fps, Some(25));
        assert_eq!(r.language.as_deref(), Some("English"));
        let s = r.sides.unwrap();
        assert_eq!(
            (s.left, s.right),
            (
                vec!["chelsea".into(), "fc".into()],
                vec!["manchester".into(), "united".into()]
            )
        );
        assert!(
            parse("NBA 2026-27 / RS / 11.11.2026 / A @ B", None)
                .assertions
                .contains(&Assertion::Season(2026, 2027))
        );
    }

    #[test]
    fn a_card_title_gives_its_number_names_and_part() {
        let r = parse(
            "UFC.Fight.Night.293.Cortes-Acosta.vs.Walker.Prelims.1080p.WEB-DL.H264.Fight-BB",
            None,
        );
        assert_eq!(
            (r.series, r.number, r.part),
            (Series::FightNight, Some(293), PartHint::Prelims)
        );
        let s = r.sides.unwrap();
        assert_eq!(s.left, vec!["cortes", "acosta"]);
        assert_eq!(s.right, vec!["walker"]);
        assert_eq!(r.group.as_deref(), Some("bb"));
        let r = parse(
            "UFC.Fight.Night.234.Ankalaev.vs.Walker.2.WEB-DL.H264.Fight-BB",
            None,
        );
        assert_eq!((r.rematches.clone(), r.part), (vec![2], PartHint::None));
        assert_eq!(r.sides.unwrap().right, vec!["walker"]);
        let r = parse(
            "UFC 333 Volkanovski Vs Evloev 1080P 60FPS Full Event WEB-DL AAC2.0. X265 POOTLED",
            None,
        );
        assert_eq!(
            (r.series, r.number, r.part, r.fps),
            (Series::Numbered, Some(333), PartHint::Full, Some(60))
        );
        assert_eq!(r.group.as_deref(), Some("pootled"));
        let r = parse(
            "UFC 333&nbsp;Volkanovski Vs Evloev 1080P 60FPS Full Event WEB-DL AAC2.0. X265 POOTLED",
            None,
        );
        assert_eq!(r.number, Some(333));
        assert_eq!(
            parse("UFC.333.Early.Prelims.1080p.WEB-DL.H264.Fight-BB", None).part,
            PartHint::EarlyPrelims
        );
        assert_eq!(
            parse(
                "UFC.333.Volkanovski.vs.Evloev.Main.Card.And.Prelims.1080p.IPTV.MKV/H.265",
                None
            )
            .part,
            PartHint::Full
        );
        assert_eq!(
            parse(
                "UFC.333.Volkanovski.vs.Evloev.Main.Card.1080p.WEB-DL.H264-nVa",
                None
            )
            .part,
            PartHint::Main
        );
    }

    #[test]
    fn a_place_stands_before_the_names() {
        let r = parse(
            "PFL Washington DC Jean vs Rodriguez 2026 07 25 720p WEB-DL H 264-FBB",
            None,
        );
        assert_eq!(r.sides.unwrap().left, vec!["washington", "dc", "jean"]);
        assert_eq!(r.dates, vec![d(2026, 7, 25)]);
        let r = parse("PFL Tampa: Cyborg vs. Vieira", None);
        assert_eq!(r.head, vec!["tampa"]);
        assert_eq!(r.sides.unwrap().left, vec!["cyborg"]);
        let r = parse("PFL MENA 9 2026 720p WEB H 264-JFF", None);
        assert_eq!(
            (r.series, r.number, r.head.clone()),
            (Series::Named, Some(9), vec!["mena".to_string()])
        );
    }

    #[test]
    fn competitions_are_whole_names_at_the_start() {
        let l = |t: &str| {
            parse(t, None)
                .leagues
                .iter()
                .map(|l| l.slug)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            l("EPL.2019.01.19.Liverpool.vs.Crystal.Palace"),
            vec!["premier-league"]
        );
        assert_eq!(
            l("Guinness Six Nations 2022 / Round1 / Scotland. vs. England"),
            vec!["six-nations"]
        );
        assert_eq!(
            l("Primera Division 2026-2027 / Matchday 10 / 25.10.2026 / Barcelona - Real Madrid"),
            vec!["la-liga"]
        );
        assert!(l("Euroleague 2026-27 / 07.10.2026 / Paris @ Lyon-Villeurbanne").is_empty());
        assert!(
            l("One Championship ONE Fight Night 49 60fps 1080p WEBRip h264-TJ [TJET]").is_empty()
        );
        assert!(
            parse(
                "2 Bundesliga 2026 10 31 FC Kaiserslautern vs Hertha BSC",
                None
            )
            .qualified
        );
        assert!(parse("Frauen-Bundesliga 2026 11 09 Koln vs Leipzig", None).qualified);
        // "Women's" stands before the name, so it is a qualified one and a women's one alike.
        assert!(
            parse(
                "BBC.Women's.Six.Nations.2026.05.17.Ireland.v.Scotland.1080i.HDTV",
                None
            )
            .qualified
        );
        assert!(
            parse(
                "BBC.Women's.Six.Nations.2026.05.17.Ireland.v.Scotland.1080i.HDTV",
                None
            )
            .women
        );
        assert!(
            parse(
                "Aoashi S02E03 Premier League Match 7-Kashiwa Dai Business School 1080p",
                None
            )
            .qualified
        );
    }

    #[test]
    fn shows_games_and_episodes_are_marked() {
        let r = parse(
            "UFC 334 Embedded-Vlog Series-Episode 2 1080p WEBRip h264-TJ [TJET]",
            None,
        );
        assert!(r.shoulder.contains(&"embedded") && r.shoulder.contains(&"vlog"));
        assert!(
            parse("UFC 334 Countdown 720p", None)
                .shoulder
                .contains(&"countdown")
        );
        assert!(
            parse("UFC 334 Weigh-In", None)
                .shoulder
                .contains(&"weigh-in")
        );
        assert!(
            parse("NBA 2K27 Update v2 1-RUNE", None)
                .non_event
                .contains(&"video game")
        );
        assert!(
            parse(
                "PGA TOUR 2K25 (v2026.03.27 & All DLCs) (Hypervisor|DenuvOwO)",
                None
            )
            .non_event
            .contains(&"video game")
        );
        assert!(
            parse("Tom Scott England 2026 S01E31 1080p NBLA WEB-DL", None)
                .non_event
                .contains(&"episode")
        );
        assert!(
            parse("Match Of The Day 2026 11 08 1080p HEVC x265-MeGusta", None)
                .shoulder
                .contains(&"highlights show")
        );
    }

    #[test]
    fn rounds_compare_like_with_like() {
        let r = |w: &str| round_of(&title::tokens(w)).unwrap();
        use Standing::{Different, Equal, Incomparable};
        assert_eq!(r("Round 1").standing(&r("Round 2")), Different);
        assert_eq!(r("Round 1").standing(&r("Round1")), Equal);
        assert_eq!(r("Round 1").standing(&r("Matchweek 9")), Incomparable);
        assert_eq!(r("Grand Final").standing(&r("Final")), Equal);
        assert_eq!(r("Semi-final").standing(&r("Final")), Different);
        assert_eq!(r("Final").standing(&r("Matchweek 9")), Incomparable);
        assert_eq!(r("Game 7").standing(&r("Game 7")), Equal);
        assert_eq!(r("Group B").standing(&r("Group C")), Different);
    }
}
