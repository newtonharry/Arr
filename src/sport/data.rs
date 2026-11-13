//! Sport tables in `state.sqlite`: follows, events, teams and when each league was last read.
//! Added next to the existing tables with `CREATE TABLE IF NOT EXISTS`, so an older release
//! that opens the same state simply never touches them. Timestamps are RFC 3339 UTC with whole
//! seconds, like everywhere else, so string comparison orders them.
//!
//! There is no column for a score, a winner or a result, and there must not be.

use super::espn::{Fixture, TeamInfo};
use crate::import::rfc3339;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::collections::HashSet;
use time::OffsetDateTime;

pub fn create_tables(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS sport_follows (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            league TEXT NOT NULL,
            team_id TEXT,
            team_name TEXT,
            since_utc TEXT NOT NULL
        );
        -- A league follow has no team; NULLs never collide in a plain UNIQUE, so the index
        -- reads them as ''.
        CREATE UNIQUE INDEX IF NOT EXISTS sport_follows_one ON sport_follows(league, IFNULL(team_id, ''));
        CREATE TABLE IF NOT EXISTS sport_events (
            id TEXT PRIMARY KEY,
            league TEXT NOT NULL,
            season TEXT,
            starts_at_utc TEXT NOT NULL,
            ends_at_utc TEXT,
            name TEXT NOT NULL,
            short_name TEXT,
            home_id TEXT,
            home_name TEXT,
            away_id TEXT,
            away_name TEXT,
            round TEXT,
            venue TEXT,
            state TEXT NOT NULL CHECK (state IN ('scheduled', 'postponed', 'cancelled', 'played', 'unlisted')),
            first_seen_utc TEXT NOT NULL,
            updated_utc TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS sport_events_by_league ON sport_events(league, starts_at_utc);
        CREATE INDEX IF NOT EXISTS sport_events_by_time ON sport_events(starts_at_utc);
        CREATE TABLE IF NOT EXISTS sport_teams (
            league TEXT NOT NULL,
            team_id TEXT NOT NULL,
            name TEXT NOT NULL,
            short_name TEXT,
            abbreviation TEXT,
            PRIMARY KEY (league, team_id)
        );
        -- Fixtures a schedule read listed but did not store because they were over before the
        -- league was followed. They are never served: they are only evidence, so that no other
        -- fixture fits is judged over everything the recorded stretch listed, and not only
        -- over what was kept.
        CREATE TABLE IF NOT EXISTS sport_evidence (
            id TEXT PRIMARY KEY,
            league TEXT NOT NULL,
            season TEXT,
            starts_at_utc TEXT NOT NULL,
            ends_at_utc TEXT,
            name TEXT NOT NULL,
            short_name TEXT,
            home_id TEXT,
            home_name TEXT,
            away_id TEXT,
            away_name TEXT,
            round TEXT,
            venue TEXT,
            state TEXT NOT NULL,
            noted_utc TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS sport_evidence_by_league ON sport_evidence(league, starts_at_utc);
        -- What the last read of a league's team list was: whole (nothing dropped, renamed or cut)
        -- or not. Only a whole list may say that a short name names one team.
        CREATE TABLE IF NOT EXISTS sport_teams_read (
            league TEXT PRIMARY KEY,
            read_utc TEXT NOT NULL,
            complete INTEGER NOT NULL
        );
        -- The stretches of time a schedule read judged in full (see `refresh::read_league`):
        -- every event of the league that starts in [from, to) was in the answer, as of
        -- `read_utc`. No other fixture fits is only known inside such a stretch.
        CREATE TABLE IF NOT EXISTS sport_ranges (
            league TEXT NOT NULL,
            from_utc TEXT NOT NULL,
            to_utc TEXT NOT NULL,
            read_utc TEXT NOT NULL,
            PRIMARY KEY (league, from_utc)
        );
        CREATE TABLE IF NOT EXISTS sport_refresh (
            league TEXT PRIMARY KEY,
            refreshed_utc TEXT,
            teams_utc TEXT,
            teams_failures INTEGER NOT NULL DEFAULT 0,
            teams_next_try_utc TEXT,
            failures INTEGER NOT NULL DEFAULT 0,
            next_try_utc TEXT,
            last_error TEXT
        );",
    )?;
    Ok(())
}

fn ts(t: OffsetDateTime) -> String {
    rfc3339(t)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub id: String,
    pub league: String,
    pub season: Option<String>,
    pub starts_at: String,
    pub ends_at: Option<String>,
    pub name: String,
    pub short_name: Option<String>,
    pub home_id: Option<String>,
    pub home_name: Option<String>,
    pub away_id: Option<String>,
    pub away_name: Option<String>,
    pub round: Option<String>,
    pub venue: Option<String>,
    pub state: String,
}

const EVENT_COLUMNS: &str = "e.id, e.league, e.season, e.starts_at_utc, e.ends_at_utc, e.name, e.short_name, e.home_id, e.home_name, e.away_id, e.away_name, e.round, e.venue, e.state";

fn event_row(r: &Row) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        id: r.get(0)?,
        league: r.get(1)?,
        season: r.get(2)?,
        starts_at: r.get(3)?,
        ends_at: r.get(4)?,
        name: r.get(5)?,
        short_name: r.get(6)?,
        home_id: r.get(7)?,
        home_name: r.get(8)?,
        away_id: r.get(9)?,
        away_name: r.get(10)?,
        round: r.get(11)?,
        venue: r.get(12)?,
        state: r.get(13)?,
    })
}

/// One event by id. An `unlisted` event is never served, so it is not found either.
pub fn event(c: &Connection, id: &str) -> Result<Option<EventRow>> {
    Ok(c.query_row(
        &format!(
            "SELECT {EVENT_COLUMNS} FROM sport_events e WHERE e.id = ?1 AND e.state != 'unlisted'"
        ),
        [id],
        event_row,
    )
    .optional()?)
}

/// Events of one league between the same two teams (either way round) that start in
/// `[from, to)`. A pair has a handful of fixtures in any window, so nothing is cut.
pub fn pair_events_between(
    c: &Connection,
    league: &str,
    a: &str,
    b: &str,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<EventRow>> {
    // An event whose sides are not known could be a fixture of this pair: it is loaded too, and
    // the judgement treats it as a fixture it cannot rule out.
    let mut stmt = c.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM sport_events e
         WHERE e.league = ?1 AND e.state != 'unlisted' AND e.starts_at_utc >= ?2 AND e.starts_at_utc < ?3
           AND ((e.home_id = ?4 AND e.away_id = ?5) OR (e.home_id = ?5 AND e.away_id = ?4)
                OR e.home_id IS NULL OR e.away_id IS NULL)
         UNION
         SELECT {EVENT_COLUMNS} FROM sport_evidence e
         WHERE e.league = ?1 AND e.starts_at_utc >= ?2 AND e.starts_at_utc < ?3
           AND ((e.home_id = ?4 AND e.away_id = ?5) OR (e.home_id = ?5 AND e.away_id = ?4)
                OR e.home_id IS NULL OR e.away_id IS NULL)
           AND e.id NOT IN (SELECT id FROM sport_events)
         ORDER BY 4, 1 LIMIT 200"
    ))?;
    Ok(stmt
        .query_map(params![league, ts(from), ts(to), a, b], event_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Events of one league that start in `[from, to)`, any state but `unlisted`, soonest first.
pub fn league_events_between(
    c: &Connection,
    league: &str,
    from: OffsetDateTime,
    to: OffsetDateTime,
    limit: usize,
) -> Result<Vec<EventRow>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM sport_events e
         WHERE e.league = ?1 AND e.state != 'unlisted' AND e.starts_at_utc >= ?2 AND e.starts_at_utc < ?3
         UNION
         SELECT {EVENT_COLUMNS} FROM sport_evidence e
         WHERE e.league = ?1 AND e.starts_at_utc >= ?2 AND e.starts_at_utc < ?3
           AND e.id NOT IN (SELECT id FROM sport_events)
         ORDER BY 4, 1 LIMIT ?4"
    ))?;
    Ok(stmt
        .query_map(params![league, ts(from), ts(to), limit as i64], event_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn event_id(league: &str, espn_id: &str) -> String {
    format!("event:{league}:{espn_id}")
}

/// The name of an event whose own title did not pass the checks: the league and the date.
/// Whether a stored name is the neutral one (`neutral_name`): the feed's own title for the
/// event did not pass the checks, so nothing about who is in it is known.
pub fn is_neutral_name(league: &str, name: &str) -> bool {
    let l = super::catalog::find(league).map_or(league, |l| l.name);
    name.strip_prefix(l)
        .and_then(|rest| rest.strip_prefix(" event on "))
        .is_some()
}

fn neutral_name(league: &str, starts_at: OffsetDateTime) -> String {
    let l = super::catalog::find(league).map_or(league, |l| l.name);
    format!("{l} event on {}", starts_at.date())
}

fn row_of(league: &str, f: &Fixture) -> EventRow {
    EventRow {
        id: event_id(league, &f.id),
        league: league.to_string(),
        season: f.season.clone(),
        starts_at: ts(f.starts_at),
        ends_at: f.ends_at.map(ts),
        name: f
            .name
            .clone()
            .unwrap_or_else(|| neutral_name(league, f.starts_at)),
        short_name: f.short_name.clone(),
        home_id: f.home.as_ref().map(|s| s.id.clone()),
        home_name: f.home.as_ref().map(|s| s.name.clone()),
        away_id: f.away.as_ref().map(|s| s.id.clone()),
        away_name: f.away.as_ref().map(|s| s.name.clone()),
        round: f.round.clone(),
        venue: f.venue.clone(),
        state: f.state.as_str().to_string(),
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Upserted {
    pub added: usize,
    pub changed: usize,
}

/// Stores fixtures. A fixture that is already stored exactly as it is read changes nothing
/// (not even `updated_utc`), so reading the same schedule twice is a no-op. A known event is
/// always updated (it may have been played while Arr was off, or come back from `unlisted`);
/// a new one is stored only if it is not over by `keep_from`, so there is no backlog.
pub fn upsert_events(
    c: &mut Connection,
    league: &str,
    fixtures: &[&Fixture],
    keep_from: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<Upserted> {
    let tx = c.transaction()?;
    let mut out = Upserted::default();
    let now_s = ts(now);
    for f in fixtures {
        let new = row_of(league, f);
        let old = tx
            .query_row(
                &format!("SELECT {EVENT_COLUMNS} FROM sport_events e WHERE e.id = ?1"),
                [&new.id],
                event_row,
            )
            .optional()?;
        match old {
            None if f.ends_at.unwrap_or(f.starts_at) < keep_from => {
                // Not kept (no backlog), but not forgotten as evidence either: a recorded
                // stretch says everything in it was listed, and this was listed.
                tx.execute(
                    "INSERT OR REPLACE INTO sport_evidence (id, league, season, starts_at_utc, ends_at_utc, name, short_name, home_id, home_name, away_id, away_name, round, venue, state, noted_utc)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                    params![
                        new.id, new.league, new.season, new.starts_at, new.ends_at, new.name,
                        new.short_name, new.home_id, new.home_name, new.away_id, new.away_name,
                        new.round, new.venue, new.state, now_s
                    ],
                )?;
                continue;
            }
            None => {
                tx.execute(
                    "INSERT INTO sport_events (id, league, season, starts_at_utc, ends_at_utc, name, short_name, home_id, home_name, away_id, away_name, round, venue, state, first_seen_utc, updated_utc)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15)",
                    params![
                        new.id, new.league, new.season, new.starts_at, new.ends_at, new.name,
                        new.short_name, new.home_id, new.home_name, new.away_id, new.away_name,
                        new.round, new.venue, new.state, now_s
                    ],
                )?;
                out.added += 1;
            }
            Some(old) if old != new => {
                tx.execute(
                    "UPDATE sport_events SET season = ?2, starts_at_utc = ?3, ends_at_utc = ?4, name = ?5, short_name = ?6, home_id = ?7, home_name = ?8, away_id = ?9, away_name = ?10, round = ?11, venue = ?12, state = ?13, updated_utc = ?14 WHERE id = ?1",
                    params![
                        new.id, new.season, new.starts_at, new.ends_at, new.name, new.short_name,
                        new.home_id, new.home_name, new.away_id, new.away_name, new.round,
                        new.venue, new.state, now_s
                    ],
                )?;
                out.changed += 1;
            }
            Some(_) => {}
        }
        // Teams seen in fixtures are known teams, even before the league's team list is read.
        for side in [&f.home, &f.away].into_iter().flatten() {
            tx.execute(
                "INSERT OR IGNORE INTO sport_teams (league, team_id, name) VALUES (?1, ?2, ?3)",
                params![league, side.id, side.name],
            )?;
        }
    }
    tx.commit()?;
    Ok(out)
}

/// Moves events that are stored as still to come, start inside one of `ranges` (spans of
/// calendar months that came back complete) and are in none of the answers read to the
/// `unlisted` state. Absence never cancels: `cancelled` is only ever what the schedule source
/// says. An unlisted event is left out of every answer and comes back, in place, when the
/// source lists it again. Nothing is deleted.
pub fn mark_unlisted(
    c: &mut Connection,
    league: &str,
    ranges: &[(OffsetDateTime, OffsetDateTime)],
    seen: &HashSet<String>,
    now: OffsetDateTime,
) -> Result<usize> {
    let tx = c.transaction()?;
    let prefix = event_id(league, "");
    let mut n = 0;
    for (from, to) in ranges {
        let ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM sport_events WHERE league = ?1 AND state IN ('scheduled', 'postponed') AND starts_at_utc >= ?2 AND starts_at_utc < ?3",
            )?;
            stmt.query_map(params![league, ts(*from), ts(*to)], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            let espn_id = id.strip_prefix(&prefix).unwrap_or(&id);
            if !seen.contains(espn_id) {
                tx.execute(
                    "UPDATE sport_events SET state = 'unlisted', updated_utc = ?2 WHERE id = ?1",
                    params![id, ts(now)],
                )?;
                n += 1;
            }
        }
    }
    tx.commit()?;
    Ok(n)
}

pub fn upsert_teams(c: &mut Connection, league: &str, teams: &[TeamInfo]) -> Result<()> {
    let tx = c.transaction()?;
    for t in teams {
        tx.execute(
            "INSERT INTO sport_teams (league, team_id, name, short_name, abbreviation) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (league, team_id) DO UPDATE SET
                 name = CASE WHEN ?3 = 'Team ' || ?2 THEN name ELSE ?3 END,
                 short_name = ?4, abbreviation = ?5",
            params![league, t.id, t.name, t.short_name, t.abbreviation],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRow {
    pub league: String,
    pub id: String,
    pub name: String,
    pub short_name: Option<String>,
    pub abbreviation: Option<String>,
}

fn team_row(r: &Row) -> rusqlite::Result<TeamRow> {
    Ok(TeamRow {
        league: r.get(0)?,
        id: r.get(1)?,
        name: r.get(2)?,
        short_name: r.get(3)?,
        abbreviation: r.get(4)?,
    })
}

pub fn teams_of(c: &Connection, league: &str) -> Result<Vec<TeamRow>> {
    let mut stmt = c.prepare(
        "SELECT league, team_id, name, short_name, abbreviation FROM sport_teams WHERE league = ?1 ORDER BY name COLLATE NOCASE, team_id",
    )?;
    Ok(stmt
        .query_map([league], team_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn all_teams(c: &Connection) -> Result<Vec<TeamRow>> {
    let mut stmt = c.prepare(
        "SELECT league, team_id, name, short_name, abbreviation FROM sport_teams ORDER BY league, name COLLATE NOCASE",
    )?;
    Ok(stmt
        .query_map([], team_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn team(c: &Connection, league: &str, id: &str) -> Result<Option<TeamRow>> {
    Ok(c.query_row(
        "SELECT league, team_id, name, short_name, abbreviation FROM sport_teams WHERE league = ?1 AND team_id = ?2",
        params![league, id],
        team_row,
    )
    .optional()?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowRow {
    pub league: String,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub since: String,
}

/// A follow with its team's label taken from `sport_teams` now (the one place a team's name
/// lives and is corrected), falling back to the label stored when it was made, then to
/// `Team <id>`. `since` is always the stored one.
const FOLLOW_SELECT: &str = "SELECT f.league, f.team_id,
    CASE WHEN f.team_id IS NULL THEN NULL ELSE COALESCE(
        (SELECT t.name FROM sport_teams t WHERE t.league = f.league AND t.team_id = f.team_id),
        f.team_name, 'Team ' || f.team_id) END,
    f.since_utc FROM sport_follows f";

fn follow_row(r: &Row) -> rusqlite::Result<FollowRow> {
    Ok(FollowRow {
        league: r.get(0)?,
        team_id: r.get(1)?,
        team_name: r.get(2)?,
        since: r.get(3)?,
    })
}

/// Follows a league (no team) or a team in it. Following again changes nothing and keeps the
/// first follow time. The flag says whether this call made the follow.
pub fn follow(
    c: &mut Connection,
    league: &str,
    team: Option<(&str, &str)>,
    now: OffsetDateTime,
) -> Result<(FollowRow, bool)> {
    let tx = c.transaction()?;
    let team_id = team.map(|t| t.0);
    let existing = tx
        .query_row(
            &format!("{FOLLOW_SELECT} WHERE f.league = ?1 AND IFNULL(f.team_id, '') = ?2"),
            params![league, team_id.unwrap_or("")],
            follow_row,
        )
        .optional()?;
    if let Some(f) = existing {
        return Ok((f, false));
    }
    let row = FollowRow {
        league: league.to_string(),
        team_id: team_id.map(str::to_string),
        team_name: team.map(|t| t.1.to_string()),
        since: ts(now),
    };
    tx.execute(
        "INSERT INTO sport_follows (league, team_id, team_name, since_utc) VALUES (?1, ?2, ?3, ?4)",
        params![row.league, row.team_id, row.team_name, row.since],
    )?;
    tx.commit()?;
    Ok((row, true))
}

pub fn unfollow(c: &Connection, league: &str, team_id: Option<&str>) -> Result<bool> {
    Ok(c.execute(
        "DELETE FROM sport_follows WHERE league = ?1 AND IFNULL(team_id, '') = ?2",
        params![league, team_id.unwrap_or("")],
    )? > 0)
}

pub fn follows(c: &Connection) -> Result<Vec<FollowRow>> {
    let mut stmt = c.prepare(&format!("{FOLLOW_SELECT} ORDER BY f.since_utc, f.id"))?;
    Ok(stmt
        .query_map([], follow_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn followed_leagues(c: &Connection) -> Result<Vec<String>> {
    let mut stmt = c.prepare("SELECT DISTINCT league FROM sport_follows ORDER BY league")?;
    Ok(stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Events of what is followed that have not ended by `from` and start before `to`, soonest
/// first. An event counts for a follow only if it had not ended when the follow began, which
/// is what keeps the past out: following starts from now. Without `states`, every state.
pub fn followed_events(
    c: &Connection,
    from: OffsetDateTime,
    to: OffsetDateTime,
    league: Option<&str>,
    states: Option<&[&str]>,
    limit: usize,
) -> Result<Vec<EventRow>> {
    // Unlisted events are never served, whatever the caller asks for.
    let states_sql = states
        .map(|s| {
            let list = s
                .iter()
                .map(|x| format!("'{x}'"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("AND e.state IN ({list})")
        })
        .unwrap_or_else(|| "AND e.state != 'unlisted'".to_string());
    let mut stmt = c.prepare(&format!(
        "SELECT {EVENT_COLUMNS} FROM sport_events e
         WHERE COALESCE(e.ends_at_utc, e.starts_at_utc) >= ?1 AND e.starts_at_utc < ?2
           AND (?3 IS NULL OR e.league = ?3) {states_sql}
           AND EXISTS (SELECT 1 FROM sport_follows f
                       WHERE f.league = e.league
                         AND (f.team_id IS NULL OR f.team_id = e.home_id OR f.team_id = e.away_id)
                         AND COALESCE(e.ends_at_utc, e.starts_at_utc) >= f.since_utc)
         ORDER BY e.starts_at_utc, e.id LIMIT ?4"
    ))?;
    Ok(stmt
        .query_map(params![ts(from), ts(to), league, limit as i64], event_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// The first event still to come (or under way) of one follow, from the follow's own start.
pub fn next_event_of(
    c: &Connection,
    f: &FollowRow,
    now: OffsetDateTime,
) -> Result<Option<EventRow>> {
    let since = f.since.as_str().max(ts(now).as_str()).to_string();
    Ok(c.query_row(
        &format!(
            "SELECT {EVENT_COLUMNS} FROM sport_events e
             WHERE e.league = ?1 AND e.state IN ('scheduled', 'postponed')
               AND (?2 IS NULL OR e.home_id = ?2 OR e.away_id = ?2)
               AND COALESCE(e.ends_at_utc, e.starts_at_utc) >= ?3
             ORDER BY e.starts_at_utc, e.id LIMIT 1"
        ),
        params![f.league, f.team_id, since],
        event_row,
    )
    .optional()?)
}

/// A league's events for its page: still to come (or only just started) soonest first, or
/// already played, latest first.
pub fn league_events(
    c: &Connection,
    league: &str,
    upcoming: bool,
    now: OffsetDateTime,
    limit: usize,
) -> Result<Vec<EventRow>> {
    let sql = if upcoming {
        format!(
            "SELECT {EVENT_COLUMNS} FROM sport_events e
             WHERE e.league = ?1 AND e.state IN ('scheduled', 'postponed')
               AND COALESCE(e.ends_at_utc, e.starts_at_utc) >= ?2
             ORDER BY e.starts_at_utc, e.id LIMIT ?3"
        )
    } else {
        format!(
            "SELECT {EVENT_COLUMNS} FROM sport_events e
             WHERE e.league = ?1 AND e.state = 'played' AND e.starts_at_utc <= ?2
             ORDER BY e.starts_at_utc DESC, e.id LIMIT ?3"
        )
    };
    // A game that started a few hours ago and is not marked played yet is still "now".
    let edge = if upcoming {
        now - time::Duration::hours(4)
    } else {
        now
    };
    let mut stmt = c.prepare(&sql)?;
    Ok(stmt
        .query_map(params![league, ts(edge), limit as i64], event_row)?
        .collect::<rusqlite::Result<_>>()?)
}

#[derive(Debug, Clone, Default)]
pub struct RefreshRow {
    pub refreshed: Option<OffsetDateTime>,
    pub teams: Option<OffsetDateTime>,
    pub teams_failures: u32,
    pub teams_next_try: Option<OffsetDateTime>,
    pub failures: u32,
    pub next_try: Option<OffsetDateTime>,
    pub last_error: Option<String>,
}

fn parse(s: Option<String>) -> Option<OffsetDateTime> {
    s.and_then(|s| OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok())
}

pub fn refresh_state(c: &Connection, league: &str) -> Result<RefreshRow> {
    Ok(c.query_row(
        "SELECT refreshed_utc, teams_utc, failures, next_try_utc, last_error, teams_failures, teams_next_try_utc FROM sport_refresh WHERE league = ?1",
        [league],
        |r| {
            Ok(RefreshRow {
                refreshed: parse(r.get(0)?),
                teams: parse(r.get(1)?),
                failures: r.get::<_, i64>(2)?.max(0) as u32,
                next_try: parse(r.get(3)?),
                last_error: r.get(4)?,
                teams_failures: r.get::<_, i64>(5)?.max(0) as u32,
                teams_next_try: parse(r.get(6)?),
            })
        },
    )
    .optional()?
    .unwrap_or_default())
}

pub fn note_refreshed(c: &Connection, league: &str, now: OffsetDateTime) -> Result<()> {
    c.execute(
        "INSERT INTO sport_refresh (league, refreshed_utc, failures) VALUES (?1, ?2, 0)
         ON CONFLICT (league) DO UPDATE SET refreshed_utc = ?2, failures = 0, next_try_utc = NULL, last_error = NULL",
        params![league, ts(now)],
    )?;
    Ok(())
}

pub fn note_failed(
    c: &Connection,
    league: &str,
    error: &str,
    next_try: OffsetDateTime,
) -> Result<()> {
    c.execute(
        "INSERT INTO sport_refresh (league, failures, next_try_utc, last_error) VALUES (?1, 1, ?3, ?2)
         ON CONFLICT (league) DO UPDATE SET failures = failures + 1, next_try_utc = ?3, last_error = ?2",
        params![league, error, ts(next_try)],
    )?;
    Ok(())
}

pub fn note_teams(c: &Connection, league: &str, now: OffsetDateTime, complete: bool) -> Result<()> {
    c.execute(
        "INSERT INTO sport_teams_read (league, read_utc, complete) VALUES (?1, ?2, ?3)
         ON CONFLICT (league) DO UPDATE SET read_utc = ?2, complete = ?3",
        params![league, ts(now), complete],
    )?;
    c.execute(
        "INSERT INTO sport_refresh (league, teams_utc) VALUES (?1, ?2)
         ON CONFLICT (league) DO UPDATE SET teams_utc = ?2, teams_failures = 0, teams_next_try_utc = NULL",
        params![league, ts(now)],
    )?;
    Ok(())
}

/// When the league's team list was last read and whether that read was whole.
pub fn teams_read(c: &Connection, league: &str) -> Result<Option<(OffsetDateTime, bool)>> {
    let row: Option<(String, bool)> = c
        .query_row(
            "SELECT read_utc, complete FROM sport_teams_read WHERE league = ?1",
            [league],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.and_then(|(t, ok)| parse(Some(t)).map(|t| (t, ok))))
}

/// Records the stretches a schedule read judged in full.
pub fn record_ranges(
    c: &Connection,
    league: &str,
    spans: &[(OffsetDateTime, OffsetDateTime)],
    now: OffsetDateTime,
) -> Result<()> {
    for (from, to) in spans {
        c.execute(
            "INSERT INTO sport_ranges (league, from_utc, to_utc, read_utc) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (league, from_utc) DO UPDATE SET to_utc = ?3, read_utc = ?4",
            params![league, ts(*from), ts(*to), ts(now)],
        )?;
    }
    // What is long past and long since read is no use to a judgement: keep the table small.
    // The spans go first, and evidence is kept for as long as any span still covers it: a
    // span says "everything in these days was listed", and what it relied on must outlive it.
    c.execute(
        "DELETE FROM sport_ranges WHERE league = ?1 AND to_utc < ?2",
        params![league, ts(now - time::Duration::days(120))],
    )?;
    c.execute(
        "DELETE FROM sport_evidence WHERE league = ?1 AND starts_at_utc < ?2
           AND NOT EXISTS (SELECT 1 FROM sport_ranges r
                           WHERE r.league = sport_evidence.league
                             AND r.from_utc <= sport_evidence.starts_at_utc
                             AND sport_evidence.starts_at_utc < r.to_utc)",
        params![league, ts(now - time::Duration::days(120))],
    )?;
    Ok(())
}

/// A recorded range is good for a judgement now if it was read within `FRESH_DAYS`, or after
/// its own days were over (so nothing in it can have changed since).
pub const RANGE_FRESH_DAYS: i64 = 3;

/// The stretches the schedule is known in full for, merged. A stretch counts only while it is
/// current: read within [`RANGE_FRESH_DAYS`], or after (a day short of) its own end.
pub fn complete_spans(
    c: &Connection,
    league: &str,
    now: OffsetDateTime,
) -> Result<Vec<(OffsetDateTime, OffsetDateTime)>> {
    let mut stmt = c.prepare(
        "SELECT from_utc, to_utc, read_utc FROM sport_ranges WHERE league = ?1 ORDER BY from_utc",
    )?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([league], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut spans: Vec<(OffsetDateTime, OffsetDateTime)> = vec![];
    for (from, to, read) in rows {
        let (Some(from), Some(to), Some(read)) =
            (parse(Some(from)), parse(Some(to)), parse(Some(read)))
        else {
            continue;
        };
        let current = now - read <= time::Duration::days(RANGE_FRESH_DAYS)
            || read >= to - time::Duration::days(1);
        if !current {
            continue;
        }
        match spans.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => spans.push((from, to)),
        }
    }
    Ok(spans)
}

/// The team list could not be read: not asked for again before `next_try`.
pub fn note_teams_failed(c: &Connection, league: &str, next_try: OffsetDateTime) -> Result<()> {
    c.execute(
        "INSERT INTO sport_refresh (league, teams_failures, teams_next_try_utc) VALUES (?1, 1, ?2)
         ON CONFLICT (league) DO UPDATE SET teams_failures = teams_failures + 1, teams_next_try_utc = ?2",
        params![league, ts(next_try)],
    )?;
    Ok(())
}
