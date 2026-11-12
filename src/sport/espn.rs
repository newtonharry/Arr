//! The schedule source: ESPN's public scoreboard feed. It is unofficial and has no key, so the
//! client is deliberately gentle: one request at a time with a gap between them, a short
//! cache, a pause after any failure that doubles while failures continue, and a plain `Arr`
//! user agent.
//!
//! Only fixtures are read. The feed's answers carry scores, winners and result text; the
//! structs below have no field for any of it, so none of it can reach the store or an answer
//! (see "No spoilers" in docs/API.md). Tests keep that true.

use super::catalog::{League, Sport};
use crate::config::SportsConfig;
use crate::upstream::{self, TtlCache, UpstreamError};
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// The most a month's answer may be: a busy basketball month is about 4 MB.
const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Asked for on every month request; the feed's default of 100 cuts a busy month short.
pub const PAGE_LIMIT: usize = 400;
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
const MAX_TEAMS: usize = 400;

/// What happens to an event, in the only four ways Arr tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventState {
    Scheduled,
    Postponed,
    Cancelled,
    /// It has been played. Nothing about how it went is kept.
    Played,
}

impl EventState {
    pub fn as_str(self) -> &'static str {
        match self {
            EventState::Scheduled => "scheduled",
            EventState::Postponed => "postponed",
            EventState::Cancelled => "cancelled",
            EventState::Played => "played",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    pub id: String,
    pub name: String,
    /// The team's abbreviation ("MNC"), letters and digits only.
    pub abbreviation: Option<String>,
}

/// One fixture, as far as Arr cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub id: String,
    pub starts_at: OffsetDateTime,
    /// Set for multi-day events (a golf tournament).
    pub ends_at: Option<OffsetDateTime>,
    pub season: Option<String>,
    /// `Away at Home` built from the two teams, or a title that passed `event_title`. `None`
    /// when a title failed the checks: the store then names the event from its league and date.
    pub name: Option<String>,
    pub short_name: Option<String>,
    pub home: Option<Side>,
    pub away: Option<Side>,
    pub round: Option<String>,
    pub venue: Option<String>,
    pub state: EventState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamInfo {
    pub id: String,
    pub name: String,
    pub short_name: Option<String>,
    pub abbreviation: Option<String>,
}

/// A month's fixtures.
#[derive(Debug, Clone)]
pub struct Month {
    pub fixtures: Vec<Fixture>,
    /// The answer reached the page limit, so it may be missing events.
    pub truncated: bool,
    /// Records that could not be read as fixtures (a broken date, say). Their ids, when they
    /// had a usable one, count as seen: an event whose record is broken is not an absent one.
    pub rejected: usize,
    pub rejected_ids: Vec<String>,
}

impl Month {
    /// Whether this answer may be taken to list everything the league has in its month: it
    /// was not cut at the page limit, it was not empty, and every record could be read.
    pub fn complete(&self) -> bool {
        !self.truncated && self.rejected == 0 && !self.fixtures.is_empty()
    }
}

// ---- the parts of the feed that are read. No field here may hold a score or a result. ----

#[derive(Deserialize)]
struct RawBoard {
    /// Required: an answer without a list of events is not a schedule.
    events: Option<Vec<RawEvent>>,
}

#[derive(Deserialize)]
struct RawEvent {
    id: Option<Value>,
    date: Option<String>,
    #[serde(rename = "endDate")]
    end_date: Option<String>,
    name: Option<String>,
    season: Option<RawSeason>,
    status: Option<RawStatus>,
    #[serde(default)]
    venues: Vec<RawVenue>,
    #[serde(default)]
    competitions: Vec<RawCompetition>,
}

#[derive(Deserialize)]
struct RawSeason {
    year: Option<Value>,
}

#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "type")]
    kind: Option<RawStatusType>,
}

#[derive(Deserialize)]
struct RawStatusType {
    name: Option<String>,
    state: Option<String>,
}

#[derive(Deserialize)]
struct RawVenue {
    #[serde(rename = "fullName")]
    full_name: Option<String>,
}

#[derive(Deserialize)]
struct RawCompetition {
    status: Option<RawStatus>,
    venue: Option<RawVenue>,
    #[serde(default)]
    notes: Vec<RawNote>,
    #[serde(default)]
    competitors: Vec<RawCompetitor>,
}

#[derive(Deserialize)]
struct RawNote {
    headline: Option<String>,
}

#[derive(Deserialize)]
struct RawCompetitor {
    #[serde(rename = "homeAway")]
    home_away: Option<String>,
    team: Option<RawTeam>,
}

#[derive(Deserialize)]
struct RawTeam {
    id: Option<Value>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "shortDisplayName")]
    short_display_name: Option<String>,
    abbreviation: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct RawTeams {
    /// Required: an answer without it is not a team list. An empty one is well formed.
    sports: Option<Vec<RawTeamsSport>>,
}

#[derive(Deserialize)]
struct RawTeamsSport {
    #[serde(default)]
    leagues: Vec<RawTeamsLeague>,
}

#[derive(Deserialize)]
struct RawTeamsLeague {
    #[serde(default)]
    teams: Vec<RawTeamEntry>,
}

#[derive(Deserialize)]
struct RawTeamEntry {
    team: Option<RawTeam>,
}

/// Text from the feed, kept short and plain.
fn clean(text: Option<&str>, max: usize) -> Option<String> {
    let t: String = text?
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if t.is_empty() {
        None
    } else {
        Some(t.chars().take(max).collect())
    }
}

// ---- What may be taken from the feed's text ("No results", ARCHITECTURE.md) ----
//
// Every string comes in through a positive rule for its kind (a name, a title, an
// abbreviation, a season, a round) and, on top of that, the reject list below. What does not fit
// is replaced by something built from Arr's own data or dropped; the event or team stays.

/// Words and shapes that tell how a contest went. Matched as whole words (the edges are any
/// character that is not a letter or digit, so "def." and "Decker" are told apart); "champion"
/// is here but "Championship" is not, because a tournament's name may say that and a result
/// ("X, champion") may not.
fn outcome_like(t: &str) -> bool {
    static RESULT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let result = RESULT.get_or_init(|| {
        regex::Regex::new(concat!(
            r"(?i)\d{1,3}\s*[-\u{2013}\u{2014}:]\s*\d{1,3}",
            r"|(?:^|[^\p{L}\p{N}])(?:def|dec|defeats?|defeated|beats?|wins?|winners?|won|win over|lost to|lost",
            r"|loses?|draws?|drew|ko|tko|submission|submits?|submitted|taps?|tapped|decision|forfeit(?:ed)?",
            r"|walkover|retired|retires|champion|crowned|clinch(?:es|ed)?|victory|victorious",
            r"|knocked out|knocks out|stoppage|stops?|stopped|outpoints?|finishes|eliminated|advances",
            r"|upset|stuns?|stunned|edges?|tops|routs?|sweeps?)(?:[^\p{L}\p{N}]|$)"
        ))
        .expect("static pattern")
    });
    result.is_match(t)
}

/// (Parentheses are allowed too: a real venue is "Toyota Center (Houston)".)
const NAME_PUNCT: &str = " .'\u{2019}&-/()";

/// Small lowercase words that may sit inside a name ("Atl\u{e9}tico de Madrid", "Stade de France").
const NAME_PARTICLES: [&str; 23] = [
    "de", "da", "do", "dos", "das", "del", "della", "di", "du", "van", "von", "der", "den", "la",
    "le", "el", "al", "of", "the", "and", "y", "e", "&",
];
/// More of them for venues ("Parc des Princes", "Stadio delle Alpi", "Stadion im Sportpark").
const NAME_PARTICLES_MORE: [&str; 7] = ["des", "dei", "delle", "degli", "zu", "am", "im"];

/// One word of a name. It must not start with a lowercase letter: a capital, a digit ("76ers",
/// "1."), a letter of a script without case, or a bare symbol all fit, as do the particles above
/// and the apostrophe form "d'Ivoire" (a letter or two, an apostrophe, a capital). Mixed case
/// inside the word is fine ("McLaren"). Parentheses around a word are ignored.
fn name_word(raw: &str) -> bool {
    let t = raw.trim_matches(|c| c == '(' || c == ')');
    if !t.chars().any(char::is_alphanumeric) {
        return true;
    }
    if NAME_PARTICLES.contains(&t) || NAME_PARTICLES_MORE.contains(&t) {
        return true;
    }
    let mut chars = t.chars();
    let first = chars.next().unwrap_or(' ');
    if !first.is_lowercase() {
        return true;
    }
    // A sponsor's web address ("crypto.com Arena").
    if let Some((host, tld)) = t.rsplit_once('.')
        && ["com", "net", "org", "io", "tv", "co"].contains(&tld)
        && !host.is_empty()
        && host.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return true;
    }
    // d'Ivoire, l'Equipe
    let lead = t.chars().take_while(|c| c.is_lowercase()).count();
    let rest: Vec<char> = t.chars().skip(lead).collect();
    (1..=2).contains(&lead)
        && matches!(rest.first(), Some('\'' | '\u{2019}'))
        && rest.get(1).is_some_and(|c| c.is_uppercase())
}

/// A name (a team, a venue, a short name): letters of any script, digits, spaces and
/// `. ' & - / ( )` only, bounded, every word of the kind `name_word` accepts (so lowercase
/// prose is not a name), reading like neither a score nor an outcome.
pub fn plain_name(text: Option<&str>, max: usize) -> Option<String> {
    let t = clean(text, max + 1)?;
    let ok = t.chars().count() <= max
        && t.chars()
            .all(|c| c.is_alphanumeric() || NAME_PUNCT.contains(c))
        && t.split(' ').all(name_word)
        && !outcome_like(&t);
    ok.then_some(t)
}

/// A team abbreviation: up to six letters, digits or `&`.
fn abbreviation(text: Option<&str>) -> Option<String> {
    let t = clean(text, 7)?;
    let ok = t.chars().count() <= 6
        && t.chars().all(|c| c.is_alphanumeric() || c == '&')
        && !outcome_like(&t);
    ok.then_some(t)
}

/// A season as a year (`2026`) or a span of years (`2026-27`, `2026-2027`).
fn season_label(v: Option<&Value>) -> Option<String> {
    static SEASON: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let t = match v? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let season = SEASON
        .get_or_init(|| regex::Regex::new(r"^\d{4}(?:-\d{2}|-\d{4})?$").expect("static pattern"));
    season.is_match(&t).then_some(t)
}

/// A word of a name or title: starts with a capital letter or a digit, then letters, digits
/// and `' . - &` (so "McIlroy", "AT&T", "3M", "Pro-Am", "U.S." fit; "wins" does not).
fn title_token(t: &str) -> bool {
    let mut c = t.chars();
    c.next()
        .is_some_and(|f| f.is_uppercase() || f.is_ascii_digit())
        && t.chars().count() <= 24
        && t.chars()
            .all(|c| c.is_alphanumeric() || "'\u{2019}.-&".contains(c))
}

/// Small words that may sit in a tournament title without a capital.
const CONNECTORS: [&str; 14] = [
    "the",
    "of",
    "in",
    "at",
    "by",
    "and",
    "de",
    "del",
    "la",
    "presented",
    "pres.",
    "for",
    "on",
    "&",
];

/// A tournament's title: every word is capitalised, all capitals, a number or a connector.
fn tournament_title(t: &str) -> bool {
    let tokens: Vec<&str> = t.split(' ').collect();
    t.chars().count() <= 80
        && tokens.len() <= 12
        && tokens.iter().any(|w| title_token(w))
        && tokens
            .iter()
            .all(|w| title_token(w) || CONNECTORS.contains(w))
}

/// Particles that may sit inside a fighter's name.
const PARTICLES: [&str; 14] = [
    "de", "da", "do", "dos", "das", "del", "di", "van", "von", "der", "den", "la", "le", "el",
];

/// One side of `A vs. B`: one to four name tokens (capitalised, letters with `' . -`), a
/// particle after the first, an optional trailing numeral.
fn fighter(tokens: &[&str]) -> bool {
    if tokens.is_empty() || tokens.len() > 4 {
        return false;
    }
    tokens.iter().enumerate().all(|(i, w)| {
        let last = i + 1 == tokens.len();
        let letters = w
            .chars()
            .all(|c| c.is_alphabetic() || "'\u{2019}.-".contains(c));
        (title_token(w) && letters)
            || (i > 0 && PARTICLES.contains(w))
            || (i > 0 && last && w.len() <= 2 && w.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// A fight card's title: it starts with one of the league's own names (UFC, PFL, ...), then a
/// small tail (numbers, "Fight Night", "on ESPN", up to five capitalised words), then
/// optionally `: <Name> vs. <Name>` and nothing else.
fn card_title(t: &str, league: &League) -> bool {
    let (head, names) = match t.split_once(": ") {
        Some((h, n)) => (h, Some(n)),
        None => (t, None),
    };
    let lower = head.to_lowercase();
    let prefix = std::iter::once(league.name)
        .chain(league.aliases.iter().copied())
        .map(str::to_lowercase)
        .filter(|p| lower == *p || lower.starts_with(&format!("{p} ")))
        .map(|p| p.len())
        .max();
    let Some(prefix) = prefix else {
        return false;
    };
    let tail: Vec<&str> = head[prefix..].split_whitespace().collect();
    let mut words = 0;
    let tail_ok = tail.len() <= 8
        && tail.iter().enumerate().all(|(i, w)| {
            let numeral = w.len() <= 4 && w.bytes().all(|b| b.is_ascii_digit());
            let network = w.len() <= 6
                && w.chars().next().is_some_and(char::is_uppercase)
                && w.chars()
                    .all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '+');
            let word = title_token(w)
                && w.chars()
                    .all(|c| c.is_alphabetic() || "'\u{2019}.-".contains(c));
            if word && !network {
                words += 1;
            }
            numeral || network || (*w == "on" && i + 1 < tail.len()) || word
        })
        && words <= 5;
    if !tail_ok {
        return false;
    }
    match names {
        None => true,
        Some(n) => {
            let tokens: Vec<&str> = n.split(' ').collect();
            let sep: Vec<usize> = tokens
                .iter()
                .enumerate()
                .filter(|(_, w)| ["vs", "vs.", "v"].contains(w))
                .map(|(i, _)| i)
                .collect();
            match sep.as_slice() {
                [i] => fighter(&tokens[..*i]) && fighter(&tokens[i + 1..]),
                _ => false,
            }
        }
    }
}

/// An event's title from the feed, for events with no two sides. Free text, so it must fit the
/// grammar of its kind (a fight card for MMA leagues, a tournament title for everything else)
/// and pass the reject list. A title that does not is dropped; the event is kept and named from
/// its league and date.
pub fn event_title(text: Option<&str>, league: &League) -> Option<String> {
    let t = clean(text, 200)?;
    let fits = if league.sport == Sport::Mma {
        card_title(&t, league)
    } else {
        tournament_title(&t)
    };
    (fits && !outcome_like(&t)).then_some(t)
}

/// An id as the feed writes it, a string or a number, kept only when it is a plain number
/// without leading zeros (so it is safe inside an Arr id).
fn plain_id(v: Option<&Value>) -> Option<String> {
    let s = match v? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let ok = !s.is_empty()
        && s.len() <= 18
        && s.bytes().all(|b| b.is_ascii_digit())
        && !s.starts_with('0');
    ok.then_some(s)
}

/// The feed writes `2026-11-21T12:30Z` (no seconds); a full RFC 3339 time is accepted too.
pub fn parse_time(s: &str) -> Option<OffsetDateTime> {
    use time::format_description::well_known::Rfc3339;
    let short = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]Z");
    time::PrimitiveDateTime::parse(s.trim(), &short)
        .ok()
        .map(time::PrimitiveDateTime::assume_utc)
        .or_else(|| OffsetDateTime::parse(s.trim(), &Rfc3339).ok())
}

fn state_of(status: Option<&RawStatus>) -> EventState {
    let Some(t) = status.and_then(|s| s.kind.as_ref()) else {
        return EventState::Scheduled;
    };
    let name = t.name.as_deref().unwrap_or("").to_ascii_uppercase();
    if name.contains("POSTPONED") {
        EventState::Postponed
    } else if name.contains("CANCEL") {
        EventState::Cancelled
    } else if t.state.as_deref() == Some("post") {
        EventState::Played
    } else {
        // Not started, or under way: still to be played as far as a schedule goes.
        EventState::Scheduled
    }
}

/// A team as it is used in fixtures and team lists. A name that does not fit the name rule
/// becomes `Team <id>`: the team stays, whatever the feed wrote.
fn side(t: &RawTeam) -> Option<Side> {
    let id = plain_id(t.id.as_ref())?;
    let name = plain_name(t.display_name.as_deref().or(t.name.as_deref()), 120)
        .unwrap_or_else(|| format!("Team {id}"));
    Some(Side {
        id,
        name,
        abbreviation: abbreviation(t.abbreviation.as_deref()),
    })
}

fn fixture(e: RawEvent, league: &League) -> Option<Fixture> {
    let id = plain_id(e.id.as_ref())?;
    let starts_at = parse_time(e.date.as_deref()?)?;
    let comp = e.competitions.first();
    // The event's own status covers a whole card or tournament; a plain game only has the
    // competition's.
    let state = state_of(e.status.as_ref().or(comp.and_then(|c| c.status.as_ref())));
    let teams: Vec<&RawCompetitor> = comp
        .map(|c| c.competitors.iter().filter(|x| x.team.is_some()).collect())
        .unwrap_or_default();
    let pick = |want: &str, fallback: usize| {
        let by_flag = teams
            .iter()
            .find(|c| c.home_away.as_deref() == Some(want))
            .copied();
        let any_flag = teams.iter().any(|c| c.home_away.is_some());
        by_flag
            .or_else(|| {
                if any_flag {
                    None
                } else {
                    teams.get(fallback).copied()
                }
            })
            .and_then(|c| c.team.as_ref())
            .and_then(side)
    };
    let (home, away) = if teams.len() == 2 {
        (pick("home", 0), pick("away", 1))
    } else {
        (None, None)
    };
    // The name never comes from the feed's own `name` or `shortName` when both teams are
    // known: it is built from the structured team names, away side first ("Fulham at
    // Manchester City", "Stade Toulousain at Lyon"), and the short name from their
    // abbreviations ("FUL @ MNC"). `home` and `away` keep their own meaning.
    let (name, short_name) = match (&home, &away) {
        (Some(h), Some(a)) => (
            Some(format!("{} at {}", a.name, h.name)),
            h.abbreviation
                .as_ref()
                .zip(a.abbreviation.as_ref())
                .map(|(h, a)| format!("{a} @ {h}")),
        ),
        _ => (event_title(e.name.as_deref(), league), None),
    };
    let venue = comp
        .and_then(|c| c.venue.as_ref())
        .or(e.venues.first())
        .and_then(|v| plain_name(v.full_name.as_deref(), 120));
    // Notes are free text and can say how a game went, so they are never copied: only a
    // headline that is exactly a round label is kept.
    let round = comp.and_then(|c| {
        c.notes
            .iter()
            .find_map(|n| round_label(n.headline.as_deref()?))
    });
    Some(Fixture {
        id,
        starts_at,
        ends_at: e
            .end_date
            .as_deref()
            .and_then(parse_time)
            .filter(|end| *end > starts_at),
        season: e.season.and_then(|s| season_label(s.year.as_ref())),
        name,
        short_name,
        home,
        away,
        round,
        venue,
        state,
    })
}

/// A round name from a strict list: "Round 12", "Matchweek 9", "Group B", "Semi-final", ...
/// Anything else (a headline that might name a winner or a score) gives nothing.
pub fn round_label(text: &str) -> Option<String> {
    static NUMBERED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static NAMED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let numbered = NUMBERED.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^(round|matchweek|match week|matchday|match day|gameweek|game week|week|game|leg|round of)\s+(\d{1,3})$",
        )
        .expect("static pattern")
    });
    let named = NAMED.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^(group\s+[a-z0-9]|grand[ -]final|quarter[ -]?finals?|semi[ -]?finals?|finals?|play[ -]?offs?|play[ -]?in|preliminary final|qualifying final|elimination final)$",
        )
        .expect("static pattern")
    });
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let upper_first = |s: &str| {
        let mut c = s.chars();
        c.next()
            .map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase())
            .unwrap_or_default()
    };
    if let Some(c) = numbered.captures(&t) {
        return Some(format!("{} {}", upper_first(&c[1]), &c[2]));
    }
    if named.is_match(&t) {
        if let Some(g) = t.get(..5).filter(|g| g.eq_ignore_ascii_case("group")) {
            return Some(format!(
                "{} {}",
                upper_first(g),
                t[5..].trim().to_uppercase()
            ));
        }
        return Some(upper_first(&t));
    }
    None
}

/// The fixtures of a scoreboard answer. Events without a usable id or start time are left out.
pub fn parse_month(body: &[u8], league: &League) -> Result<Month, UpstreamError> {
    let board: RawBoard = serde_json::from_slice(body).map_err(|e| UpstreamError::BadResponse {
        reason: format!(
            "invalid JSON ({:?} error at line {} column {})",
            e.classify(),
            e.line(),
            e.column()
        ),
    })?;
    let events = board.events.ok_or_else(|| UpstreamError::BadResponse {
        reason: "no list of events in the answer".into(),
    })?;
    let truncated = events.len() >= PAGE_LIMIT;
    let mut seen = std::collections::HashSet::new();
    let (mut fixtures, mut rejected, mut rejected_ids) = (vec![], 0, vec![]);
    for e in events {
        let id = plain_id(e.id.as_ref());
        match fixture(e, league) {
            Some(f) => {
                if seen.insert(f.id.clone()) {
                    fixtures.push(f);
                }
            }
            None => {
                rejected += 1;
                rejected_ids.extend(id);
            }
        }
    }
    Ok(Month {
        fixtures,
        truncated,
        rejected,
        rejected_ids,
    })
}

pub fn parse_teams(body: &[u8]) -> Result<Vec<TeamInfo>, UpstreamError> {
    let raw: RawTeams = serde_json::from_slice(body).map_err(|e| UpstreamError::BadResponse {
        reason: format!(
            "invalid JSON ({:?} error at line {} column {})",
            e.classify(),
            e.line(),
            e.column()
        ),
    })?;
    let mut seen = std::collections::HashSet::new();
    let sports = raw.sports.ok_or_else(|| UpstreamError::BadResponse {
        reason: "no list of sports in the answer".into(),
    })?;
    let teams: Vec<TeamInfo> = sports
        .into_iter()
        .flat_map(|s| s.leagues)
        .flat_map(|l| l.teams)
        .filter_map(|t| t.team)
        .filter_map(|t| {
            let s = side(&t)?;
            Some(TeamInfo {
                id: s.id,
                name: s.name,
                short_name: plain_name(t.short_display_name.as_deref(), 60),
                abbreviation: abbreviation(t.abbreviation.as_deref()),
            })
        })
        .filter(|t| seen.insert(t.id.clone()))
        .take(MAX_TEAMS)
        .collect();
    Ok(teams)
}

struct Gate {
    /// When the last request finished.
    last: Option<Instant>,
    failures: u32,
    blocked_until: Option<Instant>,
}

pub struct Espn {
    base: Url,
    http: reqwest::Client,
    min_interval: Duration,
    backoff: Duration,
    /// Held across a request: one at a time, and a caller that waited finds the answer of the
    /// one ahead of it in the cache instead of asking again.
    gate: tokio::sync::Mutex<Gate>,
    months: TtlCache<String, Arc<Month>>,
    teams: TtlCache<String, Arc<Vec<TeamInfo>>>,
}

impl Espn {
    pub fn new(cfg: &SportsConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .connect_timeout(cfg.timeout.min(Duration::from_secs(10)))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("Arr")
            .build()
            .expect("static reqwest configuration is valid");
        Espn {
            base: cfg.base_url.clone(),
            http,
            min_interval: cfg.min_interval,
            backoff: cfg.backoff,
            gate: tokio::sync::Mutex::new(Gate {
                last: None,
                failures: 0,
                blocked_until: None,
            }),
            months: TtlCache::new(cfg.cache, 64),
            // A team list changes rarely: kept twelve times as long (an hour by default).
            teams: TtlCache::new(cfg.cache.saturating_mul(12), 64),
        }
    }

    fn url(&self, path: &str, tail: &str, query: &[(&str, &str)]) -> Result<Url, UpstreamError> {
        let mut url =
            self.base
                .join(&format!("{path}/{tail}"))
                .map_err(|_| UpstreamError::BadResponse {
                    reason: "invalid path".into(),
                })?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    /// One request under the gate. `recheck` finds what whoever was ahead of us published;
    /// `parse` turns the body into the answer and `publish` stores it, both while the gate is
    /// still held, so the request counts as a success only if the answer is usable and a
    /// caller behind us always finds it. `Err(Throttled)` without any request while a pause
    /// after a failure is running.
    async fn fetch<T>(
        &self,
        url: Url,
        recheck: impl Fn() -> Option<T>,
        parse: impl FnOnce(&[u8]) -> Result<T, UpstreamError>,
        publish: impl FnOnce(&T),
    ) -> Result<T, UpstreamError> {
        let mut gate = self.gate.lock().await;
        if let Some(found) = recheck() {
            return Ok(found);
        }
        if let Some(until) = gate.blocked_until
            && until > Instant::now()
        {
            return Err(UpstreamError::Throttled {
                reason: "the schedule source failed a moment ago; waiting before asking again"
                    .into(),
            });
        }
        if let Some(last) = gate.last {
            let wait = self.min_interval.saturating_sub(last.elapsed());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        let result: Result<T, (UpstreamError, Option<Duration>)> = async {
            let resp = upstream::send(self.http.get(url).header("Accept", "application/json"))
                .await
                .map_err(|e| (e, None))?;
            let retry =
                upstream::retry_after(resp.headers()).filter(|_| resp.status().as_u16() == 429);
            let body = upstream::read_bytes(resp, MAX_BYTES)
                .await
                .map_err(|e| (e, retry))?;
            parse(&body).map_err(|e| (e, None))
        }
        .await;
        gate.last = Some(Instant::now());
        match result {
            Ok(answer) => {
                publish(&answer);
                gate.failures = 0;
                gate.blocked_until = None;
                Ok(answer)
            }
            Err((e, retry)) => {
                gate.failures = gate.failures.saturating_add(1);
                let doubled = self
                    .backoff
                    .saturating_mul(1u32 << gate.failures.saturating_sub(1).min(10))
                    .min(MAX_BACKOFF);
                let pause = retry.map_or(doubled, |r| r.max(doubled).min(MAX_BACKOFF));
                gate.blocked_until = (!pause.is_zero()).then(|| Instant::now() + pause);
                Err(e)
            }
        }
    }

    /// The fixtures of one calendar month of a league (the feed's days are the American ones,
    /// so callers ask for the month on either side of a boundary they care about).
    pub async fn month(
        &self,
        league: &'static League,
        year: i32,
        month: u8,
    ) -> Result<Arc<Month>, UpstreamError> {
        let path = league.espn;
        let key = format!("{path}:{year}{month:02}");
        if let Some(hit) = self.months.get(&key) {
            return Ok(hit);
        }
        let dates = format!("{year}{month:02}");
        let limit = PAGE_LIMIT.to_string();
        let url = self.url(path, "scoreboard", &[("dates", &dates), ("limit", &limit)])?;
        self.fetch(
            url,
            || self.months.get(&key),
            |b| parse_month(b, league).map(Arc::new),
            |m| self.months.insert(key.clone(), m.clone()),
        )
        .await
    }

    /// The teams of a league.
    pub async fn teams(&self, path: &str) -> Result<Arc<Vec<TeamInfo>>, UpstreamError> {
        let key = path.to_string();
        if let Some(hit) = self.teams.get(&key) {
            return Ok(hit);
        }
        let url = self.url(path, "teams", &[("limit", "400")])?;
        self.fetch(
            url,
            || self.teams.get(&key),
            |b| parse_teams(b).map(Arc::new),
            |t| self.teams.insert(key.clone(), t.clone()),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pm(body: &[u8]) -> Result<Month, UpstreamError> {
        parse_month(body, crate::sport::catalog::find("premier-league").unwrap())
    }

    fn state(name: &str, st: &str) -> EventState {
        let body = format!(
            r#"{{"events":[{{"id":"7","date":"2026-11-21T12:30Z","name":"A at B",
               "status":{{"type":{{"name":"{name}","state":"{st}"}}}}}}]}}"#
        );
        pm(body.as_bytes()).unwrap().fixtures[0].state
    }

    #[test]
    fn the_feeds_statuses_become_the_four_states() {
        assert_eq!(state("STATUS_SCHEDULED", "pre"), EventState::Scheduled);
        assert_eq!(state("STATUS_IN_PROGRESS", "in"), EventState::Scheduled);
        assert_eq!(state("STATUS_HALFTIME", "in"), EventState::Scheduled);
        assert_eq!(state("STATUS_POSTPONED", "post"), EventState::Postponed);
        assert_eq!(state("STATUS_CANCELED", "post"), EventState::Cancelled);
        assert_eq!(state("STATUS_FINAL", "post"), EventState::Played);
        assert_eq!(state("STATUS_FULL_TIME", "post"), EventState::Played);
        assert_eq!(state("STATUS_FORFEIT", "post"), EventState::Played);
        // No status at all reads as still to come.
        let m =
            pm(br#"{"events":[{"id":"7","date":"2026-11-21T12:30Z","name":"A at B"}]}"#).unwrap();
        assert_eq!(m.fixtures[0].state, EventState::Scheduled);
    }

    #[test]
    fn sides_and_dates_are_read_defensively() {
        // No home/away flags: the feed's order stands. Individual sports have no sides.
        let m = pm(
            br#"{"events":[
              {"id":"1","date":"2026-11-21T12:30:00Z","name":"X vs Y","competitions":[{"competitors":[
                {"team":{"id":"5","displayName":"X"}},{"team":{"id":"6","displayName":"Y"}}]}]},
              {"id":"2","date":"2026-11-21T12:30Z","endDate":"2026-11-25T12:30Z","name":"A Open","competitions":[{"competitors":[
                {"athlete":{"fullName":"Someone"},"score":"-12"}]}]},
              {"id":"3","date":"2026-11-21T12:30Z","name":"  Odd \n name  ","competitions":[]}]}"#,
        )
        .unwrap();
        assert_eq!(m.fixtures.len(), 3);
        assert_eq!(m.fixtures[0].home.as_ref().unwrap().name, "X");
        assert_eq!(m.fixtures[0].away.as_ref().unwrap().name, "Y");
        assert!(m.fixtures[1].home.is_none() && m.fixtures[1].away.is_none());
        assert!(m.fixtures[1].ends_at.unwrap() > m.fixtures[1].starts_at);
        // Lowercase prose is not a title: the event stays, unnamed here.
        assert_eq!(m.fixtures[2].name, None);
        assert!(!m.truncated);
        assert!(pm(b"not json").is_err());
    }
}

#[cfg(test)]
mod title_tests {
    use super::*;
    use crate::sport::catalog::find;
    use serde_json::json;

    fn league(slug: &str) -> &'static League {
        find(slug).unwrap()
    }

    /// Titles and names that tell how it went, or are prose.
    const SPOILERS: [&str; 24] = [
        "Smith def. Jones",
        "Smith def Jones",
        "Smith lost to Jones",
        "Jones wins by KO",
        "Jones wins",
        "Smith beat Jones",
        "Smith defeated Jones",
        "Jones won by submission",
        "Smith vs Jones 3-1",
        "UFC 300: Smith vs. Jones (3 - 1)",
        "Smith vs. Jones: TKO",
        "Jones via decision",
        "Smith dec. Jones",
        "Draw",
        "Jones, champion",
        "Smith forfeit",
        "Smith submits Jones",
        "Jones is the winner",
        "UFC 334: Gane defeats Hokit",
        "UFC Jones Wins",
        "UFC Smith Taps Jones",
        "Smith Wins Masters",
        "Jones Stuns Smith",
        "Smith Edges Jones",
    ];

    #[test]
    fn a_fight_card_title_must_fit_the_card_grammar() {
        for t in SPOILERS {
            assert_eq!(event_title(Some(t), league("ufc")), None, "{t}");
        }
        // Prose or other shapes that no reject word catches.
        for t in [
            "Smith vs. Jones",
            "UFC: smith vs. jones",
            "UFC 334: Gane vs. Hokit vs. Other",
            "UFC 334: Gane and Hokit",
            "UFC 334: Gane vs. Hokit: more",
            "UFC 334 Gane vs. Hokit",
            "UFC 334: one two three four five vs. Hokit",
            "UFC a b c d e f g h i",
            "UFC is back",
            "Ultimate Fighting Hokit",
            "PFL Dubai: Nemkov vs. Bilostenniy",
        ] {
            assert_eq!(event_title(Some(t), league("ufc")), None, "{t}");
        }
        assert_eq!(event_title(None, league("ufc")), None);
    }

    #[test]
    fn a_tournament_title_must_be_capitalised_words_and_small_connectors() {
        for t in SPOILERS {
            assert_eq!(event_title(Some(t), league("pga-tour")), None, "{t}");
        }
        for t in [
            "the winner takes it all",
            "Smith plays well at the Open",
            "Open golf tournament happening",
            "UFC 334: Gane vs. Hokit",
            "A b",
            "Masters: final",
        ] {
            assert_eq!(event_title(Some(t), league("pga-tour")), None, "{t}");
        }
    }

    #[test]
    fn real_titles_pass_and_trigger_letters_inside_longer_words_do_not_trip() {
        for (slug, t) in [
            ("ufc", "UFC Fight Night: Cortes Acosta vs. Walker"),
            ("ufc", "UFC 334: Gane vs. Hokit"),
            ("ufc", "UFC Fight Night: Proch\u{e1}zka vs. Stirling"),
            ("ufc", "UFC Fight Night: Adesanya vs Magomedov"),
            ("ufc", "UFC 300"),
            ("ufc", "UFC on ESPN 61"),
            ("ufc", "UFC Fight Night 245"),
            ("ufc", "UFC Fight Night Las Vegas"),
            (
                "ufc",
                "UFC Fight Night: Rafael dos Anjos vs. Benoit Saint Denis",
            ),
            ("ufc", "UFC Fight Night: Decker vs. Winston"),
            ("ufc", "UFC Fight Night: Kowalski vs. Koch"),
            ("ufc", "UFC Fight Night: Tkachenko v Drewry"),
            ("ufc", "UFC Fight Night: Beaton vs. O'Malley Jr."),
            ("ufc", "Ultimate Fighting Championship 1"),
            ("pfl", "PFL Dubai: Nemkov vs. Bilostenniy"),
            ("pfl", "PFL Charlotte: Battle vs. Rosta"),
            ("pfl", "PFL Tampa: Cyborg vs. Vieira"),
            ("pfl", "PFL San Diego: McKee vs. Isbulaev"),
            ("pfl", "PFL World Championship"),
            ("dp-world-tour", "Abu Dhabi Aldar Championship"),
            ("dp-world-tour", "DP World Tour Championship"),
            ("dp-world-tour", "BMW Australian PGA Championship"),
            ("pga-tour", "Masters Tournament"),
            ("pga-tour", "The Open"),
            ("pga-tour", "U.S. Open"),
            ("pga-tour", "3M Open"),
            ("pga-tour", "PGA Championship"),
            ("pga-tour", "THE PLAYERS Championship"),
            ("pga-tour", "AT&T Pebble Beach Pro-Am"),
            ("pga-tour", "Sony Open in Hawaii"),
            ("pga-tour", "Zurich Classic of New Orleans"),
            ("pga-tour", "The Sentry"),
            ("pga-tour", "Rocket Classic"),
            ("pga-tour", "Valero Texas Open"),
            ("pga-tour", "THE CJ CUP Byron Nelson"),
            ("pga-tour", "ONEflight Myrtle Beach Classic"),
            ("pga-tour", "Charles Schwab Challenge"),
            ("pga-tour", "Texas Children's Houston Open"),
            ("pga-tour", "the Memorial Tournament presented by Workday"),
            ("pga-tour", "the Memorial Tournament pres. by Workday"),
            ("pga-tour", "Genesis Scottish Open"),
            ("pga-tour", "Wincanton Winter Classic"),
            ("pga-tour", "Cognizant Classic in The Palm Beaches"),
            ("liv-golf", "LIV Golf New York"),
            ("liv-golf", "LIV Golf Team Championship"),
            ("lpga-tour", "Maybank Championship"),
        ] {
            assert_eq!(
                event_title(Some(t), league(slug)).as_deref(),
                Some(t),
                "{slug}: {t}"
            );
        }
    }

    #[test]
    fn names_follow_the_name_rule() {
        for ok in [
            "Etihad Stadium",
            "Meta APEX",
            "Stade de France",
            "St. Mary's Stadium",
            "Brighton & Hove Albion",
            "1. FC Union Berlin",
            "Racing 92",
            "Wincanton Arena",
            "Decker Park",
            "Proch\u{e1}zka Arena",
            "Estadio Jos\u{e9} Zorrilla / Valladolid",
            "Toyota Center (Houston)",
            "Brighton & Hove Albion",
            "1. FC K\u{f6}ln",
            "Paris Saint-Germain",
            "Philadelphia 76ers",
            "Atl\u{e9}tico de Madrid",
            "Borussia M\u{f6}nchengladbach",
            "AS Roma",
            "Stade Toulousain",
            "Union Bordeaux-B\u{e8}gles",
            "ACT Brumbies",
            "Moana Pasifika",
            "Fijian Drua",
            "South Sydney Rabbitohs",
            "Manly Warringah Sea Eagles",
            "St. George Illawarra Dragons",
            "Sydney FC",
            "Western Sydney Wanderers FC",
            "LA Clippers",
            "Estadio Santiago Bernab\u{e9}u",
            "Allianz Arena",
            "St. James' Park",
            "Parc des Princes",
            "Stadio delle Alpi",
            "Stadion im Sportpark",
            "Cote d'Ivoire",
            "Stade de l'Aube",
            "Costa d'Avorio",
            "\u{9e7f}\u{5cf6}\u{30a2}\u{30f3}\u{30c8}\u{30e9}\u{30fc}\u{30ba}",
            "\u{421}\u{43f}\u{430}\u{440}\u{442}\u{430}\u{43a} \u{41c}\u{43e}\u{441}\u{43a}\u{432}\u{430}",
            "McLaren Park",
            "crypto.com Arena",
        ] {
            assert_eq!(plain_name(Some(ok), 120).as_deref(), Some(ok), "{ok}");
        }
        for bad in [
            "Jones wins by KO",
            "3-1",
            "Smith def. Jones",
            "Arena <3",
            "Smith lost to Jones",
            "Team (wins)",
            "smith outclasses jones",
            "Smith outclasses jones",
            "home side cruises",
            "venue where smith won",
            "the winner takes it all",
            "lowercase Name",
            "Stadium of smith",
            "Team A (3-1)",
            "Team; DROP",
            "Stadium, the winner",
            "",
        ] {
            assert_eq!(plain_name(Some(bad), 120), None, "{bad}");
        }
        assert_eq!(plain_name(Some(&"x".repeat(200)), 120), None, "too long");
    }

    #[test]
    fn abbreviations_and_seasons_follow_their_rules() {
        assert_eq!(abbreviation(Some("MNC")).as_deref(), Some("MNC"));
        assert_eq!(abbreviation(Some("AT&T")).as_deref(), Some("AT&T"));
        for bad in ["MNC 3-1 FUL", "TOOLONG1", "M N", "a-b", "DEF", "WIN"] {
            assert_eq!(abbreviation(Some(bad)), None, "{bad}");
        }
        for (v, want) in [
            (json!(2026), Some("2026")),
            (json!("2026"), Some("2026")),
            (json!("2026-27"), Some("2026-27")),
            (json!("2026-2027"), Some("2026-2027")),
            (json!("Smith def. Jones"), None),
            (json!("2026 Smith"), None),
            (json!("26"), None),
            (json!("2026-7"), None),
            (json!("20267"), None),
            (json!(null), None),
        ] {
            assert_eq!(season_label(Some(&v)).as_deref(), want, "{v}");
        }
    }

    fn board(name: &str, short: &str, venue: &str, with_sides: bool) -> Fixture {
        let sides = if with_sides {
            r#",{"homeAway":"home","team":{"id":"5","displayName":"Lyon","abbreviation":"LYO"}},
               {"homeAway":"away","team":{"id":"6","displayName":"Stade Toulousain","abbreviation":"TOU"}}"#
        } else {
            ""
        };
        let body = format!(
            r#"{{"events":[{{"id":"7","date":"2026-11-21T12:30Z","name":"{name}","shortName":"{short}",
               "competitions":[{{"venue":{{"fullName":"{venue}"}},"competitors":[{{"athlete":{{"fullName":"x"}}}}{sides}]}}]}}]}}"#
        );
        parse_month(body.as_bytes(), league("ufc"))
            .unwrap()
            .fixtures
            .remove(0)
    }

    #[test]
    fn a_team_event_is_named_from_its_teams_whatever_the_feed_says() {
        for spoiler in SPOILERS {
            let f = board(spoiler, spoiler, spoiler, true);
            assert_eq!(
                f.name.as_deref(),
                Some("Stade Toulousain at Lyon"),
                "{spoiler}"
            );
            assert_eq!(f.short_name.as_deref(), Some("TOU @ LYO"));
            assert_eq!(f.venue, None, "{spoiler}");
        }
        let f = board("Anything", "Anything", "Stade Gerland", true);
        assert_eq!(f.venue.as_deref(), Some("Stade Gerland"));
        assert_eq!(f.home.as_ref().unwrap().name, "Lyon");
        assert_eq!(f.away.as_ref().unwrap().name, "Stade Toulousain");
    }

    #[test]
    fn a_card_keeps_a_good_title_and_loses_a_bad_one_not_the_event() {
        for spoiler in SPOILERS {
            let f = board(spoiler, spoiler, spoiler, false);
            assert_eq!(f.name, None, "{spoiler}");
            assert_eq!(f.short_name, None);
            assert_eq!(f.venue, None, "{spoiler}");
            assert_eq!(f.id, "7", "the event itself is kept");
        }
        let f = board("UFC 334: Gane vs. Hokit", "UFC 334", "Meta APEX", false);
        assert_eq!(f.name.as_deref(), Some("UFC 334: Gane vs. Hokit"));
        assert_eq!(f.venue.as_deref(), Some("Meta APEX"));
    }

    #[test]
    fn team_fields_that_carry_a_result_become_neutral_in_fixtures_and_lists() {
        let m = parse_month(
            br#"{"events":[{"id":"7","date":"2026-11-21T12:30Z","name":"x","season":{"year":"Smith def. Jones"},
               "competitions":[{"competitors":[
                 {"homeAway":"home","team":{"id":"5","displayName":"Smith def. Jones","abbreviation":"MNC 3-1 FUL"}},
                 {"homeAway":"away","team":{"id":"6","displayName":"Away FC","abbreviation":"AWY"}}]}]}]}"#,
            league("premier-league"),
        )
        .unwrap();
        let f = &m.fixtures[0];
        assert_eq!(f.home.as_ref().unwrap().name, "Team 5");
        assert_eq!(f.name.as_deref(), Some("Away FC at Team 5"));
        assert_eq!(f.short_name, None, "one side has no usable abbreviation");
        assert_eq!(f.season, None);
        let teams = parse_teams(
            br#"{"sports":[{"leagues":[{"teams":[
              {"team":{"id":"5","displayName":"Jones wins by KO","shortDisplayName":"3-1","abbreviation":"3-1"}},
              {"team":{"id":"6","displayName":"Away FC","shortDisplayName":"Away","abbreviation":"AWY"}}]}]}]}"#,
        )
        .unwrap();
        assert_eq!(teams[0].name, "Team 5");
        assert_eq!(
            (teams[0].short_name.clone(), teams[0].abbreviation.clone()),
            (None, None)
        );
        assert_eq!(teams[1].short_name.as_deref(), Some("Away"));
    }
}
