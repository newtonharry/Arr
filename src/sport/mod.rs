//! Sport: the schedules of leagues and the leagues and teams a person follows.
//!
//! This is a module of its own, next to movies and series rather than inside them. Phase one
//! (this one) only knows what is on and when: a built-in list of leagues (`catalog`), a gentle
//! client for ESPN's public schedule feed (`espn`), tables for follows and events (`data`) and
//! the refresh that keeps them current (`refresh`). Release names are read (`release`), judged
//! against a stored event (`matcher`, with the name rules in `names`) and searched for (`find`),
//! read-only. Nothing here downloads, files a download or touches Plex.
//!
//! No spoilers: scores, winners and result text are not parsed, not stored and not served.
//! Event names are the fixture ("Lakers at Bucks"), never the outcome.

pub mod catalog;
pub mod data;
pub mod espn;
pub mod find;
pub mod gate;
#[cfg(test)]
mod match_tests;
pub mod matcher;
pub mod names;
pub mod refresh;
pub mod release;
#[cfg(test)]
mod review2_tests;
#[cfg(test)]
mod review3_tests;
#[cfg(test)]
mod review4_tests;
#[cfg(test)]
mod review5_tests;
#[cfg(test)]
mod review6_tests;
#[cfg(test)]
mod review_tests;

use crate::config::SportsConfig;
use catalog::League;
use data::{EventRow, FollowRow};
use serde_json::{Value, json};

/// Which part of an event a file holds. A game or a golf tournament is `Whole`; a fight card
/// comes in parts that releases carry separately; a golf round is `Round`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Part {
    Whole,
    Main,
    Prelims,
    EarlyPrelims,
    Round(u8),
}

impl Part {
    /// `main`, `prelims`, `early-prelims`, `r1`..`r4`; `None` for a whole event.
    pub fn slug(self) -> Option<String> {
        Some(match self {
            Part::Whole => return None,
            Part::Main => "main".into(),
            Part::Prelims => "prelims".into(),
            Part::EarlyPrelims => "early-prelims".into(),
            Part::Round(n) => format!("r{n}"),
        })
    }

    pub fn from_slug(s: &str) -> Option<Part> {
        Some(match s {
            "main" => Part::Main,
            "prelims" => Part::Prelims,
            "early-prelims" => Part::EarlyPrelims,
            _ => {
                let n: u8 = s.strip_prefix('r')?.parse().ok()?;
                if !(1..=4).contains(&n) || s.len() != 2 {
                    return None;
                }
                Part::Round(n)
            }
        })
    }

    /// What a person calls it.
    pub fn label(self) -> String {
        match self {
            Part::Whole => "Full event".into(),
            Part::Main => "Main card".into(),
            Part::Prelims => "Prelims".into(),
            Part::EarlyPrelims => "Early prelims".into(),
            Part::Round(n) => format!("Round {n}"),
        }
    }
}

/// What a person is told about a copy Arr could not fully check, in plain words, by the reason
/// the matcher gave. There is no note for a copy with nothing to say: absence of a caution is
/// not a verification.
pub fn caution_note(reason: &str) -> &'static str {
    match reason {
        "unlabelled_main" => {
            "The title doesn't say which part this is. It's probably the main card."
        }
        "unlabelled_full" => {
            "The title doesn't say which part this is. It's probably the whole event."
        }
        "unlabelled_whole" => "The title doesn't say which part this is.",
        "rematch_unverified" => {
            "Check it's the right fight: the rematch number couldn't be checked."
        }
        "date_only" => "Only the date matched, not the names.",
        "place_only" => "Only the place matched, not the names.",
        "another_fixture_fits" => {
            "Another event fits this title as well. Check it's the right one."
        }
        "schedule_unknown" => {
            "Arr can't rule out another event on those days. Check it's the right one."
        }
        "unverified_text" => "Some of the title couldn't be checked.",
        "no_uploader_tag" => "The title has no uploader's name to go by.",
        "late_upload" => "It was put up long after the event.",
        "undated" => "Nothing says when it was put up.",
        "serial_unverified" => "Its number isn't one Arr can check.",
        "number_unverified" => "Its number isn't one Arr can check.",
        "series_unverified" => "Arr can't check that this is the series the title says.",
        "names_unverified" | "sides_unverified" => {
            "Arr doesn't know who is on the card, so the names couldn't be checked."
        }
        "head_unverified" => "The place in the title couldn't be checked.",
        "season_unverified" => "The season in the title couldn't be checked.",
        "round_unverified" => "The round or stage in the title couldn't be checked.",
        _ => "Some of the title couldn't be checked.",
    }
}

/// The schedule source, shared by the API and the worker.
pub struct Sports {
    pub espn: espn::Espn,
    /// How often the indexers are asked for the copies of an event (see `gate`).
    pub copies: gate::CopiesGate,
    /// How many requests may be judging a search at once (see `find::judge_search_bounded`):
    /// judging runs off the async workers, and only this many at a time.
    pub judging: std::sync::Arc<tokio::sync::Semaphore>,
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
            copies: gate::CopiesGate::default(),
            judging: std::sync::Arc::new(tokio::sync::Semaphore::new(find::JUDGE_SLOTS)),
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
        // When Arr reckons it is over (the source's end, or the start plus a length typical for
        // the sport). Copies are looked for only after it.
        "expected_end": catalog::find(&e.league)
            .and_then(|l| find::expected_end(e, l.sport))
            .map(crate::import::rfc3339),
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
    use super::catalog;
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
        let m = parse_month(BOARD.as_bytes(), catalog::find("premier-league").unwrap()).unwrap();
        assert_eq!(
            m.fixtures.len(),
            1,
            "events without a usable id or time are skipped"
        );
        let f = &m.fixtures[0];
        assert_eq!(f.name.as_deref(), Some("Manchester City v Fulham"));
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
