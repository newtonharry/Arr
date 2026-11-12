//! Sport: the schedules of leagues and the leagues and teams a person follows.
//!
//! This is a module of its own, next to movies and series rather than inside them. Phase one
//! (this one) only knows what is on and when: a built-in list of leagues (`catalog`), a gentle
//! client for ESPN's public schedule feed (`espn`), tables for follows and events (`data`) and
//! the refresh that keeps them current (`refresh`). Nothing here downloads, matches releases
//! or touches Plex.
//!
//! No spoilers: scores, winners and result text are not parsed, not stored and not served.
//! Event names are the fixture ("Lakers at Bucks"), never the outcome.

pub mod catalog;
pub mod data;
pub mod espn;
pub mod refresh;

use crate::config::SportsConfig;
use catalog::League;
use data::{EventRow, FollowRow};
use serde_json::{Value, json};

/// The schedule source, shared by the API and the worker.
pub struct Sports {
    pub espn: espn::Espn,
    /// One turn per league: a league is read by one caller at a time (a page, a follow or
    /// the worker), and the ones behind it find it freshly read.
    turns: std::sync::Mutex<
        std::collections::HashMap<&'static str, std::sync::Arc<tokio::sync::Mutex<()>>>,
    >,
}

impl Sports {
    pub fn new(cfg: &SportsConfig) -> Self {
        Sports {
            espn: espn::Espn::new(cfg),
            turns: Default::default(),
        }
    }

    pub(crate) fn league_lock(&self, slug: &'static str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(slug)
            .or_default()
            .clone()
    }
}

/// A league as the API shows it.
pub fn league_json(l: &League) -> Value {
    json!({
        "id": l.id(),
        "slug": l.slug,
        "name": l.name,
        "sport": l.sport.as_str(),
        "sport_label": l.sport.label(),
        "has_teams": l.teams,
    })
}

/// A game, fight or tournament as the API shows it. Times are UTC; the app shows them in the
/// viewer's own time.
pub fn event_json(e: &EventRow) -> Value {
    let side = |id: &Option<String>, name: &Option<String>| match (id, name) {
        (Some(id), Some(name)) => json!({ "id": id, "name": name }),
        _ => Value::Null,
    };
    json!({
        "id": e.id,
        "league": catalog::find(&e.league).map_or_else(|| json!({ "slug": e.league }), league_json),
        "name": e.name,
        "short_name": e.short_name,
        "starts_at": e.starts_at,
        "ends_at": e.ends_at,
        "state": e.state,
        "home": side(&e.home_id, &e.home_name),
        "away": side(&e.away_id, &e.away_name),
        "round": e.round,
        "venue": e.venue,
        "season": e.season,
    })
}

/// A follow as the API shows it.
pub fn follow_json(f: &FollowRow) -> Value {
    json!({
        "league": catalog::find(&f.league).map_or_else(|| json!({ "slug": f.league }), league_json),
        "team": f.team_id.as_ref().map(|id| json!({ "id": id, "name": f.team_name })),
        "since": f.since,
    })
}

#[cfg(test)]
mod tests {
    use super::espn::parse_month;

    /// The feed's scoreboard shape, with the fields that give a result away.
    const BOARD: &str = r#"{"events":[{"id":"401","date":"2026-11-21T12:30Z","name":"Fulham at Manchester City",
        "shortName":"FUL @ MNC","season":{"year":2026},"status":{"type":{"name":"STATUS_FINAL","state":"post"}},
        "competitions":[{"venue":{"fullName":"Etihad Stadium"},"notes":[{"headline":"Round 12"}],
        "competitors":[{"homeAway":"home","winner":true,"score":"3","team":{"id":"382","displayName":"Manchester City"}},
                       {"homeAway":"away","winner":false,"score":"0","team":{"id":"160","displayName":"Fulham"}}]}]},
        {"id":"0402","date":"2026-11-22T12:30Z","name":"Bad id"},
        {"id":"403","date":"soon","name":"Bad date"}]}"#;

    #[test]
    fn a_scoreboard_is_read_without_its_result() {
        let m = parse_month(BOARD.as_bytes()).unwrap();
        assert_eq!(
            m.fixtures.len(),
            1,
            "events without a usable id or time are skipped"
        );
        let f = &m.fixtures[0];
        assert_eq!(f.name, "Fulham at Manchester City");
        assert_eq!(f.home.as_ref().unwrap().name, "Manchester City");
        assert_eq!(f.away.as_ref().unwrap().id, "160");
        assert_eq!(f.round.as_deref(), Some("Round 12"));
        assert_eq!(f.venue.as_deref(), Some("Etihad Stadium"));
        assert_eq!(f.state.as_str(), "played");
        // Whatever the fixture holds, it holds no number from the result.
        let debug = format!("{f:?}");
        assert!(
            !debug.contains("score") && !debug.contains("winner"),
            "{debug}"
        );
    }
}
