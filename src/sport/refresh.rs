//! Keeping schedules current, politely.
//!
//! One league costs at most two schedule requests (the calendar months around the window of
//! yesterday to three weeks ahead) and, for a league with teams, one request for its team list
//! the first time and then weekly. A league is read again after six hours, or every twenty
//! minutes while a followed game is about to start or just started (so a postponement or a
//! finished game shows up). Requests go one at a time with a gap between them (`espn.rs`); a
//! league that failed is left alone for 15 minutes, doubling with each failure in a row up to
//! six hours; and a run stops after two leagues in a row fail, so a source that is down costs
//! one or two timeouts, not one per followed league.

use super::Sports;
use super::catalog::{self, League};
use super::data::{self, Upserted};
use super::espn::{Fixture, TeamInfo};
use crate::store::Store;
use crate::upstream::UpstreamError;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use time::{Date, OffsetDateTime};

/// The window that is read: from the start of yesterday (UTC) to three weeks ahead.
const BACK: time::Duration = time::Duration::days(1);
const AHEAD: time::Duration = time::Duration::days(21);
/// A league is read again after this long.
pub const STALE: Duration = Duration::from_secs(6 * 3600);
/// ... or after this long while a followed event is about to start or has just started.
const NEAR_STALE: Duration = Duration::from_secs(20 * 60);
const NEAR_BEFORE: time::Duration = time::Duration::hours(4);
const NEAR_AFTER: time::Duration = time::Duration::hours(2);
const TEAMS_STALE: Duration = Duration::from_secs(7 * 86_400);
const FAIL_FIRST: Duration = Duration::from_secs(15 * 60);
const FAIL_MAX: Duration = Duration::from_secs(6 * 3600);
/// Leagues read per worker run, and how long a run may keep starting new ones.
const LEAGUES_PER_RUN: usize = 6;
const RUN_BUDGET: Duration = Duration::from_secs(45);
const MAX_FAILURES_IN_A_ROW: usize = 2;

#[derive(Debug)]
pub enum RefreshError {
    Upstream(UpstreamError),
    Store(anyhow::Error),
}

impl From<UpstreamError> for RefreshError {
    fn from(e: UpstreamError) -> Self {
        RefreshError::Upstream(e)
    }
}

impl From<anyhow::Error> for RefreshError {
    fn from(e: anyhow::Error) -> Self {
        RefreshError::Store(e)
    }
}

impl RefreshError {
    pub fn describe(&self) -> String {
        match self {
            RefreshError::Upstream(e) => format!("the schedule source {}", e.describe()),
            RefreshError::Store(e) => format!("could not save the schedule: {e:#}"),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub fixtures: usize,
    pub added: usize,
    pub changed: usize,
    pub cancelled: usize,
}

fn start_of_day(t: OffsetDateTime) -> OffsetDateTime {
    t.replace_time(time::Time::MIDNIGHT)
}

/// The calendar months that cover `from - 1 day` to `to + 1 day` (the feed's days are not
/// UTC days, so the window gets a day of margin on each side).
fn months(from: OffsetDateTime, to: OffsetDateTime) -> Vec<(i32, u8)> {
    let first: Date = (from - time::Duration::days(1)).date();
    let last: Date = (to + time::Duration::days(1)).date();
    let mut out = vec![];
    let (mut y, mut m) = (first.year(), first.month() as u8);
    loop {
        out.push((y, m));
        if (y, m) >= (last.year(), last.month() as u8) {
            break;
        }
        if m == 12 {
            (y, m) = (y + 1, 1);
        } else {
            m += 1;
        }
    }
    out
}

/// Reads one league's schedule and stores it. A failure is recorded against the league (so it
/// is left alone for a while) and returned; stored events are left as they were.
pub async fn refresh_league(
    store: &Store,
    sports: &Sports,
    league: &'static League,
    now: OffsetDateTime,
) -> Result<Stats, RefreshError> {
    match read_league(store, sports, league, now).await {
        Ok(stats) => {
            let slug = league.slug;
            store
                .with(move |c| data::note_refreshed(c, slug, now))
                .await?;
            // The team list is a nicety (it names the teams a person can follow): a failure
            // to read it does not fail the league.
            if league.teams {
                let _ = ensure_teams(store, sports, league, now).await;
            }
            Ok(stats)
        }
        Err(e) => {
            if let RefreshError::Upstream(u) = &e {
                let (slug, msg) = (league.slug, u.describe());
                store
                    .with(move |c| {
                        let failures = data::refresh_state(c, slug)?.failures;
                        data::note_failed(c, slug, &msg, now + backoff(failures))
                    })
                    .await?;
            }
            Err(e)
        }
    }
}

/// How long a league is left alone after `failures` failures in a row (before this one).
fn backoff(failures: u32) -> time::Duration {
    let d = FAIL_FIRST
        .saturating_mul(1u32 << failures.min(10))
        .min(FAIL_MAX);
    time::Duration::try_from(d).unwrap_or(time::Duration::hours(6))
}

async fn read_league(
    store: &Store,
    sports: &Sports,
    league: &'static League,
    now: OffsetDateTime,
) -> Result<Stats, RefreshError> {
    let from = start_of_day(now - BACK);
    let to = now + AHEAD;
    let mut fetched: Vec<Fixture> = vec![];
    let mut complete = true;
    let mut failed = None;
    for (y, m) in months(from, to) {
        match sports.espn.month(league.espn, y, m).await {
            Ok(month) => {
                complete &= !month.truncated;
                fetched.extend(month.fixtures.iter().cloned());
            }
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    let seen: HashSet<String> = fetched.iter().map(|f| f.id.clone()).collect();
    // Only what is not over by the start of the window is kept: no backlog of old games.
    let keep: Vec<Fixture> = fetched
        .iter()
        .filter(|f| f.ends_at.unwrap_or(f.starts_at) >= from)
        .cloned()
        .collect();
    // A schedule is only trusted to say what is gone when every month answered, none was cut
    // short, and it was not empty (an empty answer is more likely a glitch than a league that
    // cancelled everything).
    let trusted = failed.is_none() && complete && !fetched.is_empty();
    let slug = league.slug;
    let stats = store
        .with(move |c| {
            let refs: Vec<&Fixture> = keep.iter().collect();
            let Upserted { added, changed } = data::upsert_events(c, slug, &refs, now)?;
            let cancelled = if trusted {
                data::mark_vanished(c, slug, from, to, &seen, now)?
            } else {
                0
            };
            Ok(Stats {
                fixtures: refs.len(),
                added,
                changed,
                cancelled,
            })
        })
        .await?;
    match failed {
        Some(e) => Err(e.into()),
        None => Ok(stats),
    }
}

/// Reads the league's team list unless it was read within the week (or never produced a team).
pub async fn ensure_teams(
    store: &Store,
    sports: &Sports,
    league: &'static League,
    now: OffsetDateTime,
) -> Result<(), RefreshError> {
    if !league.teams {
        return Ok(());
    }
    let slug = league.slug;
    let (state, known) = store
        .with(move |c| {
            Ok((
                data::refresh_state(c, slug)?,
                data::teams_of(c, slug)?.len(),
            ))
        })
        .await?;
    let fresh = state
        .teams
        .is_some_and(|t| now - t < time::Duration::try_from(TEAMS_STALE).unwrap_or_default());
    if fresh && known > 0 {
        return Ok(());
    }
    // A failure to read it is not retried for a while either.
    if state.next_try.is_some_and(|t| t > now) {
        return Ok(());
    }
    let teams: std::sync::Arc<Vec<TeamInfo>> = sports.espn.teams(league.espn).await?;
    store
        .with(move |c| {
            data::upsert_teams(c, slug, &teams)?;
            data::note_teams(c, slug, now)
        })
        .await?;
    Ok(())
}

/// What `ensure_fresh_within` found or did.
#[derive(Debug)]
pub enum Freshness {
    /// Read recently enough; nothing was asked of the source.
    Fresh,
    Refreshed(Stats),
    /// The source failed (or a failure earlier is still being waited out); stored data stands.
    Failed(String),
    /// Still being read when the caller stopped waiting; it carries on in the background.
    Pending,
}

/// Brings a league up to date unless it was read within `max_age`, waiting at most `budget`
/// for the source. The read continues in the background past the budget.
pub async fn ensure_fresh_within(
    store: Store,
    sports: std::sync::Arc<Sports>,
    league: &'static League,
    max_age: Duration,
    budget: Duration,
) -> Freshness {
    let work = tokio::spawn(async move {
        let now = OffsetDateTime::now_utc();
        let slug = league.slug;
        let state = match store.with(move |c| data::refresh_state(c, slug)).await {
            Ok(s) => s,
            Err(e) => return Freshness::Failed(format!("{e:#}")),
        };
        let age_ok = state
            .refreshed
            .is_some_and(|t| now - t < time::Duration::try_from(max_age).unwrap_or_default());
        if age_ok {
            // The events are current; the team list may still be missing (its first read
            // failed). Asking costs nothing when it is known.
            let _ = ensure_teams(&store, &sports, league, now).await;
            return Freshness::Fresh;
        }
        if let Some(t) = state.next_try.filter(|t| *t > now) {
            return Freshness::Failed(format!(
                "the schedule source failed earlier; trying again after {}",
                crate::import::rfc3339(t)
            ));
        }
        match refresh_league(&store, &sports, league, now).await {
            Ok(s) => Freshness::Refreshed(s),
            Err(e) => Freshness::Failed(e.describe()),
        }
    });
    match tokio::time::timeout(budget, work).await {
        Ok(Ok(f)) => f,
        Ok(Err(_)) => Freshness::Failed("the refresh stopped unexpectedly".into()),
        Err(_) => Freshness::Pending,
    }
}

/// Reads the team list if it is not known, waiting at most `budget` (it carries on past that).
/// `Err` says why the list could not be read.
pub async fn ensure_teams_within(
    store: Store,
    sports: std::sync::Arc<Sports>,
    league: &'static League,
    budget: Duration,
) -> Result<(), String> {
    let work = tokio::spawn(async move {
        ensure_teams(&store, &sports, league, OffsetDateTime::now_utc())
            .await
            .map_err(|e| e.describe())
    });
    match tokio::time::timeout(budget, work).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("the team list could not be read".into()),
        Err(_) => Err("the schedule source is slow to answer".into()),
    }
}

/// Whether a followed league should be read now.
fn due(store_state: &data::RefreshRow, near: bool, now: OffsetDateTime) -> bool {
    if store_state.next_try.is_some_and(|t| t > now) {
        return false;
    }
    let Some(last) = store_state.refreshed else {
        return true;
    };
    let age = now - last;
    let limit = if near { NEAR_STALE } else { STALE };
    age >= time::Duration::try_from(limit).unwrap_or_default()
}

/// The worker's task: reads the followed leagues that are due. It never returns an error, so
/// it cannot fail a worker pass; what happened is in the value it returns.
pub async fn run(store: &Store, sports: &Sports, now: OffsetDateTime) -> Value {
    let started = Instant::now();
    let plan = store
        .with(move |c| {
            let mut due_now: Vec<(Option<OffsetDateTime>, &'static League)> = vec![];
            for slug in data::followed_leagues(c)? {
                let Some(league) = catalog::find(&slug) else {
                    continue;
                };
                let state = data::refresh_state(c, &slug)?;
                let near = !data::followed_events(
                    c,
                    now - NEAR_BEFORE,
                    now + NEAR_AFTER,
                    Some(&slug),
                    Some(&["scheduled"]),
                    1,
                )?
                .is_empty();
                if due(&state, near, now) {
                    due_now.push((state.refreshed, league));
                }
            }
            // Never read first, then the longest ago.
            due_now.sort_by_key(|(at, l)| (*at, l.slug));
            Ok(due_now)
        })
        .await;
    let plan = match plan {
        Ok(p) => p,
        Err(e) => return json!({ "error": format!("{e:#}"), "leagues": [] }),
    };
    let due_count = plan.len();
    let mut results = vec![];
    let mut in_a_row = 0;
    let mut stopped = None;
    for (_, league) in plan.into_iter().take(LEAGUES_PER_RUN) {
        if started.elapsed() >= RUN_BUDGET {
            stopped = Some("out of time for this run");
            break;
        }
        if in_a_row >= MAX_FAILURES_IN_A_ROW {
            stopped = Some("the schedule source is failing");
            break;
        }
        match refresh_league(store, sports, league, now).await {
            Ok(s) => {
                in_a_row = 0;
                results.push(json!({
                    "league": league.slug, "status": "ok", "fixtures": s.fixtures,
                    "added": s.added, "changed": s.changed, "cancelled": s.cancelled,
                }));
            }
            Err(e) => {
                in_a_row += 1;
                results.push(json!({
                    "league": league.slug, "status": "failed", "error": e.describe(),
                }));
            }
        }
    }
    json!({ "due": due_count, "leagues": results, "stopped": stopped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn the_window_asks_for_one_or_two_months() {
        let m = |now: OffsetDateTime| months(start_of_day(now - BACK), now + AHEAD);
        assert_eq!(
            m(datetime!(2026-11-12 10:00 UTC)),
            vec![(2026, 11), (2026, 12)]
        );
        assert_eq!(
            m(datetime!(2026-12-20 10:00 UTC)),
            vec![(2026, 12), (2027, 1)]
        );
        assert_eq!(
            m(datetime!(2026-02-02 10:00 UTC)),
            vec![(2026, 2), (2026, 3)]
        );
        for day in 1..=365 {
            let now = datetime!(2026-01-01 12:00 UTC) + time::Duration::days(day);
            assert!(m(now).len() <= 2, "{now}");
        }
    }

    #[test]
    fn a_league_that_keeps_failing_is_left_alone_for_longer_each_time() {
        assert_eq!(backoff(0), time::Duration::minutes(15));
        assert_eq!(backoff(1), time::Duration::minutes(30));
        assert_eq!(backoff(3), time::Duration::hours(2));
        assert_eq!(backoff(20), time::Duration::hours(6));
    }
}
