//! The schedule source: ESPN's public scoreboard feed. It is unofficial and has no key, so the
//! client is deliberately gentle: one request at a time with a gap between them, a short
//! cache, a pause after any failure that doubles while failures continue, and a plain `Arr`
//! user agent.
//!
//! Only fixtures are read. The feed's answers carry scores, winners and result text; the
//! structs below have no field for any of it, so none of it can reach the store or an answer
//! (see "No spoilers" in docs/API.md). Tests keep that true.

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

/// Words and shapes that tell how a contest went. Matched as whole words (the edges are any
/// character that is not a letter or digit, so "def." and "Decker" are told apart); "champion"
/// is here but "Championship" is not, because a tournament's name may say that and a result
/// ("X, champion") may not.
fn outcome_like(t: &str) -> bool {
    static RESULT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let result = RESULT.get_or_init(|| {
        regex::Regex::new(concat!(
            r"(?i)\d{1,3}\s*[-\u{2013}\u{2014}:]\s*\d{1,3}",
            r"|(?:^|[^\p{L}\p{N}])(?:def|dec|defeats?|defeated|beats?|wins?|won|win over|lost to|lost",
            r"|loses?|draws?|drew|ko|tko|submission|submitted|decision|forfeit(?:ed)?|walkover",
            r"|retired|retires|champion|crowned|clinch(?:es|ed)?|victory|victorious|knocked out",
            r"|stoppage)(?:[^\p{L}\p{N}]|$)"
        ))
        .expect("static pattern")
    });
    result.is_match(t)
}

/// An event's title from the feed, for events with no two sides (a fight card, a tournament).
/// It is free text, so it must fit a plain grammar: a heading (letters, digits, spaces and
/// `' & . - + / ( ) # ,`), optionally followed by `: <Name> vs. <Name>` (`vs`, `vs.` or `v`),
/// and must read like neither a score nor an outcome. A title that does not is dropped; the
/// event is kept and named from its league and date.
pub fn event_title(text: Option<&str>) -> Option<String> {
    static GRAMMAR: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let t = clean(text, 200)?;
    let grammar = GRAMMAR.get_or_init(|| {
        regex::Regex::new(concat!(
            r"(?i)^[\p{L}\p{N} '\u{2019}&.\-+/()#,]{2,120}",
            r"(?::\s[\p{L}\p{N} '\u{2019}.\-]{1,40}\s(?:vs\.?|v)\s[\p{L}\p{N} '\u{2019}.\-]{1,40})?$"
        ))
        .expect("static pattern")
    });
    (grammar.is_match(&t) && !outcome_like(&t)).then_some(t)
}

/// A venue's name from the structured venue field: plain characters, nothing that reads like
/// a result.
fn venue_name(text: Option<&str>) -> Option<String> {
    let t = clean(text, 120)?;
    let plain = t
        .chars()
        .all(|c| c.is_alphanumeric() || " '\u{2019}&.-+/()#,".contains(c));
    (plain && !outcome_like(&t)).then_some(t)
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

fn side(t: &RawTeam) -> Option<Side> {
    Some(Side {
        id: plain_id(t.id.as_ref())?,
        name: clean(t.display_name.as_deref().or(t.name.as_deref()), 120)?,
        abbreviation: clean(t.abbreviation.as_deref(), 12)
            .filter(|a| a.chars().all(|c| c.is_alphanumeric())),
    })
}

fn fixture(e: RawEvent) -> Option<Fixture> {
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
        _ => (event_title(e.name.as_deref()), None),
    };
    let venue = comp
        .and_then(|c| c.venue.as_ref())
        .or(e.venues.first())
        .and_then(|v| venue_name(v.full_name.as_deref()));
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
        season: e.season.and_then(|s| plain_id_or_text(s.year.as_ref())),
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

fn plain_id_or_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => clean(Some(s), 20),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The fixtures of a scoreboard answer. Events without a usable id or start time are left out.
pub fn parse_month(body: &[u8]) -> Result<Month, UpstreamError> {
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
        match fixture(e) {
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
                short_name: clean(t.short_display_name.as_deref(), 60),
                abbreviation: clean(t.abbreviation.as_deref(), 20),
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
        path: &str,
        year: i32,
        month: u8,
    ) -> Result<Arc<Month>, UpstreamError> {
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
            |b| parse_month(b).map(Arc::new),
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

    fn state(name: &str, st: &str) -> EventState {
        let body = format!(
            r#"{{"events":[{{"id":"7","date":"2026-11-21T12:30Z","name":"A at B",
               "status":{{"type":{{"name":"{name}","state":"{st}"}}}}}}]}}"#
        );
        parse_month(body.as_bytes()).unwrap().fixtures[0].state
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
            parse_month(br#"{"events":[{"id":"7","date":"2026-11-21T12:30Z","name":"A at B"}]}"#)
                .unwrap();
        assert_eq!(m.fixtures[0].state, EventState::Scheduled);
    }

    #[test]
    fn sides_and_dates_are_read_defensively() {
        // No home/away flags: the feed's order stands. Individual sports have no sides.
        let m = parse_month(
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
        assert_eq!(m.fixtures[2].name.as_deref(), Some("Odd name"));
        assert!(!m.truncated);
        assert!(parse_month(b"not json").is_err());
    }
}

#[cfg(test)]
mod title_tests {
    use super::*;

    const SPOILERS: [&str; 16] = [
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
    ];

    #[test]
    fn titles_that_tell_how_it_went_are_refused() {
        for t in SPOILERS {
            assert_eq!(event_title(Some(t)), None, "{t}");
        }
        for t in [
            "UFC 300: Smith vs. Jones",
            "UFC: Smith def. Jones",
            "UFC: Smith vs. Jones, KO",
        ] {
            assert_eq!(
                event_title(Some(t)).is_some(),
                t == "UFC 300: Smith vs. Jones",
                "{t}"
            );
        }
        // Not the grammar of a title at all.
        for t in [
            "Smith vs. Jones: Jones vs. Smith: more",
            "A!",
            "x",
            "<b>Open</b>",
            "Open = 3",
        ] {
            assert_eq!(event_title(Some(t)), None, "{t}");
        }
        assert_eq!(event_title(None), None);
    }

    #[test]
    fn real_titles_and_names_that_only_contain_trigger_letters_pass() {
        for t in [
            "UFC Fight Night: Cortes Acosta vs. Walker",
            "UFC 334: Gane vs. Hokit",
            "UFC Fight Night: Proch\u{e1}zka vs. Stirling",
            "PFL Dubai: Nemkov vs. Bilostenniy",
            "UFC 300",
            "Abu Dhabi Aldar Championship",
            "DP World Tour Championship",
            "BMW Australian PGA Championship",
            "Tour Championship",
            "Masters Tournament",
            "The Open",
            "U.S. Open",
            "3M Open",
            "Rocket Classic",
            "Zurich Classic of New Orleans",
            "THE CJ CUP Byron Nelson",
            "the Memorial Tournament pres. by Workday",
            "Charles Schwab Challenge",
            "Genesis Scottish Open",
            "UFC Fight Night: Decker vs. Winston",
            "UFC Fight Night: Kowalski vs. Koch",
            "UFC Fight Night: Tkachenko v Drewry",
            "UFC Fight Night: Beaton vs. Windsor",
            "Wincanton Winter Classic",
            "Kokomo Open",
            "Wonderland Invitational",
        ] {
            assert_eq!(event_title(Some(t)).as_deref(), Some(t), "{t}");
        }
        for v in [
            "Etihad Stadium",
            "Meta APEX",
            "Stade de France",
            "St. Mary's Stadium",
            "Wincanton Arena",
            "Decker Park",
        ] {
            assert_eq!(venue_name(Some(v)).as_deref(), Some(v), "{v}");
        }
        for v in [
            "Jones wins by KO",
            "3-1",
            "Smith def. Jones",
            "Arena <3",
            "Smith lost to Jones",
        ] {
            assert_eq!(venue_name(Some(v)), None, "{v}");
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
        parse_month(body.as_bytes()).unwrap().fixtures.remove(0)
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
    fn a_card_or_tournament_keeps_a_good_title_and_loses_a_bad_one_not_the_event() {
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
}
