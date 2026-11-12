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
const MONTH_TTL: Duration = Duration::from_secs(5 * 60);
const TEAMS_TTL: Duration = Duration::from_secs(3600);
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
}

/// One fixture, as far as Arr cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub id: String,
    pub starts_at: OffsetDateTime,
    /// Set for multi-day events (a golf tournament).
    pub ends_at: Option<OffsetDateTime>,
    pub season: Option<String>,
    pub name: String,
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
}

// ---- the parts of the feed that are read. No field here may hold a score or a result. ----

#[derive(Deserialize)]
struct RawBoard {
    #[serde(default)]
    events: Vec<RawEvent>,
}

#[derive(Deserialize)]
struct RawEvent {
    id: Option<Value>,
    date: Option<String>,
    #[serde(rename = "endDate")]
    end_date: Option<String>,
    name: Option<String>,
    #[serde(rename = "shortName")]
    short_name: Option<String>,
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
    #[serde(default)]
    sports: Vec<RawTeamsSport>,
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
    let name = clean(e.name.as_deref(), 200).or_else(|| {
        let (h, a) = (home.as_ref()?, away.as_ref()?);
        Some(format!("{} vs {}", h.name, a.name))
    })?;
    let venue = comp
        .and_then(|c| c.venue.as_ref())
        .or(e.venues.first())
        .and_then(|v| clean(v.full_name.as_deref(), 120));
    let round = comp.and_then(|c| {
        c.notes
            .iter()
            .find_map(|n| clean(n.headline.as_deref(), 100))
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
        short_name: clean(e.short_name.as_deref(), 60),
        home,
        away,
        round,
        venue,
        state,
    })
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
    let truncated = board.events.len() >= PAGE_LIMIT;
    let mut seen = std::collections::HashSet::new();
    let fixtures = board
        .events
        .into_iter()
        .filter_map(fixture)
        .filter(|f| seen.insert(f.id.clone()))
        .collect();
    Ok(Month {
        fixtures,
        truncated,
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
    Ok(raw
        .sports
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
        .collect())
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
            months: TtlCache::new(MONTH_TTL, 64),
            teams: TtlCache::new(TEAMS_TTL, 64),
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

    /// One request under the gate. `Err(Throttled)` without any request while a pause after
    /// a failure is running.
    async fn fetch(
        &self,
        url: Url,
        recheck: impl Fn() -> bool,
    ) -> Result<Option<Vec<u8>>, UpstreamError> {
        let mut gate = self.gate.lock().await;
        // Whoever was ahead of us may have fetched exactly this.
        if recheck() {
            return Ok(None);
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
        let result: Result<Vec<u8>, (UpstreamError, Option<Duration>)> = async {
            let resp = upstream::send(self.http.get(url).header("Accept", "application/json"))
                .await
                .map_err(|e| (e, None))?;
            let retry =
                upstream::retry_after(resp.headers()).filter(|_| resp.status().as_u16() == 429);
            upstream::read_bytes(resp, MAX_BYTES)
                .await
                .map_err(|e| (e, retry))
        }
        .await;
        gate.last = Some(Instant::now());
        match result {
            Ok(body) => {
                gate.failures = 0;
                gate.blocked_until = None;
                Ok(Some(body))
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
        let body = self.fetch(url, || self.months.get(&key).is_some()).await?;
        match body {
            None => self.months.get(&key).ok_or(UpstreamError::Unavailable {
                reason: "answer expired".into(),
            }),
            Some(b) => {
                let parsed = Arc::new(parse_month(&b)?);
                self.months.insert(key, parsed.clone());
                Ok(parsed)
            }
        }
    }

    /// The teams of a league.
    pub async fn teams(&self, path: &str) -> Result<Arc<Vec<TeamInfo>>, UpstreamError> {
        let key = path.to_string();
        if let Some(hit) = self.teams.get(&key) {
            return Ok(hit);
        }
        let url = self.url(path, "teams", &[("limit", "400")])?;
        let body = self.fetch(url, || self.teams.get(&key).is_some()).await?;
        match body {
            None => self.teams.get(&key).ok_or(UpstreamError::Unavailable {
                reason: "answer expired".into(),
            }),
            Some(b) => {
                let parsed = Arc::new(parse_teams(&b)?);
                self.teams.insert(key, parsed.clone());
                Ok(parsed)
            }
        }
    }
}
