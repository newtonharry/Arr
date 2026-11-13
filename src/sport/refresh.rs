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
use super::espn::Fixture;
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
/// A team list that holds a `Team <id>` fallback name is read again after this long.
const TEAMS_FALLBACK_STALE: Duration = Duration::from_secs(86_400);
const FAIL_FIRST: Duration = Duration::from_secs(15 * 60);
const FAIL_MAX: Duration = Duration::from_secs(6 * 3600);
/// Leagues read per worker run, and how long a run may keep starting new ones.
const LEAGUES_PER_RUN: usize = 6;
const RUN_BUDGET: Duration = Duration::from_secs(45);
const MAX_FAILURES_IN_A_ROW: usize = 2;
/// Team lists of leagues nobody follows or has opened, read in the background so that search
/// finds teams out of the box: at most this many per run, after the followed schedules.
const CATALOGUE_TEAMS_PER_RUN: usize = 2;

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
    /// Events that are no longer listed (see `data::mark_unlisted`). Never cancelled.
    pub unlisted: usize,
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

/// The span of a month in which an absent event may be called unlisted. The feed's days are
/// the American ones, so an event on the month's first or last day may be listed under the
/// neighbouring month: that day is left out of the span unless the neighbour answered in full
/// too (`inner_start`, `inner_end`), in which case one of the two would list the event.
fn month_span(
    y: i32,
    m: u8,
    inner_start: bool,
    inner_end: bool,
) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let first = Date::from_calendar_date(y, time::Month::try_from(m).ok()?, 1).ok()?;
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let next = Date::from_calendar_date(ny, time::Month::try_from(nm).ok()?, 1).ok()?;
    let day = time::Duration::days(1);
    let (start, end) = (first.midnight().assume_utc(), next.midnight().assume_utc());
    Some((
        if inner_start { start } else { start + day },
        if inner_end { end } else { end - day },
    ))
}

/// Reads one league's schedule and stores it. A failure is recorded against the league (so it
/// is left alone for a while) and returned; stored events are left as they were. Callers go
/// through `refresh_when`, which makes sure only one read of a league runs at a time.
async fn refresh_league(
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
    // Everything the answers mention, including records that could not be read: an event
    // whose record is broken is not an absent one.
    let mut seen: HashSet<String> = HashSet::new();
    // Months whose answer can be trusted to list everything: each is judged on its own.
    let mut complete_months: Vec<(i32, u8)> = vec![];
    let mut failed = None;
    for (y, m) in months(from, to) {
        match sports.espn.month(league, y, m).await {
            Ok(month) => {
                seen.extend(month.fixtures.iter().map(|f| f.id.clone()));
                seen.extend(month.rejected_ids.iter().cloned());
                fetched.extend(month.fixtures.iter().cloned());
                if month.complete() {
                    complete_months.push((y, m));
                }
            }
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    // Absence is only ever read from a full set of answers.
    let complete: Vec<(OffsetDateTime, OffsetDateTime)> = if failed.is_some() {
        vec![]
    } else {
        complete_months
            .iter()
            .filter_map(|&(y, m)| {
                let before = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
                let after = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                // A shared boundary day is only judged when both months answered in full.
                month_span(
                    y,
                    m,
                    complete_months.contains(&before),
                    complete_months.contains(&after),
                )
            })
            .collect()
    };
    let slug = league.slug;
    let total = fetched.len();
    let stats = store
        .with(move |c| {
            let refs: Vec<&Fixture> = fetched.iter().collect();
            let Upserted { added, changed } = data::upsert_events(c, slug, &refs, from, now)?;
            let unlisted = if complete.is_empty() {
                0
            } else {
                data::mark_unlisted(c, slug, &complete, &seen, now)?
            };
            Ok(Stats {
                fixtures: total,
                added,
                changed,
                unlisted,
            })
        })
        .await?;
    match failed {
        Some(e) => Err(e.into()),
        None => Ok(stats),
    }
}

/// Whether the league's team list was read successfully and recently enough to say who is in
/// the league. Only such a list may answer "no such team".
pub fn teams_current(state: &data::RefreshRow, now: OffsetDateTime) -> bool {
    state
        .teams
        .is_some_and(|t| now - t < time::Duration::try_from(TEAMS_STALE).unwrap_or_default())
}

/// Whether a stored team list needs no new read: it has teams, and it was read within the week,
/// or within a day when it holds a `Team <id>` fallback name (a name that was refused, which a
/// corrected rule or feed may now give). A read that succeeds restamps the list, so a league whose
/// list always holds a refused name is asked for at most once a day; one that fails waits out
/// `teams_next_try` (a list in that wait is skipped by every caller). This is the freshness every
/// reader of the lists uses; `teams_current` (seven days, for "no such team" versus "try again")
/// is separate.
fn list_fresh(
    state: &data::RefreshRow,
    known: usize,
    fallbacks: bool,
    now: OffsetDateTime,
) -> bool {
    let age = if fallbacks {
        TEAMS_FALLBACK_STALE
    } else {
        TEAMS_STALE
    };
    known > 0
        && state
            .teams
            .is_some_and(|t| now - t < time::Duration::try_from(age).unwrap_or_default())
}

/// Reads the league's team list unless it was read within the week (or never produced a team).
/// A failure is remembered, with a time before which it is not tried again.
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
    let (state, known, fallbacks) = store
        .with(move |c| {
            let teams = data::teams_of(c, slug)?;
            // Teams stored as `Team <id>` (their real name was refused once): read the list again
            // after a day, not a week, so a corrected rule or a corrected feed shows up soon.
            let fallbacks = teams.iter().any(|t| t.name == format!("Team {}", t.id));
            Ok((data::refresh_state(c, slug)?, teams.len(), fallbacks))
        })
        .await?;
    if list_fresh(&state, known, fallbacks, now) {
        return Ok(());
    }
    if state.teams_next_try.is_some_and(|t| t > now) {
        return Ok(());
    }
    match sports.espn.teams(league.espn).await {
        // Well formed but empty: this league has no team list yet. Its own retry timing, and
        // nothing for the other leagues (the client saw a good answer).
        Ok(teams) if teams.is_empty() => {
            let failures = state.teams_failures;
            store
                .with(move |c| data::note_teams_failed(c, slug, now + backoff(failures)))
                .await?;
            Ok(())
        }
        Ok(teams) => {
            store
                .with(move |c| {
                    data::upsert_teams(c, slug, &teams)?;
                    data::note_teams(c, slug, now)
                })
                .await?;
            Ok(())
        }
        Err(e) => {
            let failures = state.teams_failures;
            store
                .with(move |c| data::note_teams_failed(c, slug, now + backoff(failures)))
                .await?;
            Err(e.into())
        }
    }
}

/// What `refresh_when` found or did.
#[derive(Debug)]
pub enum Freshness {
    /// Read recently enough; nothing was asked of the source.
    Fresh,
    Refreshed(Stats),
    /// The source failed just now; stored data stands.
    Failed(String),
    /// An earlier failure is still being waited out; nothing was asked.
    Backoff(String),
    /// Still being read when the caller stopped waiting; it carries on in the background.
    Pending,
}

/// When a league needs reading.
#[derive(Clone, Copy)]
enum Need {
    /// Not read within this long.
    Older(Duration),
    /// The worker's rule: six hours, or twenty minutes near a followed game.
    Due,
}

fn is_near(c: &rusqlite::Connection, slug: &str, now: OffsetDateTime) -> anyhow::Result<bool> {
    Ok(!data::followed_events(
        c,
        now - NEAR_BEFORE,
        now + NEAR_AFTER,
        Some(slug),
        Some(&["scheduled"]),
        1,
    )?
    .is_empty())
}

/// Reads a league if it needs it. There is one read of a league at a time, whoever asks (a
/// page, a follow, the worker): the others wait their turn, find the league freshly read and
/// ask the source for nothing.
async fn refresh_when(
    store: &Store,
    sports: &Sports,
    league: &'static League,
    now: OffsetDateTime,
    need: Need,
) -> Freshness {
    let lock = sports.league_lock(league.slug);
    let _turn = lock.lock().await;
    refresh_locked(store, sports, league, now, need).await
}

/// `refresh_when` for a caller that already holds the league's turn.
async fn refresh_locked(
    store: &Store,
    sports: &Sports,
    league: &'static League,
    now: OffsetDateTime,
    need: Need,
) -> Freshness {
    let slug = league.slug;
    let looked = store
        .with(move |c| Ok((data::refresh_state(c, slug)?, is_near(c, slug, now)?)))
        .await;
    let (state, near) = match looked {
        Ok(x) => x,
        Err(e) => return Freshness::Failed(format!("{e:#}")),
    };
    if let Some(t) = state.next_try.filter(|t| *t > now) {
        return Freshness::Backoff(format!(
            "the schedule source failed earlier; trying again after {}",
            crate::import::rfc3339(t)
        ));
    }
    let needed = match need {
        Need::Older(max_age) => state
            .refreshed
            .is_none_or(|t| now - t >= time::Duration::try_from(max_age).unwrap_or_default()),
        Need::Due => due(&state, near, now),
    };
    if !needed {
        // The events are current; the team list may still be missing (its first read
        // failed). Asking costs nothing when it is known.
        let _ = ensure_teams(store, sports, league, now).await;
        return Freshness::Fresh;
    }
    match refresh_league(store, sports, league, now).await {
        Ok(s) => Freshness::Refreshed(s),
        Err(e) => Freshness::Failed(e.describe()),
    }
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
        refresh_when(
            &store,
            &sports,
            league,
            OffsetDateTime::now_utc(),
            Need::Older(max_age),
        )
        .await
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
        // Behind a read of the league that is already running.
        let lock = sports.league_lock(league.slug);
        let _turn = lock.lock().await;
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
pub async fn run(store: Store, sports: std::sync::Arc<Sports>, now: OffsetDateTime) -> Value {
    run_with_budget(store, sports, now, RUN_BUDGET).await
}

/// `run` with its own limit on how long it may take. A league that is being read already (by a
/// page or a follow) is skipped, and the wait for a read the worker starts is cut at what is
/// left of the budget: that read carries on in the background and the pass moves on.
pub async fn run_with_budget(
    store: Store,
    sports: std::sync::Arc<Sports>,
    now: OffsetDateTime,
    budget: Duration,
) -> Value {
    let started = Instant::now();
    let (mut report, clean) = run_schedules(store.clone(), sports.clone(), now, budget).await;
    // Only once the followed leagues are done, and only if the source has been answering.
    let teams = if clean {
        read_catalogue_teams(
            &store,
            &sports,
            now,
            budget.saturating_sub(started.elapsed()),
        )
        .await
    } else {
        vec![]
    };
    report["catalogue_teams"] = json!(teams);
    report
}

/// The followed leagues that are due, nothing else. The second value says the run was clean:
/// it finished its list without a failure or running out of time.
pub async fn run_schedules(
    store: Store,
    sports: std::sync::Arc<Sports>,
    now: OffsetDateTime,
    budget: Duration,
) -> (Value, bool) {
    let started = Instant::now();
    let plan = {
        let store = store.clone();
        store
            .with(move |c| {
                let mut due_now: Vec<(Option<OffsetDateTime>, &'static League)> = vec![];
                for slug in data::followed_leagues(c)? {
                    let Some(league) = catalog::find(&slug) else {
                        continue;
                    };
                    let state = data::refresh_state(c, &slug)?;
                    if due(&state, is_near(c, &slug, now)?, now) {
                        due_now.push((state.refreshed, league));
                    }
                }
                // Never read first, then the longest ago.
                due_now.sort_by_key(|(at, l)| (*at, l.slug));
                Ok(due_now)
            })
            .await
    };
    let plan = match plan {
        Ok(p) => p,
        Err(e) => return (json!({ "error": format!("{e:#}"), "leagues": [] }), false),
    };
    let due_count = plan.len();
    let mut results = vec![];
    let mut in_a_row = 0;
    let mut failures = 0;
    // Due leagues that were dealt with (read, or found read already): a run that did fewer than
    // were due (the six-league cap, a league someone else is reading, one still running) is not clean.
    let mut done = 0;
    let mut stopped = None;
    for (_, league) in plan.into_iter().take(LEAGUES_PER_RUN) {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            stopped = Some("out of time for this run");
            break;
        }
        if in_a_row >= MAX_FAILURES_IN_A_ROW {
            stopped = Some("the schedule source is failing");
            break;
        }
        // Someone is reading this league right now: nothing for the worker to add.
        let Ok(turn) = sports.league_lock(league.slug).try_lock_owned() else {
            results.push(
                json!({ "league": league.slug, "status": "skipped", "why": "being read already" }),
            );
            continue;
        };
        let (store2, sports2) = (store.clone(), sports.clone());
        let work = tokio::spawn(async move {
            let _turn = turn;
            refresh_locked(&store2, &sports2, league, now, Need::Due).await
        });
        let outcome = match tokio::time::timeout(remaining, work).await {
            Ok(Ok(f)) => f,
            Ok(Err(_)) => Freshness::Failed("the refresh stopped unexpectedly".into()),
            Err(_) => {
                results.push(json!({ "league": league.slug, "status": "running", "why": "still being read in the background" }));
                stopped = Some("out of time for this run");
                break;
            }
        };
        match outcome {
            Freshness::Refreshed(s) => {
                in_a_row = 0;
                done += 1;
                results.push(json!({
                    "league": league.slug, "status": "ok", "fixtures": s.fixtures,
                    "added": s.added, "changed": s.changed, "unlisted": s.unlisted,
                }));
            }
            Freshness::Failed(why) => {
                in_a_row += 1;
                failures += 1;
                results.push(json!({ "league": league.slug, "status": "failed", "error": why }));
            }
            Freshness::Fresh | Freshness::Backoff(_) => {
                done += 1;
                results.push(json!({ "league": league.slug, "status": "skipped" }));
            }
            Freshness::Pending => {
                results.push(json!({ "league": league.slug, "status": "skipped" }));
            }
        }
    }
    let clean = stopped.is_none() && failures == 0 && done == due_count;
    (
        json!({ "due": due_count, "leagues": results, "stopped": stopped }),
        clean,
    )
}

/// Reads the team list of up to `CATALOGUE_TEAMS_PER_RUN` catalogue leagues whose list is not
/// current (never read first), so a team in any league can be found by search. It is the same
/// read a league page makes (`ensure_teams`: one request, the client's gap and backoff, a
/// failed league waited out on its own timing), and it stops at the first failure. A league that
/// is being read already is skipped.
async fn read_catalogue_teams(
    store: &Store,
    sports: &std::sync::Arc<Sports>,
    now: OffsetDateTime,
    budget: Duration,
) -> Vec<Value> {
    let started = Instant::now();
    let plan = store
        .with(move |c| {
            let mut want: Vec<(Option<OffsetDateTime>, &'static League)> = vec![];
            for league in catalog::LEAGUES.iter().filter(|l| l.teams) {
                let state = data::refresh_state(c, league.slug)?;
                let teams = data::teams_of(c, league.slug)?;
                let fallbacks = teams.iter().any(|t| t.name == format!("Team {}", t.id));
                if list_fresh(&state, teams.len(), fallbacks, now) {
                    continue;
                }
                if state.teams_next_try.is_some_and(|t| t > now) {
                    continue;
                }
                want.push((state.teams, league));
            }
            want.sort_by_key(|(at, l)| (*at, l.slug));
            Ok(want)
        })
        .await
        .unwrap_or_default();
    let mut out = vec![];
    for (_, league) in plan.into_iter().take(CATALOGUE_TEAMS_PER_RUN) {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let Ok(turn) = sports.league_lock(league.slug).try_lock_owned() else {
            continue;
        };
        let (store2, sports2) = (store.clone(), sports.clone());
        let work = tokio::spawn(async move {
            let _turn = turn;
            ensure_teams(&store2, &sports2, league, now).await
        });
        match tokio::time::timeout(remaining, work).await {
            Ok(Ok(Ok(()))) => out.push(json!({ "league": league.slug, "status": "ok" })),
            Ok(Ok(Err(e))) => {
                out.push(
                    json!({ "league": league.slug, "status": "failed", "error": e.describe() }),
                );
                break;
            }
            _ => break,
        }
    }
    out
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
            vec![(2026, 1), (2026, 2)]
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
