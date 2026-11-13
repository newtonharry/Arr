//! Sport: the league list, following, the schedule refresh and what the API shows. The schedule
//! source is a loopback fake (`/espn/...` in the shared mock) that serves trimmed recordings of
//! the real feed, scores and winners included, so these tests can prove none of that is kept.
#![recursion_limit = "256"]

mod common;

use arr::sport::data;
use common::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

const EPL: &str = "soccer/eng.1";
const UFC: &str = "mma/ufc";

/// Manchester City v Fulham, nine days after the recording day.
const FULHAM_AT_CITY: &str = "401879233";
/// Newcastle United v Arsenal, the same day.
const ARSENAL_AT_NEWCASTLE: &str = "401879232";

async fn follow(e: &Exec, body: Value) -> (u16, Value) {
    let (s, v, _) = e.json(e.post("/api/v1/sport/follow").json(&body)).await;
    (s, v)
}

async fn get(e: &Exec, path: &str) -> (u16, Value) {
    let (s, v, _) = e.json(e.get(path)).await;
    (s, v)
}

/// `/sport/events` for a window from thirty days ago to sixty days ahead.
fn wide_events() -> String {
    let f = &time::format_description::well_known::Rfc3339;
    let now = OffsetDateTime::now_utc();
    format!(
        "/api/v1/sport/events?from={}&to={}",
        (now - time::Duration::days(30)).format(f).unwrap(),
        (now + time::Duration::days(60)).format(f).unwrap()
    )
}

fn names(events: &Value) -> Vec<String> {
    events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

async fn stored(e: &Exec, sql: &'static str) -> Vec<String> {
    e.state
        .store
        .with(move |c| {
            let mut stmt = c.prepare(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(match r.get::<_, rusqlite::types::Value>(0)? {
                        rusqlite::types::Value::Text(t) => t,
                        rusqlite::types::Value::Integer(i) => i.to_string(),
                        other => format!("{other:?}"),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
}

async fn count(e: &Exec, table: &'static str) -> usize {
    stored(
        e,
        match table {
            "sport_events" => "SELECT COUNT(*) FROM sport_events",
            "sport_teams" => "SELECT COUNT(*) FROM sport_teams",
            _ => panic!("{table}"),
        },
    )
    .await[0]
        .parse()
        .unwrap()
}

/// Follows a league straight in the store, without the immediate read the API does.
async fn follow_quietly(e: &Exec, league: &'static str, now: OffsetDateTime) {
    e.state
        .store
        .with(move |c| data::follow(c, league, None, now).map(|_| ()))
        .await
        .unwrap();
}

async fn refresh_run(e: &Exec, now: OffsetDateTime) -> Value {
    // The followed leagues only; the team lists of the catalogue have their own tests below.
    arr::sport::refresh::run_schedules(
        e.state.store.clone(),
        e.state.sports.clone().unwrap(),
        now,
        Duration::from_secs(45),
    )
    .await
    .0
}

async fn full_run(e: &Exec, now: OffsetDateTime) -> Value {
    arr::sport::refresh::run(e.state.store.clone(), e.state.sports.clone().unwrap(), now).await
}

fn scoreboard_requests(e: &Exec) -> Vec<String> {
    e.mock
        .lock()
        .unwrap()
        .espn_log
        .iter()
        .filter(|l| l.contains("/scoreboard"))
        .cloned()
        .collect()
}

// ------------------------------------------------------------------ the league list and search

#[tokio::test]
async fn the_league_list_covers_the_sports_and_says_what_is_followed() {
    let e = start_exec(ExecOptions::default()).await;
    let (s, v) = get(&e, "/api/v1/sport/leagues").await;
    assert_eq!(s, 200, "{v}");
    let leagues = v["leagues"].as_array().unwrap();
    let slugs: Vec<&str> = leagues
        .iter()
        .map(|l| l["slug"].as_str().unwrap())
        .collect();
    for want in [
        "premier-league",
        "a-league-men",
        "champions-league",
        "nba",
        "nbl",
        "nrl",
        "super-rugby-pacific",
        "urc",
        "gallagher-premiership",
        "six-nations",
        "rugby-championship",
        "rugby-internationals",
        "rugby-world-cup",
        "top-14",
        "pga-tour",
        "dp-world-tour",
        "liv-golf",
        "ufc",
        "pfl",
    ] {
        assert!(slugs.contains(&want), "{want} missing from {slugs:?}");
    }
    let nba = leagues.iter().find(|l| l["slug"] == "nba").unwrap();
    assert_eq!(
        nba,
        &json!({ "id": "league:nba", "slug": "nba", "name": "NBA", "sport": "basketball",
                 "sport_label": "Basketball", "has_teams": true, "followed": false, "followed_teams": 0 })
    );
    let ufc = leagues.iter().find(|l| l["slug"] == "ufc").unwrap();
    assert_eq!(
        (ufc["sport"].as_str(), ufc["has_teams"].as_bool()),
        (Some("mma"), Some(false))
    );
    // Listing the leagues asks the schedule source for nothing.
    assert_eq!(espn_requests(&e.mock), 0);

    let (s, _) = follow(&e, json!({ "league": "nba" })).await;
    assert_eq!(s, 201);
    let (_, v) = get(&e, "/api/v1/sport/leagues").await;
    let nba = v["leagues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["slug"] == "nba")
        .unwrap();
    assert_eq!(nba["followed"], true);
}

#[tokio::test]
async fn search_finds_a_league_by_alias_and_a_team_by_name() {
    let e = start_exec(ExecOptions::default()).await;
    let (s, v) = get(&e, "/api/v1/sport/search?q=EPL").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["leagues"][0]["slug"], "premier-league");
    assert_eq!(v["teams"], json!([]));
    let (_, v) = get(&e, "/api/v1/sport/search?q=ultimate%20fighting").await;
    assert_eq!(v["leagues"][0]["slug"], "ufc");
    let (_, v) = get(&e, "/api/v1/sport/search?q=rugby").await;
    assert!(v["leagues"].as_array().unwrap().len() >= 8, "{v}");

    // Teams are known once a league has been opened.
    assert_eq!(
        espn_requests(&e.mock),
        0,
        "searching asks the source for nothing"
    );
    let (s, _) = get(&e, "/api/v1/sport/league/premier-league").await;
    assert_eq!(s, 200);
    let (_, v) = get(&e, "/api/v1/sport/search?q=man%20city").await;
    assert_eq!(v["teams"][0]["team"]["name"], "Manchester City", "{v}");
    assert_eq!(v["teams"][0]["team"]["id"], "382");
    assert_eq!(v["teams"][0]["team"]["followed"], false);
    assert_eq!(v["teams"][0]["league"]["slug"], "premier-league");
    // By abbreviation, and by the start of a word.
    let (_, v) = get(&e, "/api/v1/sport/search?q=MNC").await;
    assert_eq!(v["teams"][0]["team"]["id"], "382");
    let (_, v) = get(&e, "/api/v1/sport/search?q=chel").await;
    assert_eq!(v["teams"][0]["team"]["name"], "Chelsea");

    let (s, v) = get(&e, "/api/v1/sport/search").await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (400, Some("invalid_query"))
    );
}

// ------------------------------------------------------------------ following

#[tokio::test]
async fn following_a_league_shows_its_upcoming_events_at_once() {
    let e = start_exec(ExecOptions::default()).await;
    let started = Instant::now();
    let (s, v) = follow(&e, json!({ "league": "premier-league" })).await;
    assert_eq!(s, 201, "{v}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(v["created"], true);
    assert_eq!(v["schedule_read"], true);
    assert_eq!(v["follow"]["league"]["slug"], "premier-league");
    assert_eq!(v["follow"]["team"], Value::Null);
    assert!(v["follow"]["since"].as_str().unwrap().ends_with('Z'));
    // Fixtures nine and ten days ahead: nine of them, inside the next three weeks.
    assert_eq!(v["upcoming"], 9, "{v}");
    assert_eq!(v["next_event"]["name"], "Manchester City v Fulham");

    let (s, v) = get(&e, "/api/v1/sport/events").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(names(&v["events"]).len(), 9);
    let first = &v["events"][0];
    assert_eq!(
        first["league"],
        json!({ "id": "league:premier-league", "slug": "premier-league", "name": "Premier League",
                "sport": "soccer", "sport_label": "Soccer", "has_teams": true })
    );
    assert_eq!(
        first["id"],
        format!("event:premier-league:{FULHAM_AT_CITY}")
    );
    assert_eq!(first["name"], "Manchester City v Fulham");
    assert_eq!(
        first["home"],
        json!({ "id": "382", "name": "Manchester City" })
    );
    assert_eq!(first["away"]["name"], "Fulham");
    assert_eq!(first["venue"], "Etihad Stadium");
    assert_eq!(first["state"], "scheduled");
    assert_eq!(first["ends_at"], Value::Null);
    let start = OffsetDateTime::parse(
        first["starts_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert_eq!(start, espn_start(&e.mock, EPL, FULHAM_AT_CITY));

    // At most two schedule requests, each for a whole month with a page limit, then the teams.
    let board = scoreboard_requests(&e);
    assert!((1..=2).contains(&board.len()), "{board:?}");
    for r in &board {
        assert!(
            r.contains("dates=20") && r.contains("limit=400") && !r.contains('-'),
            "{r}"
        );
    }
    assert_eq!(espn_requests(&e.mock), board.len() + 1);

    // Following again changes nothing and asks the source for nothing more: the league was read
    // a moment ago.
    let before = espn_requests(&e.mock);
    let (s, v) = follow(&e, json!({ "league": "league:premier-league" })).await;
    assert_eq!((s, &v["created"]), (200, &json!(false)), "{v}");
    assert_eq!(espn_requests(&e.mock), before);
    let (_, v) = get(&e, "/api/v1/sport/following").await;
    assert_eq!(v["following"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["following"][0]["next_event"]["name"],
        "Manchester City v Fulham"
    );
}

#[tokio::test]
async fn follows_survive_a_restart_and_unfollowing_removes_them() {
    let mut e = start_exec(ExecOptions::default()).await;
    follow(&e, json!({ "league": "nba" })).await;
    follow(&e, json!({ "league": "nba", "team_id": "13" })).await;
    e.restart().await;
    let (_, v) = get(&e, "/api/v1/sport/following").await;
    assert_eq!(v["following"].as_array().unwrap().len(), 2, "{v}");

    let (s, v, _) = e
        .json(
            e.post("/api/v1/sport/unfollow")
                .json(&json!({ "league": "nba" })),
        )
        .await;
    assert_eq!((s, &v["removed"]), (200, &json!(true)));
    let (_, v) = get(&e, "/api/v1/sport/following").await;
    assert_eq!(
        v["following"].as_array().unwrap().len(),
        1,
        "the team follow stays"
    );
    assert_eq!(v["following"][0]["team"]["name"], "Los Angeles Lakers");
    // The events that were read stay stored.
    assert!(count(&e, "sport_events").await > 0);
    let (_, v, _) = e
        .json(
            e.post("/api/v1/sport/unfollow")
                .json(&json!({ "league": "nba" })),
        )
        .await;
    assert_eq!(v["removed"], false);
}

#[tokio::test]
async fn follows_are_refused_cleanly() {
    let e = start_exec(ExecOptions::default()).await;
    // Golf and fighting have no teams.
    for league in ["pga-tour", "ufc"] {
        let (s, v) = follow(&e, json!({ "league": league, "team_id": "1" })).await;
        assert_eq!(
            (s, v["error"]["code"].as_str()),
            (400, Some("invalid_request")),
            "{v}"
        );
        assert!(v["error"]["message"].as_str().unwrap().contains("no teams"));
    }
    let (s, v) = follow(&e, json!({ "league": "curling" })).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (404, Some("not_found")),
        "{v}"
    );
    let (s, v) = follow(&e, json!({ "league": "nba", "team_id": "lakers" })).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (400, Some("invalid_request")),
        "{v}"
    );
    let (s, v) = follow(&e, json!({ "league": "nba", "team_id": "999999" })).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (404, Some("not_found")),
        "{v}"
    );
    let (s, v) = follow(&e, json!({ "league": "nba", "colour": "blue" })).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (422, Some("invalid_body")),
        "{v}"
    );
    let (s, v) = follow(&e, json!({})).await;
    assert_eq!(s, 422, "{v}");
    // Nothing was followed.
    let (_, v) = get(&e, "/api/v1/sport/following").await;
    assert_eq!(v["following"], json!([]));
    // A league that is not followed can be unfollowed without a fuss; a bad one cannot.
    let (s, v, _) = e
        .json(
            e.post("/api/v1/sport/unfollow")
                .json(&json!({ "league": "nba" })),
        )
        .await;
    assert_eq!((s, &v["removed"]), (200, &json!(false)));
    let (s, _, _) = e
        .json(
            e.post("/api/v1/sport/unfollow")
                .json(&json!({ "league": "curling" })),
        )
        .await;
    assert_eq!(s, 404);
    let (s, v) = get(&e, "/api/v1/sport/league/curling").await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")));
}

#[tokio::test]
async fn a_team_follow_only_brings_that_teams_events() {
    let e = start_exec(ExecOptions::default()).await;
    // 13 is the Lakers: one fixture in the next three weeks.
    let (s, v) = follow(&e, json!({ "league": "nba", "team_id": "13" })).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(
        v["follow"]["team"],
        json!({ "id": "13", "name": "Los Angeles Lakers" })
    );
    assert_eq!(v["upcoming"], 1, "{v}");
    let (_, v) = get(&e, "/api/v1/sport/events").await;
    assert_eq!(
        names(&v["events"]),
        ["Charlotte Hornets at Los Angeles Lakers"]
    );

    // The Trail Blazers have a game yesterday (over before the follow) and two to come.
    let (s, v) = follow(&e, json!({ "league": "nba", "team_id": "22" })).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["upcoming"], 2, "{v}");
    let (_, v) = get(&e, "/api/v1/sport/events?from=2000-01-01&to=2000-02-01").await;
    assert_eq!(v["events"], json!([]));
    let (_, v) = get(&e, "/api/v1/sport/events").await;
    let n = names(&v["events"]);
    assert_eq!(n.len(), 3, "{n:?}");
    assert!(
        n.iter()
            .all(|n| n.contains("Los Angeles Lakers") || n.contains("Portland Trail Blazers")),
        "{n:?}"
    );
    assert!(
        v["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["state"] == "scheduled")
    );

    // Following the league as well does not show anything twice.
    follow(&e, json!({ "league": "nba" })).await;
    let (_, v) = get(&e, "/api/v1/sport/events").await;
    let ids: std::collections::HashSet<&str> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), v["events"].as_array().unwrap().len());
    assert!(ids.len() > 3);

    // The league page marks the followed team.
    let (_, v) = get(&e, "/api/v1/sport/league/nba").await;
    assert_eq!(v["league"]["followed"], true);
    assert_eq!(v["league"]["followed_teams"], 2);
    let lakers = v["teams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "13")
        .unwrap();
    assert_eq!(lakers["followed"], true);
}

#[tokio::test]
async fn nothing_before_the_follow_time_is_wanted() {
    let e = start_exec(ExecOptions::default()).await;
    // Six NBA games were played yesterday. The league page may show them as played ...
    follow(&e, json!({ "league": "nba" })).await;
    let (_, v) = get(&e, "/api/v1/sport/league/nba").await;
    let recent = names(&v["recent"]);
    assert_eq!(recent.len(), 6, "{recent:?}");
    assert!(
        v["recent"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["state"] == "played")
    );
    // ... but a follow starts now: they are not events of the follow, even when asked for.
    let (_, v) = get(&e, &wide_events()).await;
    let all = names(&v["events"]);
    assert_eq!(all.len(), 8, "{all:?}");
    assert!(
        !all.iter()
            .any(|n| n == "Milwaukee Bucks at New York Knicks"),
        "{all:?}"
    );
    // Games from four days ago (EPL) were never even stored: the window starts yesterday.
    follow(&e, json!({ "league": "premier-league" })).await;
    let stored_names = stored(
        &e,
        "SELECT name FROM sport_events WHERE league = 'premier-league'",
    )
    .await;
    assert_eq!(stored_names.len(), 9, "{stored_names:?}");
    assert!(
        !stored_names
            .iter()
            .any(|n| n == "Brighton & Hove Albion v Brentford")
    );
}

// ------------------------------------------------------------------ the refresh

#[tokio::test]
async fn a_refresh_fills_events_and_reading_again_changes_nothing() {
    let e = start_exec(ExecOptions::default()).await;
    let now = OffsetDateTime::now_utc();
    follow_quietly(&e, "premier-league", now).await;
    let r = refresh_run(&e, now).await;
    assert_eq!(r["leagues"][0]["status"], "ok", "{r}");
    assert_eq!(r["leagues"][0]["added"], 9, "{r}");
    assert_eq!(count(&e, "sport_events").await, 9);
    assert_eq!(count(&e, "sport_teams").await, 19);
    let dump = stored(&e, "SELECT id || '|' || state || '|' || first_seen_utc || '|' || updated_utc FROM sport_events ORDER BY id").await;

    // Six hours later it is read again: same schedule, nothing added or changed, and not even
    // the times of last change move.
    let later = now + time::Duration::hours(7);
    let r = refresh_run(&e, later).await;
    assert_eq!(r["leagues"][0]["status"], "ok", "{r}");
    assert_eq!(
        (
            r["leagues"][0]["added"].as_u64(),
            r["leagues"][0]["changed"].as_u64()
        ),
        (Some(0), Some(0)),
        "{r}"
    );
    assert_eq!(r["leagues"][0]["unlisted"], 0);
    let again = stored(&e, "SELECT id || '|' || state || '|' || first_seen_utc || '|' || updated_utc FROM sport_events ORDER BY id").await;
    assert_eq!(dump, again);

    // A league read a moment ago is left alone, and so is one that is not followed.
    let asked = espn_requests(&e.mock);
    let r = refresh_run(&e, later + time::Duration::minutes(10)).await;
    assert_eq!(r["due"], 0, "{r}");
    assert_eq!(espn_requests(&e.mock), asked);
}

#[tokio::test]
async fn an_event_that_disappears_upstream_is_unlisted_never_cancelled() {
    let e = start_exec(ExecOptions::default()).await;
    let now = OffsetDateTime::now_utc();
    follow_quietly(&e, "premier-league", now).await;
    refresh_run(&e, now).await;
    espn_remove(&e.mock, EPL, FULHAM_AT_CITY);
    let r = refresh_run(&e, now + time::Duration::hours(7)).await;
    assert_eq!(r["leagues"][0]["unlisted"], 1, "{r}");
    let states = stored(
        &e,
        "SELECT state FROM sport_events WHERE id = 'event:premier-league:401879233'",
    )
    .await;
    assert_eq!(states, ["unlisted"], "absence never cancels");
    assert_eq!(count(&e, "sport_events").await, 9);
    // An unlisted event is in no answer at all.
    let (_, v) = get(&e, "/api/v1/feed").await;
    assert_eq!(v["sport"].as_array().unwrap().len(), 8, "{v}");
    let (_, v) = get(&e, "/api/v1/sport/events").await;
    assert_eq!(v["events"].as_array().unwrap().len(), 8);
    assert!(!names(&v["events"]).contains(&"Manchester City v Fulham".to_string()));
    let (_, v) = get(&e, "/api/v1/sport/league/premier-league").await;
    assert_eq!(v["upcoming"].as_array().unwrap().len(), 8);
    let (_, v) = get(&e, "/api/v1/sport/following").await;
    assert_ne!(
        v["following"][0]["next_event"]["name"],
        "Manchester City v Fulham"
    );
    // Reading it again changes nothing further.
    let r = refresh_run(&e, now + time::Duration::hours(14)).await;
    assert_eq!(r["leagues"][0]["unlisted"], 0, "{r}");

    // When the source lists it again, the same row is back.
    let mut again = {
        let m = e.mock.lock().unwrap();
        m.espn_events[EPL][0].clone()
    };
    again["competitions"][0]["competitors"] = json!([]);
    again["id"] = json!(FULHAM_AT_CITY);
    again["name"] = json!("Fulham at Manchester City");
    again["date"] = json!("2026-11-21T12:30Z");
    again["status"]["type"] = json!({ "name": "STATUS_SCHEDULED", "state": "pre" });
    e.mock
        .lock()
        .unwrap()
        .espn_events
        .get_mut(EPL)
        .unwrap()
        .push(again);
    refresh_run(&e, now + time::Duration::hours(21)).await;
    let states = stored(
        &e,
        "SELECT state FROM sport_events WHERE id = 'event:premier-league:401879233'",
    )
    .await;
    assert_eq!(states, ["scheduled"]);
    assert_eq!(count(&e, "sport_events").await, 9);

    // Only the source can cancel.
    espn_set_status(&e.mock, EPL, FULHAM_AT_CITY, "STATUS_CANCELED", "post");
    let r = refresh_run(&e, now + time::Duration::hours(28)).await;
    assert_eq!(r["leagues"][0]["changed"], 1, "{r}");
    let (_, v) = get(&e, "/api/v1/sport/events").await;
    let cancelled: Vec<&Value> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["state"] == "cancelled")
        .collect();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0]["name"], "Fulham at Manchester City");
    let (_, v) = get(&e, "/api/v1/feed").await;
    assert_eq!(v["sport"].as_array().unwrap().len(), 8, "{v}");

    // An empty answer is not taken to mean anything is gone.
    {
        let mut m = e.mock.lock().unwrap();
        m.espn_events.get_mut(EPL).unwrap().clear();
    }
    let r = refresh_run(&e, now + time::Duration::hours(35)).await;
    assert_eq!(r["leagues"][0]["unlisted"], 0, "{r}");
    let live = stored(
        &e,
        "SELECT COUNT(*) FROM sport_events WHERE state = 'scheduled'",
    )
    .await;
    assert_eq!(live, ["8"]);
}

#[tokio::test]
async fn a_postponed_event_says_so_and_comes_back_when_it_is_scheduled_again() {
    let e = start_exec(ExecOptions::default()).await;
    let now = OffsetDateTime::now_utc();
    follow_quietly(&e, "premier-league", now).await;
    refresh_run(&e, now).await;
    espn_set_status(
        &e.mock,
        EPL,
        ARSENAL_AT_NEWCASTLE,
        "STATUS_POSTPONED",
        "post",
    );
    let r = refresh_run(&e, now + time::Duration::hours(7)).await;
    assert_eq!(r["leagues"][0]["changed"], 1, "{r}");
    let (_, v) = get(&e, "/api/v1/feed").await;
    let sport = v["sport"].as_array().unwrap();
    assert_eq!(
        sport.len(),
        9,
        "a postponed game is still listed, as postponed"
    );
    let postponed = sport.iter().find(|e| e["state"] == "postponed").unwrap();
    assert_eq!(postponed["name"], "Newcastle United v Arsenal");
    espn_set_status(
        &e.mock,
        EPL,
        ARSENAL_AT_NEWCASTLE,
        "STATUS_SCHEDULED",
        "pre",
    );
    refresh_run(&e, now + time::Duration::hours(14)).await;
    let states = stored(
        &e,
        "SELECT state FROM sport_events WHERE id = 'event:premier-league:401879232'",
    )
    .await;
    assert_eq!(states, ["scheduled"]);
}

#[tokio::test]
async fn a_played_game_is_marked_played_when_it_is_read_again() {
    let e = start_exec(ExecOptions::default()).await;
    let now = OffsetDateTime::now_utc();
    follow_quietly(&e, "premier-league", now).await;
    refresh_run(&e, now).await;
    espn_set_status(
        &e.mock,
        EPL,
        ARSENAL_AT_NEWCASTLE,
        "STATUS_FULL_TIME",
        "post",
    );
    refresh_run(&e, now + time::Duration::hours(7)).await;
    let states = stored(
        &e,
        "SELECT state FROM sport_events WHERE id = 'event:premier-league:401879232'",
    )
    .await;
    assert_eq!(states, ["played"]);
}

#[tokio::test]
async fn a_league_with_a_game_about_to_start_is_read_more_often() {
    let e = start_exec(ExecOptions::default()).await;
    let start = espn_start(&e.mock, EPL, FULHAM_AT_CITY);
    let first = start - time::Duration::minutes(210);
    follow_quietly(&e, "premier-league", first).await;
    refresh_run(&e, first).await;
    // Far from any game, twenty-five minutes later is too soon ...
    let asked = espn_requests(&e.mock);
    let r = refresh_run(&e, first + time::Duration::minutes(25)).await;
    assert_eq!(r["due"], 0, "{r}");
    assert_eq!(espn_requests(&e.mock), asked);
    // ... within two hours of a followed game it is not (a postponement, or the end of the
    // game, shows up soon), though not every few minutes either.
    let near = start - time::Duration::minutes(90);
    let r = refresh_run(&e, near).await;
    assert_eq!(r["due"], 1, "{r}");
    let r = refresh_run(&e, near + time::Duration::minutes(10)).await;
    assert_eq!(r["due"], 0, "{r}");
    let r = refresh_run(&e, near + time::Duration::minutes(25)).await;
    assert_eq!(r["due"], 1, "{r}");
}

#[tokio::test]
async fn the_worker_runs_the_refresh_and_not_before_it_is_due() {
    let e = start_exec(ExecOptions::default()).await;
    let now = OffsetDateTime::now_utc();
    follow_quietly(&e, "nba", now).await;
    follow_quietly(&e, "ufc", now).await;
    let report = e.tick_at(now).await;
    assert!(report.tasks.contains(&"sports".to_string()), "{report:?}");
    // Fourteen NBA games (six played yesterday) and the two cards still to come.
    assert_eq!(count(&e, "sport_events").await, 16);
    // Not due again for a quarter of an hour.
    let report = e.tick_at(now + time::Duration::minutes(1)).await;
    assert!(!report.tasks.contains(&"sports".to_string()), "{report:?}");
    let status = stored(&e, "SELECT last_status FROM tasks WHERE name = 'sports'").await;
    assert_eq!(status, ["ok"]);
}

// ------------------------------------------------------------------ the source misbehaving

#[tokio::test]
async fn a_source_that_fails_never_fails_following_the_page_or_the_worker() {
    let e = start_exec(ExecOptions::default()).await;
    e.mock.lock().unwrap().espn_fail = Some(500);
    let started = Instant::now();
    let (s, v) = follow(&e, json!({ "league": "premier-league" })).await;
    assert_eq!(
        s, 201,
        "following is recorded even when the schedule cannot be read: {v}"
    );
    assert_eq!(v["schedule_read"], false);
    assert_eq!(v["upcoming"], 0);
    assert!(started.elapsed() < Duration::from_secs(5));
    let (s, v) = get(&e, "/api/v1/sport/league/premier-league").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["upcoming"], json!([]));
    assert!(
        v["schedule"]["problem"]
            .as_str()
            .unwrap()
            .contains("HTTP 500"),
        "{v}"
    );
    assert_eq!(v["schedule"]["updated_at"], Value::Null);
    for path in [
        "/api/v1/sport/events",
        "/api/v1/sport/following",
        "/api/v1/feed",
        "/api/v1/calendar",
    ] {
        let (s, v) = get(&e, path).await;
        assert_eq!(s, 200, "{path}: {v}");
    }

    // The worker pass survives, records the failure and leaves the league alone for a while.
    let now = OffsetDateTime::now_utc();
    let report = e.tick_at(now + time::Duration::hours(1)).await;
    assert!(report.tasks.contains(&"sports".to_string()), "{report:?}");
    let asked = espn_requests(&e.mock);
    let r = refresh_run(&e, now + time::Duration::minutes(5)).await;
    assert_eq!(r["due"], 0, "waiting out the failure: {r}");
    assert_eq!(espn_requests(&e.mock), asked);

    // The source comes back; the next time the league is due, it is read.
    e.mock.lock().unwrap().espn_fail = None;
    let r = refresh_run(&e, now + time::Duration::hours(30)).await;
    assert_eq!(r["leagues"][0]["status"], "ok", "{r}");
    assert_eq!(count(&e, "sport_events").await, 9);
}

#[tokio::test]
async fn stored_events_stay_when_the_source_goes_down_later() {
    let e = start_exec(ExecOptions::default()).await;
    follow(&e, json!({ "league": "premier-league" })).await;
    let now = OffsetDateTime::now_utc();
    e.mock.lock().unwrap().espn_fail = Some(503);
    let r = refresh_run(&e, now + time::Duration::hours(7)).await;
    assert_eq!(r["leagues"][0]["status"], "failed", "{r}");
    assert!(
        r["leagues"][0]["error"]
            .as_str()
            .unwrap()
            .contains("HTTP 503")
    );
    assert!(!r.to_string().contains("127.0.0.1"), "no address in {r}");
    assert_eq!(count(&e, "sport_events").await, 9);
    let (_, v) = get(&e, "/api/v1/sport/events").await;
    assert_eq!(v["events"].as_array().unwrap().len(), 9);
    let (_, v) = get(&e, "/api/v1/feed").await;
    assert_eq!(v["sport"].as_array().unwrap().len(), 9);
}

#[tokio::test]
async fn a_slow_source_is_waited_for_only_a_little() {
    let e = start_exec(ExecOptions::default()).await;
    e.mock.lock().unwrap().espn_delay = Some(Duration::from_secs(4));
    let started = Instant::now();
    let (s, v) = follow(&e, json!({ "league": "nba" })).await;
    // The request timeout is two seconds: the follow answers without a schedule.
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["schedule_read"], false);
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "{:?}",
        started.elapsed()
    );
    let (s, _) = get(&e, "/api/v1/sport/following").await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn several_followed_leagues_cost_little_when_the_source_is_down() {
    let e = start_exec(ExecOptions::default()).await;
    let now = OffsetDateTime::now_utc();
    for league in ["premier-league", "nba", "ufc", "nrl", "mls"] {
        follow_quietly(&e, league, now).await;
    }
    e.mock.lock().unwrap().espn_fail = Some(500);
    let r = refresh_run(&e, now).await;
    // Two leagues failed in a row, then the run stopped instead of asking three more times.
    assert_eq!(r["leagues"].as_array().unwrap().len(), 2, "{r}");
    assert_eq!(r["stopped"], "the schedule source is failing");
    assert_eq!(espn_requests(&e.mock), 2);
}

#[tokio::test]
async fn a_failure_pauses_all_requests_for_a_while() {
    let e = start_exec(ExecOptions {
        sports_backoff_secs: 60,
        ..Default::default()
    })
    .await;
    e.mock.lock().unwrap().espn_fail = Some(500);
    follow(&e, json!({ "league": "nba" })).await;
    assert_eq!(espn_requests(&e.mock), 1);
    // The source is not asked again for another league while the pause lasts.
    let (s, v) = follow(&e, json!({ "league": "premier-league" })).await;
    assert_eq!((s, &v["schedule_read"]), (201, &json!(false)));
    assert_eq!(espn_requests(&e.mock), 1);
    let (s, v) = get(&e, "/api/v1/sport/league/ufc").await;
    assert_eq!(s, 200, "{v}");
    assert!(
        v["schedule"]["problem"]
            .as_str()
            .unwrap()
            .contains("throttled"),
        "{v}"
    );
    assert_eq!(espn_requests(&e.mock), 1);
}

#[tokio::test]
async fn answers_are_reused_and_simultaneous_requests_share_one() {
    let e = start_exec(ExecOptions {
        sports_cache_secs: 60,
        ..Default::default()
    })
    .await;
    e.mock.lock().unwrap().espn_delay = Some(Duration::from_millis(300));
    let page = "/api/v1/sport/league/premier-league";
    let (a, b, c) = tokio::join!(get(&e, page), get(&e, page), get(&e, page));
    for (s, v) in [a, b, c] {
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["upcoming"].as_array().unwrap().len(), 9, "{v}");
    }
    let asked = espn_requests(&e.mock);
    assert!(asked <= 3, "{:?}", e.mock.lock().unwrap().espn_log);
    // The page again: nothing more is asked.
    get(&e, page).await;
    assert_eq!(espn_requests(&e.mock), asked);
}

// ------------------------------------------------------------------ the other sports' shapes

#[tokio::test]
async fn a_fight_card_a_tournament_and_a_rugby_match_have_their_own_shapes() {
    let e = start_exec(ExecOptions::default()).await;
    let (_, v) = follow(&e, json!({ "league": "ufc" })).await;
    assert_eq!(v["upcoming"], 2, "{v}");
    let (_, v) = follow(&e, json!({ "league": "dp-world-tour" })).await;
    assert_eq!(v["upcoming"], 2, "{v}");
    follow(&e, json!({ "league": "top-14" })).await;

    let (_, v) = get(&e, &wide_events()).await;
    let events = v["events"].as_array().unwrap();
    let card = events
        .iter()
        .find(|e| e["name"] == "UFC 334: Gane vs. Hokit")
        .unwrap();
    assert_eq!(card["league"]["sport"], "mma");
    assert_eq!((&card["home"], &card["away"]), (&Value::Null, &Value::Null));
    assert_eq!(card["state"], "scheduled");
    assert!(card["venue"].is_string(), "{card}");

    // A golf tournament runs over days; it is under way today and still counts.
    let golf = events
        .iter()
        .find(|e| e["name"] == "DP World Tour Championship")
        .unwrap();
    assert_eq!(golf["league"]["sport"], "golf");
    assert!(golf["ends_at"].is_string(), "{golf}");
    assert!(golf["ends_at"].as_str().unwrap() > golf["starts_at"].as_str().unwrap());

    let rugby = events
        .iter()
        .find(|e| e["name"] == "Lyon v Stade Toulousain")
        .unwrap();
    assert_eq!(rugby["league"]["sport"], "rugby_union");
    assert_eq!(rugby["home"]["name"], "Lyon");
    assert_eq!(rugby["away"]["name"], "Stade Toulousain");
    // The league page lists its teams.
    let (_, page) = get(&e, "/api/v1/sport/league/top-14").await;
    let teams = names(&page["teams"]);
    assert!(
        teams.len() >= 9 && teams.iter().any(|t| t == "Stade Toulousain"),
        "{teams:?}"
    );
    // Golf and MMA league pages list no teams.
    let (_, page) = get(&e, "/api/v1/sport/league/ufc").await;
    assert_eq!(page["teams"], json!([]));
    assert_eq!(page["league"]["has_teams"], false);
}

// ------------------------------------------------------------------ no spoilers

/// Every key of a JSON value, at any depth.
fn keys(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                out.push(k.clone());
                keys(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| keys(x, out)),
        _ => {}
    }
}

#[tokio::test]
async fn nothing_score_like_is_stored_or_served() {
    let e = start_exec(ExecOptions::default()).await;
    // The recordings really do carry results, or this test proves nothing.
    {
        let m = e.mock.lock().unwrap();
        let all = serde_json::to_string(&m.espn_events).unwrap();
        for tell in [
            "\"score\"",
            "\"winner\"",
            "\"linescores\"",
            "Unofficial Winner",
        ] {
            assert!(all.contains(tell), "the recordings lack {tell}");
        }
    }
    for league in ["premier-league", "nba", "ufc", "dp-world-tour", "top-14"] {
        let (s, v) = follow(&e, json!({ "league": league })).await;
        assert_eq!(s, 201, "{v}");
    }
    // Yesterday's NBA games are stored as played, so a result was on offer.
    let played = stored(
        &e,
        "SELECT COUNT(*) FROM sport_events WHERE state = 'played'",
    )
    .await;
    assert!(played[0].parse::<u32>().unwrap() >= 6, "{played:?}");

    // The tables have no column that could hold one ...
    let columns = stored(
        &e,
        "SELECT t.name || '.' || c.name FROM sqlite_master t, pragma_table_info(t.name) c WHERE t.type = 'table' AND t.name LIKE 'sport_%'",
    )
    .await;
    assert!(columns.len() > 20, "{columns:?}");
    for c in &columns {
        let c = c.to_lowercase();
        for bad in ["score", "winner", "result", "outcome", "point", "goals"] {
            assert!(!c.contains(bad), "{c}");
        }
    }
    // ... and no value looks like one. Every stored text, joined.
    let rows = stored(
        &e,
        "SELECT group_concat(COALESCE(name,'') || ' ' || COALESCE(short_name,'') || ' ' || COALESCE(round,'') || ' ' || COALESCE(venue,'') || ' ' || state, ' ') FROM sport_events",
    )
    .await;
    let text = rows.join(" ").to_lowercase();
    for bad in [
        "score",
        "winner",
        "unofficial",
        "kotko",
        "decision",
        " ko ",
        "defeat",
        "beat ",
    ] {
        assert!(!text.contains(bad), "{bad} in stored events");
    }

    // The answers carry no such keys or words.
    let mut answers = vec![];
    let wide = wide_events();
    for path in [
        wide.as_str(),
        "/api/v1/sport/following",
        "/api/v1/sport/league/nba",
        "/api/v1/sport/league/ufc",
        "/api/v1/sport/league/dp-world-tour",
        "/api/v1/sport/league/top-14",
        "/api/v1/sport/league/premier-league",
        "/api/v1/feed",
        "/api/v1/calendar?start=2000-01-01T00:00:00Z&end=2000-02-01T00:00:00Z",
        "/api/v1/calendar",
    ] {
        let (s, v, text) = e.json(e.get(path)).await;
        assert_eq!(s, 200, "{path}: {text}");
        let mut k = vec![];
        keys(&v, &mut k);
        for key in &k {
            let key = key.to_lowercase();
            for bad in [
                "score",
                "winner",
                "result",
                "linescore",
                "outcome",
                "form",
                "record",
            ] {
                assert!(!key.contains(bad), "{path} has key {key}");
            }
        }
        let t = text.to_lowercase();
        assert!(!t.contains("unofficial") && !t.contains("winner"), "{path}");
        answers.push(text);
    }
    assert!(answers.iter().any(|a| a.contains("Hornets")));
}

// ------------------------------------------------------------------ feed and calendar

#[tokio::test]
async fn the_feed_and_calendar_gain_a_separate_field_and_keep_the_rest() {
    let e = start_exec(ExecOptions::default()).await;
    let (s, feed0) = get(&e, "/api/v1/feed").await;
    assert_eq!(s, 200, "{feed0}");
    assert_eq!(feed0["sport"], json!([]));
    let (_, cal0) = get(&e, "/api/v1/calendar").await;
    assert_eq!(cal0["sport"], json!([]));

    follow(&e, json!({ "league": "premier-league" })).await;
    follow(&e, json!({ "league": "nba", "team_id": "13" })).await;
    let (_, feed) = get(&e, "/api/v1/feed").await;
    let sport = feed["sport"].as_array().unwrap();
    assert_eq!(sport.len(), 10, "{feed}");
    assert!(
        sport
            .windows(2)
            .all(|w| w[0]["starts_at"].as_str() <= w[1]["starts_at"].as_str())
    );
    assert!(sport.iter().all(|e| e["league"]["slug"] == "premier-league"
        || e["name"].as_str().unwrap().contains("Lakers")));
    // The existing fields are exactly what they were.
    let mut was: Vec<&String> = feed0.as_object().unwrap().keys().collect();
    let mut is: Vec<&String> = feed.as_object().unwrap().keys().collect();
    was.sort();
    is.sort();
    assert_eq!(was, is);
    for key in ["needs_you", "on_its_way", "arrived", "looking", "coming_up"] {
        assert_eq!(feed[key], feed0[key], "{key}");
    }

    // Calendar: the window decides. Three weeks ahead has them all, a window in the past none.
    let end = (OffsetDateTime::now_utc() + time::Duration::days(30))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let (_, cal) = get(&e, &format!("/api/v1/calendar?end={end}")).await;
    assert_eq!(cal["sport"].as_array().unwrap().len(), 10, "{cal}");
    assert_eq!(cal["items"], cal0["items"]);
    let (_, old) = get(
        &e,
        "/api/v1/calendar?start=2020-01-01T00:00:00Z&end=2020-02-01T00:00:00Z",
    )
    .await;
    assert_eq!(old["sport"], json!([]));
}

// ------------------------------------------------------------------ routes, schema, switch

#[tokio::test]
async fn the_routes_need_the_token_and_are_in_the_schema() {
    let e = start_exec(ExecOptions::default()).await;
    let routes = [
        ("get", "/api/v1/sport/search"),
        ("get", "/api/v1/sport/leagues"),
        ("get", "/api/v1/sport/league/{slug}"),
        ("post", "/api/v1/sport/follow"),
        ("post", "/api/v1/sport/unfollow"),
        ("get", "/api/v1/sport/following"),
        ("get", "/api/v1/sport/events"),
    ];
    let doc = arr::openapi::document();
    for (method, path) in routes {
        assert!(
            arr::api::ROUTES.contains(&(method, path)),
            "{method} {path} not in ROUTES"
        );
        assert!(
            doc["paths"][path][method].is_object(),
            "{method} {path} undocumented"
        );
        let url = format!("{}{}", e.base, path.replace("{slug}", "nba"));
        let r = if method == "get" {
            e.client.get(&url).send().await.unwrap()
        } else {
            e.client
                .post(&url)
                .json(&json!({ "league": "nba" }))
                .send()
                .await
                .unwrap()
        };
        assert_eq!(r.status().as_u16(), 401, "{method} {path}");
    }
    // The schema says what the new fields hold.
    let feed = &doc["paths"]["/api/v1/feed"]["get"]["responses"]["200"]["content"]["application/json"]
        ["schema"];
    assert!(
        feed["required"]
            .as_array()
            .unwrap()
            .contains(&json!("sport"))
    );
    let follow = &doc["paths"]["/api/v1/sport/follow"]["post"];
    assert!(follow["responses"]["201"].is_object());
    assert_eq!(
        follow["requestBody"]["content"]["application/json"]["schema"]["required"],
        json!(["league"])
    );
}

#[tokio::test]
async fn sport_can_be_turned_off() {
    let e = start_exec(ExecOptions {
        sports: false,
        ..Default::default()
    })
    .await;
    for path in [
        "/api/v1/sport/leagues",
        "/api/v1/sport/search?q=nba",
        "/api/v1/sport/league/nba",
        "/api/v1/sport/following",
        "/api/v1/sport/events",
    ] {
        let (s, v) = get(&e, path).await;
        assert_eq!(
            (s, v["error"]["code"].as_str()),
            (503, Some("provider_not_configured")),
            "{path}: {v}"
        );
    }
    let (s, v) = follow(&e, json!({ "league": "nba" })).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (503, Some("provider_not_configured")),
        "{v}"
    );
    // The home and the calendar simply have nothing from sport.
    let (s, v) = get(&e, "/api/v1/feed").await;
    assert_eq!((s, &v["sport"]), (200, &json!([])));
    let (_, v) = get(&e, "/api/v1/calendar").await;
    assert_eq!(v["sport"], json!([]));
    // The worker does not even list the task, and nothing was asked of the source.
    let report = e.tick().await;
    assert!(!report.tasks.contains(&"sports".to_string()), "{report:?}");
    assert_eq!(espn_requests(&e.mock), 0);
    let (_, caps) = get(&e, "/api/v1/capabilities").await;
    assert_eq!(caps["mode"], "execution");
    let cap = caps["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "sport_schedules")
        .unwrap();
    assert_eq!(cap["configured"], false);
}

// ------------------------------------------------------------------ review round 1

/// Events pinned to dates of their own (served as given): `(id, date, name)`.
fn pin_events(e: &Exec, specs: &[(&str, &str, &str)]) {
    let events: Vec<Value> = specs
        .iter()
        .map(|(id, date, name)| espn_event_like(&e.mock, EPL, id, date, name))
        .collect();
    espn_use_absolute(&e.mock, EPL, events);
}

async fn state_of(e: &Exec, espn_id: &str) -> String {
    let sql: &'static str = Box::leak(
        format!("SELECT state FROM sport_events WHERE id = 'event:premier-league:{espn_id}'")
            .into_boxed_str(),
    );
    stored(e, sql).await.remove(0)
}

const MARCH: [(&str, &str, &str); 4] = [
    ("9001", "2027-03-25T15:00Z", "Alpha at Beta"),
    ("9002", "2027-03-28T15:00Z", "Gamma at Delta"),
    ("9003", "2027-04-03T15:00Z", "Epsilon at Zeta"),
    ("9004", "2027-04-06T15:00Z", "Eta at Theta"),
];

/// Set up with four pinned events over two months, followed and read once at 20 March 2027.
async fn two_months() -> (Exec, OffsetDateTime) {
    let e = start_exec(ExecOptions::default()).await;
    let now = time::macros::datetime!(2027-03-20 12:00 UTC);
    pin_events(&e, &MARCH);
    follow_quietly(&e, "premier-league", now).await;
    let r = refresh_run(&e, now).await;
    assert_eq!(r["leagues"][0]["added"], 4, "{r}");
    (e, now)
}

#[tokio::test]
async fn an_empty_month_among_two_unlists_nothing_in_it() {
    let (e, now) = two_months().await;
    // April comes back empty while March is as it was: April's events are not judged gone.
    espn_remove(&e.mock, EPL, "9003");
    espn_remove(&e.mock, EPL, "9004");
    let r = refresh_run(&e, now + time::Duration::hours(7)).await;
    assert_eq!(r["leagues"][0]["status"], "ok", "{r}");
    assert_eq!(r["leagues"][0]["unlisted"], 0, "{r}");
    for id in ["9001", "9002", "9003", "9004"] {
        assert_eq!(state_of(&e, id).await, "scheduled", "{id}");
    }
    // March, which answered in full, is judged: an event missing from it is unlisted, while
    // April's stay put.
    espn_remove(&e.mock, EPL, "9002");
    let r = refresh_run(&e, now + time::Duration::hours(14)).await;
    assert_eq!(r["leagues"][0]["unlisted"], 1, "{r}");
    assert_eq!(state_of(&e, "9002").await, "unlisted");
    assert_eq!(state_of(&e, "9003").await, "scheduled");
    assert_eq!(state_of(&e, "9004").await, "scheduled");
}

#[tokio::test]
async fn a_record_that_cannot_be_read_is_not_an_absent_event() {
    let (e, now) = two_months().await;
    // 9001 comes back with a broken date while the others parse; 9002 is really gone.
    espn_set_field(&e.mock, EPL, "9001", "date", json!("garbage"));
    espn_remove(&e.mock, EPL, "9002");
    let r = refresh_run(&e, now + time::Duration::hours(7)).await;
    assert_eq!(r["leagues"][0]["status"], "ok", "{r}");
    // The answer for March had a record that could not be read, so it is not trusted to say
    // what is gone: nothing in it is unlisted, and the broken record's event is untouched.
    assert_eq!(r["leagues"][0]["unlisted"], 0, "{r}");
    assert_eq!(state_of(&e, "9001").await, "scheduled");
    assert_eq!(state_of(&e, "9002").await, "scheduled");
    let date = stored(
        &e,
        "SELECT starts_at_utc FROM sport_events WHERE id = 'event:premier-league:9001'",
    )
    .await;
    assert_eq!(date, ["2027-03-25T15:00:00Z"]);
    // Once the record reads again, the missing one is judged.
    espn_set_field(&e.mock, EPL, "9001", "date", json!("2027-03-25T15:00Z"));
    refresh_run(&e, now + time::Duration::hours(14)).await;
    assert_eq!(state_of(&e, "9001").await, "scheduled");
    assert_eq!(state_of(&e, "9002").await, "unlisted");
}

#[tokio::test]
async fn an_event_that_moves_out_of_the_window_is_unlisted_then_comes_back_in_place() {
    let (e, now) = two_months().await;
    // 9001 moves to July: absent from March, which answered in full, and not in April.
    espn_set_field(&e.mock, EPL, "9001", "date", json!("2027-07-10T15:00Z"));
    let r = refresh_run(&e, now + time::Duration::hours(7)).await;
    assert_eq!(r["leagues"][0]["unlisted"], 1, "{r}");
    assert_eq!(state_of(&e, "9001").await, "unlisted");
    assert_ne!(state_of(&e, "9001").await, "cancelled");
    let (_, v) = get(&e, "/api/v1/sport/events?from=2027-03-01&to=2027-04-30").await;
    let n = names(&v["events"]);
    assert!(!n.contains(&"Alpha at Beta".to_string()), "{n:?}");
    assert_eq!(n.len(), 3, "{n:?}");
    // Three months on, the window reaches July and the source lists it there: the same row
    // is scheduled again, with its new date.
    let later = time::macros::datetime!(2027-06-25 12:00 UTC);
    refresh_run(&e, later).await;
    assert_eq!(state_of(&e, "9001").await, "scheduled");
    let date = stored(
        &e,
        "SELECT starts_at_utc FROM sport_events WHERE id = 'event:premier-league:9001'",
    )
    .await;
    assert_eq!(date, ["2027-07-10T15:00:00Z"]);
    assert_eq!(count(&e, "sport_events").await, 4);
    let (_, v) = get(&e, "/api/v1/sport/events?from=2027-07-01&to=2027-07-31").await;
    assert_eq!(names(&v["events"]), ["Alpha at Beta"]);
}

#[tokio::test]
async fn a_stored_event_is_updated_after_the_clock_moves_on_but_no_backlog_is_added() {
    let e = start_exec(ExecOptions::default()).await;
    let now = time::macros::datetime!(2027-03-20 12:00 UTC);
    pin_events(&e, &[("9001", "2027-03-25T15:00Z", "Alpha at Beta")]);
    follow_quietly(&e, "premier-league", now).await;
    refresh_run(&e, now).await;
    assert_eq!(state_of(&e, "9001").await, "scheduled");
    // Arr was off for ten days. The game was played, and another one before the window shows
    // up for the first time.
    espn_set_status(&e.mock, EPL, "9001", "STATUS_FINAL", "post");
    let mut old = espn_event_like(&e.mock, EPL, "9005", "2027-03-24T15:00Z", "Old at News");
    old["status"]["type"] = json!({ "name": "STATUS_FINAL", "state": "post" });
    e.mock
        .lock()
        .unwrap()
        .espn_events
        .get_mut(EPL)
        .unwrap()
        .push(old);
    let later = time::macros::datetime!(2027-03-30 12:00 UTC);
    let r = refresh_run(&e, later).await;
    assert_eq!(r["leagues"][0]["changed"], 1, "{r}");
    assert_eq!(state_of(&e, "9001").await, "played");
    assert_eq!(
        count(&e, "sport_events").await,
        1,
        "the old game is not stored"
    );
}

#[test]
fn only_round_labels_from_the_list_are_kept() {
    use arr::sport::espn::round_label;
    for (text, want) in [
        ("Round 12", Some("Round 12")),
        ("round 3", Some("Round 3")),
        ("Matchweek 9", Some("Matchweek 9")),
        ("MATCHDAY 4", Some("Matchday 4")),
        ("Week 7", Some("Week 7")),
        ("Game 5", Some("Game 5")),
        ("Group b", Some("Group B")),
        ("Quarter-final", Some("Quarter-final")),
        ("Quarterfinals", Some("Quarterfinals")),
        ("Semi final", Some("Semi final")),
        ("Final", Some("Final")),
        ("Play-offs", Some("Play-offs")),
        ("  Round   12 ", Some("Round 12")),
        ("Manchester City win 4-3 on penalties", None),
        ("Round 12: Team A beat Team B", None),
        ("Final: Team A 2-1 Team B", None),
        ("Game 5 - Lakers win", None),
        ("NBA Cup - Group Play", None),
        ("Round", None),
        ("Round 12345", None),
        ("", None),
    ] {
        assert_eq!(round_label(text).as_deref(), want, "{text:?}");
    }
}

#[tokio::test]
async fn a_note_that_says_how_a_game_went_reaches_neither_the_store_nor_an_answer() {
    let e = start_exec(ExecOptions::default()).await;
    let spoiler = "Manchester City win 4-3 on penalties";
    espn_set_note(&e.mock, EPL, FULHAM_AT_CITY, spoiler);
    espn_set_note(&e.mock, EPL, ARSENAL_AT_NEWCASTLE, "Round 12");
    espn_set_note(&e.mock, EPL, "401879238", "Semi-final");
    espn_set_field(
        &e.mock,
        EPL,
        "401879236",
        "name",
        json!("Leeds United beat Chelsea 2-0"),
    );
    follow(&e, json!({ "league": "premier-league" })).await;
    let rounds = stored(
        &e,
        "SELECT id || '=' || COALESCE(round, '-') FROM sport_events WHERE id IN ('event:premier-league:401879233', 'event:premier-league:401879232', 'event:premier-league:401879238') ORDER BY id",
    )
    .await;
    assert_eq!(
        rounds,
        [
            "event:premier-league:401879232=Round 12",
            "event:premier-league:401879233=-",
            "event:premier-league:401879238=Semi-final"
        ]
    );
    // The name and short name of a game are built from the teams, not copied.
    let short = stored(
        &e,
        "SELECT name || '|' || COALESCE(short_name, '-') FROM sport_events WHERE id = 'event:premier-league:401879236'",
    )
    .await;
    assert_eq!(short, ["Chelsea v Leeds United|CHE v LEE"]);
    let everything = stored(
        &e,
        "SELECT group_concat(COALESCE(round, '') || COALESCE(short_name, '') || name || COALESCE(venue, ''), ' ') FROM sport_events",
    )
    .await
    .join(" ");
    assert!(!everything.contains("penalties") && !everything.contains("2-0"));
    let wide = wide_events();
    for path in [
        wide.as_str(),
        "/api/v1/sport/league/premier-league",
        "/api/v1/sport/following",
        "/api/v1/feed",
        "/api/v1/calendar",
    ] {
        let (s, _, text) = e.json(e.get(path)).await;
        assert_eq!(s, 200, "{path}");
        assert!(
            !text.contains("penalties") && !text.contains("win 4-3") && !text.contains("2-0"),
            "{path}: {text}"
        );
    }
    let (_, v) = get(&e, &wide_events()).await;
    let semi = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "event:premier-league:401879238")
        .unwrap();
    assert_eq!(semi["round"], "Semi-final");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_read_of_a_league_at_a_time_whoever_asks() {
    for cache in [0, 60] {
        let e = start_exec(ExecOptions {
            sports_cache_secs: cache,
            ..Default::default()
        })
        .await;
        e.mock.lock().unwrap().espn_delay = Some(Duration::from_millis(250));
        follow_quietly(&e, "premier-league", OffsetDateTime::now_utc()).await;
        let page = "/api/v1/sport/league/premier-league";
        let body = json!({ "league": "premier-league" });
        let post = || e.json(e.post("/api/v1/sport/follow").json(&body));
        let (a, b, c, d, p1, p2, report) = tokio::join!(
            post(),
            post(),
            post(),
            post(),
            get(&e, page),
            get(&e, page),
            e.tick_at(OffsetDateTime::now_utc())
        );
        for (s, ..) in [&a, &b, &c, &d] {
            assert!(*s == 200 || *s == 201);
        }
        assert_eq!((p1.0, p2.0), (200, 200));
        assert!(report.tasks.contains(&"sports".to_string()));
        // (The worker's pass also reads other leagues' team lists: not what is counted here.)
        let log: Vec<String> = e
            .mock
            .lock()
            .unwrap()
            .espn_log
            .iter()
            .filter(|l| l.contains("eng.1"))
            .cloned()
            .collect();
        let distinct: std::collections::HashSet<&String> = log.iter().collect();
        assert_eq!(
            distinct.len(),
            log.len(),
            "cache {cache}: repeated: {log:?}"
        );
        assert!(log.len() <= 3, "cache {cache}: {log:?}");
        assert_eq!(count(&e, "sport_events").await, 9);
    }
}

#[tokio::test]
async fn an_answer_that_is_not_a_schedule_counts_as_a_failure() {
    for body in ["this is not json", "{\"events\":5}", "{}"] {
        let e = start_exec(ExecOptions {
            sports_backoff_secs: 60,
            ..Default::default()
        })
        .await;
        {
            let mut m = e.mock.lock().unwrap();
            m.espn_garbage = vec!["scoreboard".into()];
            m.espn_garbage_body = body.into();
        }
        let (s, v) = follow(&e, json!({ "league": "nba" })).await;
        assert_eq!(
            (s, &v["schedule_read"]),
            (201, &json!(false)),
            "{body}: {v}"
        );
        assert_eq!(espn_requests(&e.mock), 1);
        // The pause that follows a failure is on, though the source answered 200.
        let (s, v) = follow(&e, json!({ "league": "premier-league" })).await;
        assert_eq!((s, &v["schedule_read"]), (201, &json!(false)), "{body}");
        assert_eq!(espn_requests(&e.mock), 1, "{body}");
        let (_, v) = get(&e, "/api/v1/sport/league/nba").await;
        assert!(v["schedule"]["problem"].is_string(), "{body}: {v}");
        assert_eq!(count(&e, "sport_events").await, 0);
    }
}

#[tokio::test]
async fn a_team_list_that_is_not_json_is_not_asked_for_on_every_page() {
    let e = start_exec(ExecOptions::default()).await;
    e.mock.lock().unwrap().espn_garbage = vec!["teams".into()];
    let teams_asked = |e: &Exec| {
        e.mock
            .lock()
            .unwrap()
            .espn_log
            .iter()
            .filter(|l| l.contains("eng.1/teams"))
            .count()
    };
    let (s, v) = follow(&e, json!({ "league": "premier-league" })).await;
    assert_eq!((s, &v["schedule_read"]), (201, &json!(true)), "{v}");
    assert_eq!(teams_asked(&e), 1);
    for _ in 0..3 {
        let (s, v) = get(&e, "/api/v1/sport/league/premier-league").await;
        assert_eq!(s, 200, "{v}");
        assert!(!v["teams"].as_array().unwrap().is_empty());
    }
    assert_eq!(teams_asked(&e), 1, "the failure is remembered");
    // After the wait it is tried once more, and then left alone for longer.
    let now = OffsetDateTime::now_utc();
    refresh_run(&e, now + time::Duration::hours(7)).await;
    assert_eq!(teams_asked(&e), 2);
    refresh_run(&e, now + time::Duration::hours(14)).await;
    assert_eq!(teams_asked(&e), 3);
    // And a team list in the wrong shape is a failure too.
    e.mock.lock().unwrap().espn_garbage_body = "{\"sports\":[]}".into();
    refresh_run(&e, now + time::Duration::hours(30)).await;
    assert_eq!(teams_asked(&e), 4);
    let failures = stored(
        &e,
        "SELECT teams_failures FROM sport_refresh WHERE league = 'premier-league'",
    )
    .await;
    assert_eq!(failures, ["4"]);
}

#[tokio::test]
async fn the_events_window_survives_the_ends_of_time() {
    let e = start_exec(ExecOptions::default()).await;
    let code = |v: &Value| v["error"]["code"].as_str().map(str::to_string);
    // No end given: the default is from + 14 days, which does not exist in the last days.
    let (s, v) = get(&e, "/api/v1/sport/events?from=9999-12-25").await;
    assert_eq!(
        (s, code(&v).as_deref()),
        (400, Some("invalid_query")),
        "{v}"
    );
    // With an end, the same start is fine.
    let (s, v) = get(&e, "/api/v1/sport/events?from=9999-12-25&to=9999-12-26").await;
    assert_eq!((s, &v["events"]), (200, &json!([])), "{v}");
    let (s, v) = get(&e, "/api/v1/sport/events?from=9999-12-31&to=9999-12-31").await;
    assert_eq!(
        (s, code(&v).as_deref()),
        (400, Some("invalid_query")),
        "{v}"
    );
    // An offset that pushes the time past the last year is a bad time, not a crash.
    let (s, v) = get(
        &e,
        "/api/v1/sport/events?from=9999-12-31T23:59:59-05:00&to=9999-12-31T23:59:59-04:00",
    )
    .await;
    assert_eq!(
        (s, code(&v).as_deref()),
        (400, Some("invalid_query")),
        "{v}"
    );
    let (s, v) = get(&e, "/api/v1/sport/events?from=0001-01-01&to=0001-01-02").await;
    assert_eq!((s, &v["events"]), (200, &json!([])), "{v}");
    let (s, v) = get(&e, "/api/v1/sport/events?from=0001-01-01").await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = get(&e, "/api/v1/sport/events?to=9999-12-31").await;
    assert_eq!(
        (s, code(&v).as_deref()),
        (400, Some("invalid_query")),
        "{v}"
    );
    let (s, v) = get(&e, "/api/v1/sport/events?from=2026-13-01").await;
    assert_eq!(
        (s, code(&v).as_deref()),
        (400, Some("invalid_query")),
        "{v}"
    );
    // The calendar takes the same extremes without trouble.
    let (s, v) = get(
        &e,
        "/api/v1/calendar?start=9999-12-01T00:00:00Z&end=9999-12-31T00:00:00Z",
    )
    .await;
    assert_eq!((s, &v["sport"]), (200, &json!([])), "{v}");
}

// ------------------------------------------------------------------ review round 2

#[tokio::test]
async fn titles_that_read_like_results_never_reach_an_answer() {
    let e = start_exec(ExecOptions::default()).await;
    // A fight card whose title says how a fight went, and a game whose name, short name and
    // venue do.
    espn_set_field(&e.mock, UFC, "600061400", "name", json!("Gane def. Hokit"));
    espn_set_field(
        &e.mock,
        UFC,
        "600061665",
        "name",
        json!("UFC Fight Night: Jones wins by KO"),
    );
    espn_set_field(
        &e.mock,
        EPL,
        FULHAM_AT_CITY,
        "name",
        json!("Fulham lost to Manchester City"),
    );
    espn_set_field(
        &e.mock,
        EPL,
        FULHAM_AT_CITY,
        "shortName",
        json!("FUL 3-1 MNC"),
    );
    {
        let mut m = e.mock.lock().unwrap();
        let ev = m
            .espn_events
            .get_mut(EPL)
            .unwrap()
            .iter_mut()
            .find(|x| x["id"] == FULHAM_AT_CITY)
            .unwrap();
        ev["competitions"][0]["venue"]["fullName"] = json!("Fulham def. Manchester City");
    }
    follow(&e, json!({ "league": "ufc" })).await;
    follow(&e, json!({ "league": "premier-league" })).await;
    let (_, v) = get(&e, &wide_events()).await;
    let events = v["events"].as_array().unwrap();
    let day = |id: &str| espn_start(&e.mock, UFC, id).date().to_string();
    let by_id = |id: &str| events.iter().find(|x| x["id"] == id).unwrap();
    // The events are there, named from their league and date.
    assert_eq!(
        by_id("event:ufc:600061400")["name"],
        format!("UFC event on {}", day("600061400"))
    );
    assert_eq!(
        by_id("event:ufc:600061665")["name"],
        format!("UFC event on {}", day("600061665"))
    );
    let game = by_id(&format!("event:premier-league:{FULHAM_AT_CITY}"));
    assert_eq!(game["name"], "Manchester City v Fulham");
    assert_eq!(game["short_name"], "MNC v FUL");
    assert_eq!(game["venue"], Value::Null);
    let (_, text) = {
        let (s, _, t) = e.json(e.get(&wide_events())).await;
        (s, t)
    };
    for bad in ["def.", "lost to", "wins by", "3-1"] {
        assert!(!text.contains(bad), "{bad} in {text}");
    }
    // A good title is untouched: follow the golf league and read the events again.
    follow(&e, json!({ "league": "dp-world-tour" })).await;
    let (_, v) = get(&e, &wide_events()).await;
    assert!(
        names(&v["events"]).contains(&"DP World Tour Championship".to_string()),
        "{v}"
    );
}

#[tokio::test]
async fn a_shared_boundary_day_is_judged_only_when_both_months_answered_in_full() {
    let e = start_exec(ExecOptions::default()).await;
    let now = time::macros::datetime!(2027-03-20 12:00 UTC);
    // 9010 is at 02:00 UTC on 1 April: the feed (American days) files it under 31 March.
    pin_events(
        &e,
        &[
            ("9001", "2027-03-25T15:00Z", "Alpha at Beta"),
            ("9003", "2027-04-03T15:00Z", "Epsilon at Zeta"),
            ("9010", "2027-04-01T02:00Z", "Night at Game"),
        ],
    );
    follow_quietly(&e, "premier-league", now).await;
    refresh_run(&e, now).await;
    assert_eq!(state_of(&e, "9010").await, "scheduled");
    // March now comes back with a record that cannot be read and has no id (it may well be
    // 9010), while April is complete and does not list it.
    espn_remove(&e.mock, EPL, "9010");
    e.mock
        .lock()
        .unwrap()
        .espn_events
        .get_mut(EPL)
        .unwrap()
        .push(json!({ "date": "2027-03-31T20:00Z", "name": "no id" }));
    let r = refresh_run(&e, now + time::Duration::hours(7)).await;
    assert_eq!(r["leagues"][0]["unlisted"], 0, "{r}");
    assert_eq!(state_of(&e, "9010").await, "scheduled");
    // Once March is complete as well, the day is judged and the event is unlisted.
    e.mock
        .lock()
        .unwrap()
        .espn_events
        .get_mut(EPL)
        .unwrap()
        .retain(|x| x.get("id").is_some());
    let r = refresh_run(&e, now + time::Duration::hours(14)).await;
    assert_eq!(r["leagues"][0]["unlisted"], 1, "{r}");
    assert_eq!(state_of(&e, "9010").await, "unlisted");
}

#[tokio::test]
async fn a_team_that_cannot_be_checked_is_unavailable_not_missing() {
    let e = start_exec(ExecOptions::default()).await;
    e.mock.lock().unwrap().espn_garbage = vec!["teams".into()];
    let teams_asked = |e: &Exec| {
        e.mock
            .lock()
            .unwrap()
            .espn_log
            .iter()
            .filter(|l| l.contains("/teams"))
            .count()
    };
    // A fresh database and a team list that cannot be read: 503 each time, never 404, even
    // while the retry pause is on.
    for _ in 0..2 {
        let (s, v) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
        assert_eq!(
            (s, v["error"]["code"].as_str()),
            (503, Some("upstream_unavailable")),
            "{v}"
        );
    }
    assert_eq!(teams_asked(&e), 1, "the second try waited out the pause");
    let (_, v) = get(&e, "/api/v1/sport/following").await;
    assert_eq!(v["following"], json!([]));
    // The source recovers and the pause has passed.
    e.mock.lock().unwrap().espn_garbage = vec![];
    e.state
        .store
        .with(|c| {
            c.execute("UPDATE sport_refresh SET teams_next_try_utc = NULL", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let (s, v) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["follow"]["team"]["name"], "Manchester City");
    // A list that was read is authoritative: a team that is not in it is simply not there.
    let (s, v) = follow(
        &e,
        json!({ "league": "premier-league", "team_id": "999999" }),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (404, Some("not_found")),
        "{v}"
    );
}

#[tokio::test]
async fn a_well_formed_empty_team_list_does_not_pause_other_leagues() {
    let e = start_exec(ExecOptions {
        sports_backoff_secs: 60,
        ..Default::default()
    })
    .await;
    e.mock.lock().unwrap().espn_garbage = vec!["teams".into()];
    e.mock.lock().unwrap().espn_garbage_body = "{\"sports\":[]}".into();
    let (s, v) = follow(&e, json!({ "league": "premier-league" })).await;
    assert_eq!((s, &v["schedule_read"]), (201, &json!(true)), "{v}");
    // The next league is read at once: nothing paused the client.
    let before = espn_requests(&e.mock);
    let (s, v) = follow(&e, json!({ "league": "nba" })).await;
    assert_eq!((s, &v["schedule_read"]), (201, &json!(true)), "{v}");
    assert!(espn_requests(&e.mock) > before);
    // The league has no team list yet: its own retry timing, not authoritative.
    let rows = stored(
        &e,
        "SELECT teams_failures || '|' || COALESCE(teams_utc, '-') FROM sport_refresh WHERE league = 'premier-league'",
    )
    .await;
    assert_eq!(rows, ["1|-"]);
    // A malformed one still pauses everything.
    e.mock.lock().unwrap().espn_garbage_body = "{\"sports\":5}".into();
    follow(&e, json!({ "league": "ufc" })).await;
    follow(&e, json!({ "league": "pfl" })).await;
    e.state
        .store
        .with(|c| {
            c.execute("UPDATE sport_refresh SET teams_next_try_utc = NULL", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let (s, _) = get(&e, "/api/v1/sport/league/premier-league").await;
    assert_eq!(s, 200);
    let after_bad = espn_requests(&e.mock);
    let (s, v) = follow(&e, json!({ "league": "mls" })).await;
    assert_eq!((s, &v["schedule_read"]), (201, &json!(false)), "{v}");
    assert_eq!(
        espn_requests(&e.mock),
        after_bad,
        "paused after the malformed list"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_pass_does_not_wait_on_reads_that_are_in_flight() {
    let e = start_exec(ExecOptions::default()).await;
    e.mock.lock().unwrap().espn_delay = Some(Duration::from_millis(800));
    let now = OffsetDateTime::now_utc();
    for league in ["ufc", "nba", "premier-league"] {
        follow_quietly(&e, league, now).await;
    }
    let reads = async {
        // Two cold league pages read a slow source one request at a time.
        tokio::join!(
            get(&e, "/api/v1/sport/league/premier-league"),
            get(&e, "/api/v1/sport/league/nba")
        )
    };
    let pass = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = Instant::now();
        let r = arr::sport::refresh::run_with_budget(
            e.state.store.clone(),
            e.state.sports.clone().unwrap(),
            now,
            Duration::from_secs(1),
        )
        .await;
        (started.elapsed(), r)
    };
    let (_, (took, r)) = tokio::join!(reads, pass);
    // The pass came back within its budget (the reads take several seconds in all) ...
    assert!(took < Duration::from_millis(2500), "{took:?} {r}");
    let by_league = |slug: &str| {
        r["leagues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["league"] == slug)
            .cloned()
    };
    // ... skipping the leagues a page was already reading, and not waiting out the one it started.
    assert_eq!(by_league("nba").unwrap()["status"], "skipped", "{r}");
    assert_eq!(
        by_league("premier-league").unwrap()["status"],
        "skipped",
        "{r}"
    );
    assert_eq!(by_league("ufc").unwrap()["status"], "running", "{r}");
    assert_eq!(r["stopped"], "out of time for this run");
    // The read the worker started carries on and completes.
    let mut done = false;
    for _ in 0..100 {
        let rows = stored(
            &e,
            "SELECT COUNT(*) FROM sport_refresh WHERE league = 'ufc' AND refreshed_utc IS NOT NULL",
        )
        .await;
        if rows == ["1"] {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(done, "the background read of ufc never finished");
}

// ------------------------------------------------------------------ review round 3

#[tokio::test]
async fn every_text_field_that_carries_a_result_is_neutral_in_the_store_and_every_answer() {
    let e = start_exec(ExecOptions::default()).await;
    {
        let mut m = e.mock.lock().unwrap();
        for ev in m.espn_events.get_mut(EPL).unwrap().iter_mut() {
            ev["season"]["year"] = json!("Smith def. Jones");
            for comp in ev["competitions"].as_array_mut().unwrap() {
                comp["venue"]["fullName"] = json!("Jones wins by KO");
                for c in comp["competitors"].as_array_mut().unwrap() {
                    if c["team"]["id"] == "382" {
                        c["team"]["displayName"] = json!("Manchester City def. Fulham");
                        c["team"]["shortDisplayName"] = json!("Man City 3-1");
                        c["team"]["abbreviation"] = json!("MNC 3-1 FUL");
                    }
                }
            }
        }
        let list = m.espn_teams.get_mut(EPL).unwrap();
        for t in list["sports"][0]["leagues"][0]["teams"]
            .as_array_mut()
            .unwrap()
        {
            if t["team"]["id"] == "382" {
                t["team"]["displayName"] = json!("Jones wins by KO");
                t["team"]["shortDisplayName"] = json!("3-1");
                t["team"]["abbreviation"] = json!("3-1");
            }
        }
    }
    follow(&e, json!({ "league": "premier-league" })).await;
    let (s, v) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    assert_eq!(s, 201, "{v}");
    // What a follow stores for a team whose name did not fit.
    assert_eq!(
        v["follow"]["team"],
        json!({ "id": "382", "name": "Team 382" })
    );
    let rows = stored(
        &e,
        "SELECT group_concat(COALESCE(name, '') || '|' || COALESCE(short_name, '') || '|' || COALESCE(home_name, '') || '|' || COALESCE(away_name, '') || '|' || COALESCE(venue, '') || '|' || COALESCE(season, ''), ' ; ') FROM sport_events",
    )
    .await
    .join(" ");
    let teams = stored(
        &e,
        "SELECT group_concat(name || '|' || COALESCE(short_name, '') || '|' || COALESCE(abbreviation, ''), ' ; ') FROM sport_teams",
    )
    .await
    .join(" ");
    let follows = stored(
        &e,
        "SELECT group_concat(COALESCE(team_name, ''), ' ; ') FROM sport_follows",
    )
    .await
    .join(" ");
    let wide = wide_events();
    let mut seen = vec![rows.clone(), teams.clone(), follows.clone()];
    for path in [
        wide.as_str(),
        "/api/v1/sport/league/premier-league",
        "/api/v1/sport/following",
        "/api/v1/sport/search?q=Team",
        "/api/v1/feed",
        "/api/v1/calendar",
    ] {
        let (s, _, text) = e.json(e.get(path)).await;
        assert_eq!(s, 200, "{path}");
        seen.push(text);
    }
    // Every answer really carried the neutral label, so the checks below are not about nothing.
    assert!(
        seen.iter().filter(|t| t.contains("Team 382")).count() >= 4,
        "{seen:?}"
    );
    for text in &seen {
        for bad in ["def.", "3-1", "wins by", "Smith", "Jones", "KO"] {
            assert!(!text.contains(bad), "{bad} in {text}");
        }
    }
    assert!(rows.contains("Team 382 v Fulham"), "{rows}");
    assert!(teams.contains("Team 382|"), "{teams}");
    // The season is dropped, the venue too; the event and the team are still there.
    assert!(!rows.contains("Smith") && !rows.contains("Etihad"));
    let (_, v) = get(&e, &wide_events()).await;
    let game = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == format!("event:premier-league:{FULHAM_AT_CITY}"))
        .unwrap();
    assert_eq!(game["name"], "Team 382 v Fulham");
    assert_eq!(game["home"], json!({ "id": "382", "name": "Team 382" }));
    assert_eq!(game["short_name"], Value::Null);
    assert_eq!(
        (&game["venue"], &game["season"]),
        (&Value::Null, &Value::Null)
    );
}

#[tokio::test]
async fn a_team_is_missing_only_by_a_current_list_and_unavailable_by_a_stale_one() {
    let e = start_exec(ExecOptions::default()).await;
    // A list read last season: it has no team 382 (newly promoted), and it is old.
    follow(&e, json!({ "league": "premier-league" })).await;
    e.state
        .store
        .with(|c| {
            let old = "2026-01-01T00:00:00Z";
            c.execute(
                "UPDATE sport_refresh SET teams_utc = ?1, teams_next_try_utc = NULL",
                [old],
            )?;
            c.execute("DELETE FROM sport_teams WHERE team_id = '382'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    // The refresh that is due fails: 503, not 404.
    e.mock.lock().unwrap().espn_garbage = vec!["teams".into()];
    let (s, v) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (503, Some("upstream_unavailable")),
        "{v}"
    );
    // While it is waited out, still 503. A team that the stored list has can be followed.
    let (s, v) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    assert_eq!(s, 503, "{v}");
    let (s, v) = follow(&e, json!({ "league": "premier-league", "team_id": "359" })).await;
    assert_eq!(
        s, 201,
        "the source is down but Arsenal is in the stored list: {v}"
    );
    // The source recovers with the team in it.
    e.mock.lock().unwrap().espn_garbage = vec![];
    e.state
        .store
        .with(|c| {
            c.execute("UPDATE sport_refresh SET teams_next_try_utc = NULL", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let (s, v) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["follow"]["team"]["name"], "Manchester City");
    // A current list that does not have the team says so.
    let (s, v) = follow(
        &e,
        json!({ "league": "premier-league", "team_id": "999999" }),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (404, Some("not_found")),
        "{v}"
    );
}

// ------------------------------------------------------------------ review round 4

#[tokio::test]
async fn a_cold_team_follow_shares_one_budget_between_the_team_list_and_the_schedule() {
    let e = start_exec(ExecOptions::default()).await;
    // Each request takes 1.2 s; the request timeout is 2 s, so the budget is 1.6 s.
    e.mock.lock().unwrap().espn_delay = Some(Duration::from_millis(1200));
    let started = Instant::now();
    let (s, v) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    let took = started.elapsed();
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["follow"]["team"]["name"], "Manchester City");
    assert_eq!(
        v["schedule_read"], false,
        "the schedule is still being read"
    );
    // One deadline: the team list took most of it and the schedule got what was left, not a
    // second budget of its own (that would be about 2.8 s).
    assert!(took < Duration::from_millis(2300), "{took:?}");
    let (_, f) = get(&e, "/api/v1/sport/following").await;
    assert_eq!(f["following"].as_array().unwrap().len(), 1);
    // The read the follow started carries on and completes.
    let mut done = false;
    for _ in 0..60 {
        if count(&e, "sport_events").await > 0 {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(done, "the schedule never arrived");
}

#[tokio::test]
async fn a_followed_teams_label_comes_from_the_team_list_and_since_never_moves() {
    let e = start_exec(ExecOptions::default()).await;
    let poison = |on: bool| {
        let mut m = e.mock.lock().unwrap();
        let name = if on {
            "Jones wins by KO"
        } else {
            "Manchester City"
        };
        for ev in m.espn_events.get_mut(EPL).unwrap().iter_mut() {
            for comp in ev["competitions"].as_array_mut().unwrap() {
                for c in comp["competitors"].as_array_mut().unwrap() {
                    if c["team"]["id"] == "382" {
                        c["team"]["displayName"] = json!(name);
                    }
                }
            }
        }
        let list = m.espn_teams.get_mut(EPL).unwrap();
        for t in list["sports"][0]["leagues"][0]["teams"]
            .as_array_mut()
            .unwrap()
        {
            if t["team"]["id"] == "382" {
                t["team"]["displayName"] = json!(name);
            }
        }
    };
    poison(true);
    let (s, v) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["follow"]["team"]["name"], "Team 382");
    let since = v["follow"]["since"].clone();

    // The feed's name is good now, and the team list is read again.
    poison(false);
    e.state
        .store
        .with(|c| {
            c.execute(
                "UPDATE sport_refresh SET teams_utc = '2026-01-01T00:00:00Z', teams_next_try_utc = NULL",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let r = refresh_run(&e, OffsetDateTime::now_utc() + time::Duration::hours(7)).await;
    assert_eq!(r["leagues"][0]["status"], "ok", "{r}");

    let (_, f) = get(&e, "/api/v1/sport/following").await;
    assert_eq!(f["following"][0]["team"]["name"], "Manchester City", "{f}");
    assert_eq!(f["following"][0]["since"], since);
    let (_, page) = get(&e, "/api/v1/sport/league/premier-league").await;
    assert_eq!(
        page["following_teams"][0]["name"], "Manchester City",
        "{page}"
    );
    let listed = page["teams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "382")
        .unwrap();
    assert_eq!(
        (listed["name"].as_str(), &listed["followed"]),
        (Some("Manchester City"), &json!(true))
    );
    let (_, found) = get(&e, "/api/v1/sport/search?q=man%20city").await;
    assert_eq!(
        found["teams"][0]["team"]["name"], "Manchester City",
        "{found}"
    );
    // Following again changes nothing but shows the current label.
    let (s, again) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    assert_eq!((s, &again["created"]), (200, &json!(false)), "{again}");
    assert_eq!(again["follow"]["team"]["name"], "Manchester City");
    assert_eq!(again["follow"]["since"], since);
    // A follow whose team is gone from the table keeps the label it was made with.
    e.state
        .store
        .with(|c| {
            c.execute("DELETE FROM sport_teams WHERE team_id = '382'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, f) = get(&e, "/api/v1/sport/following").await;
    assert_eq!(f["following"][0]["team"]["name"], "Team 382", "{f}");
}

// ------------------------------------------------------------------ review round 5

/// Every team name, short name and venue in the trimmed recordings, and every fight card and
/// tournament title, through the real rules (not a copy of them).
#[test]
fn every_name_and_title_in_the_recordings_passes_the_rules() {
    use arr::sport::{catalog, espn};
    let boards = [
        (
            include_str!("fixtures/espn/soccer_eng1.json"),
            "premier-league",
        ),
        (include_str!("fixtures/espn/basketball_nba.json"), "nba"),
        (include_str!("fixtures/espn/rugby_top14.json"), "top-14"),
        (include_str!("fixtures/espn/mma_ufc.json"), "ufc"),
        (include_str!("fixtures/espn/golf_eur.json"), "dp-world-tour"),
    ];
    let lists = [
        include_str!("fixtures/espn/soccer_eng1_teams.json"),
        include_str!("fixtures/espn/basketball_nba_teams.json"),
        include_str!("fixtures/espn/rugby_top14_teams.json"),
    ];
    let (mut passed, mut failed) = (0, vec![]);
    let mut check = |what: &str, text: Option<&str>, ok: Option<String>| {
        if text.is_some() {
            if ok.is_some() {
                passed += 1;
            } else {
                failed.push(format!("{what}: {text:?}"));
            }
        }
    };
    for (board, slug) in boards {
        let v: Value = serde_json::from_str(board).unwrap();
        let league = catalog::find(slug).unwrap();
        for ev in v["events"].as_array().unwrap() {
            check(
                "title",
                ev["name"].as_str().filter(|_| !league.teams),
                espn::event_title(ev["name"].as_str(), league),
            );
            for comp in ev["competitions"].as_array().unwrap() {
                let venue = comp["venue"]["fullName"].as_str();
                check("venue", venue, espn::plain_name(venue, 120));
                for c in comp["competitors"].as_array().into_iter().flatten() {
                    let name = c["team"]["displayName"].as_str();
                    check("team", name, espn::plain_name(name, 120));
                }
            }
        }
    }
    for list in lists {
        let v: Value = serde_json::from_str(list).unwrap();
        for t in v["sports"][0]["leagues"][0]["teams"].as_array().unwrap() {
            let name = t["team"]["displayName"].as_str();
            check("team", name, espn::plain_name(name, 120));
            let short = t["team"]["shortDisplayName"].as_str();
            check("short", short, espn::plain_name(short, 60));
        }
    }
    println!("recordings: {passed} passed, {} failed", failed.len());
    assert!(failed.is_empty(), "{failed:?}");
    assert!(passed > 200, "{passed}");
}

#[test]
fn prose_cannot_get_into_a_name_or_title_by_a_separator_or_a_prefix() {
    use arr::sport::{
        catalog,
        espn::{event_title, plain_name},
    };
    let tlds = ["com", "net", "org", "io", "tv", "co"];
    let particles = [
        "de", "da", "do", "dos", "das", "del", "della", "di", "du", "van", "von", "der", "den",
        "la", "le", "el", "al", "of", "the", "and", "y", "e", "des", "dei", "delle", "degli", "zu",
        "am", "im", "on", "upon", "sur", "en", "les", "sous", "aux",
    ];
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut word = || loop {
        let mut w = String::new();
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = 3 + (seed >> 60) as usize % 7;
        for _ in 0..len {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            w.push((b'a' + ((seed >> 33) % 26) as u8) as char);
        }
        if !particles.contains(&w.as_str()) && !tlds.contains(&w.as_str()) {
            return w;
        }
    };
    let seps = [
        " ", ".", "-", "/", "(", ")", "'", "\u{2019}", "&", ". ", " - ", " (", ") ",
    ];
    let prefixes = ["", ".", "-", "/", "(", ")", "'", "\u{2019}", "&"];
    let (ufc, pga) = (
        catalog::find("ufc").unwrap(),
        catalog::find("pga-tour").unwrap(),
    );
    let mut tried = 0;
    for _ in 0..60 {
        let (a, b, c) = (word(), word(), word());
        let cap = |w: &str| format!("{}{}", w[..1].to_uppercase(), &w[1..]);
        for sep in seps {
            for prefix in prefixes {
                let lower = format!("{prefix}{a}{sep}{b}{sep}{c}");
                let capped = format!("{prefix}{}{sep}{b}", cap(&a));
                for name in [&lower, &capped] {
                    tried += 1;
                    assert_eq!(plain_name(Some(name), 120), None, "name {name:?}");
                    assert_eq!(event_title(Some(name), pga), None, "tournament {name:?}");
                    let card = format!("UFC 334: {name} vs. Jones");
                    assert_eq!(event_title(Some(&card), ufc), None, "card {card:?}");
                }
            }
        }
    }
    assert!(tried > 10_000);
    // The cases from the review, by hand.
    for bad in [
        ".smith .outclasses .jones",
        "Smith.outclasses.jones",
        "Smith-outclasses-jones",
        "Smith/outclasses/jones",
        "Smith&outclasses&jones",
        "(smith outclasses jones)",
        "'smith 'outclasses",
        "smith.com wins",
        "smith.com outclasses jones",
        "smith.com.au Arena",
        "smith..com Arena",
        "Smith's outclasses",
    ] {
        assert_eq!(plain_name(Some(bad), 120), None, "{bad}");
    }
    for good in [
        "crypto.com Arena",
        "Newcastle upon Tyne FC",
        "Stoke-on-Trent",
        "Boulogne-sur-Mer",
        "Villeneuve-d'Ascq",
        "Texas Children's Hospital",
        "Smith.Jones",
    ] {
        assert_eq!(plain_name(Some(good), 120).as_deref(), Some(good), "{good}");
    }
    // Tournament and card titles judge segments too.
    assert_eq!(event_title(Some("Smith.outclasses Open"), pga), None);
    assert_eq!(event_title(Some("Open-of-smith Classic"), pga), None);
    assert_eq!(
        event_title(Some("UFC 334: Gane.hokit vs. Jones"), ufc),
        None
    );
    assert_eq!(
        event_title(Some("U.S. Open"), pga).as_deref(),
        Some("U.S. Open")
    );
    assert_eq!(
        event_title(Some("Texas Children's Houston Open"), pga).as_deref(),
        Some("Texas Children's Houston Open")
    );
}

#[tokio::test]
async fn a_follow_that_corrects_the_label_answers_with_the_corrected_label() {
    let e = start_exec(ExecOptions::default()).await;
    follow(&e, json!({ "league": "premier-league" })).await;
    // The stored label is stale, the team list is old and the league is due: the follow
    // request itself does the refresh that corrects the name.
    let stale = || async {
        e.state
            .store
            .with(|c| {
                c.execute("UPDATE sport_teams SET name = 'Team 382' WHERE team_id = '382'", [])?;
                c.execute("UPDATE sport_refresh SET teams_utc = '2026-01-01T00:00:00Z', teams_next_try_utc = NULL, refreshed_utc = '2026-01-01T00:00:00Z'", [])?;
                Ok(())
            })
            .await
            .unwrap();
    };
    stale().await;
    let (s, first) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    assert_eq!(s, 201, "{first}");
    assert_eq!(
        first["follow"]["team"]["name"], "Manchester City",
        "{first}"
    );
    let since = first["follow"]["since"].clone();
    let (_, f) = get(&e, "/api/v1/sport/following").await;
    let mine = f["following"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["team"]["id"] == "382")
        .unwrap();
    assert_eq!(mine["team"]["name"], "Manchester City");
    // A repeat follow does the same.
    stale().await;
    let (s, again) = follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    assert_eq!((s, &again["created"]), (200, &json!(false)), "{again}");
    assert_eq!(
        again["follow"]["team"]["name"], "Manchester City",
        "{again}"
    );
    assert_eq!(again["follow"]["since"], since);
}

#[tokio::test]
async fn a_bad_name_in_a_later_list_does_not_replace_a_good_label() {
    let e = start_exec(ExecOptions::default()).await;
    follow(&e, json!({ "league": "premier-league", "team_id": "382" })).await;
    {
        let mut m = e.mock.lock().unwrap();
        let list = m.espn_teams.get_mut(EPL).unwrap();
        for t in list["sports"][0]["leagues"][0]["teams"]
            .as_array_mut()
            .unwrap()
        {
            if t["team"]["id"] == "382" {
                t["team"]["displayName"] = json!("smith outclasses jones");
            }
        }
    }
    e.state
        .store
        .with(|c| {
            c.execute("UPDATE sport_refresh SET teams_utc = '2026-01-01T00:00:00Z', teams_next_try_utc = NULL", [])?;
            Ok(())
        })
        .await
        .unwrap();
    refresh_run(&e, OffsetDateTime::now_utc() + time::Duration::hours(7)).await;
    let (_, f) = get(&e, "/api/v1/sport/following").await;
    assert_eq!(f["following"][0]["team"]["name"], "Manchester City", "{f}");
}

// ------------------------------------------------------------------ team lists in the background

fn team_requests(e: &Exec) -> Vec<String> {
    e.mock
        .lock()
        .unwrap()
        .espn_log
        .iter()
        .filter(|l| l.contains("/teams"))
        .cloned()
        .collect()
}

async fn sports_task(e: &Exec) -> Value {
    arr::worker::run_task(&e.state, "sports", OffsetDateTime::now_utc())
        .await
        .unwrap()
}

#[tokio::test]
async fn the_worker_reads_team_lists_so_search_finds_teams_nobody_opened() {
    let e = start_exec(ExecOptions::default()).await;
    let (_, v) = get(&e, "/api/v1/sport/search?q=lakers").await;
    assert_eq!(v["teams"], json!([]), "nothing read yet");
    // A couple of leagues a tick, so it takes a few ticks to reach the NBA's list.
    let mut found = false;
    for tick in 0..24 {
        let r = sports_task(&e).await;
        assert!(
            r["catalogue_teams"].as_array().unwrap().len() <= 2,
            "tick {tick}: {r}"
        );
        let (_, v) = get(&e, "/api/v1/sport/search?q=lakers").await;
        if v["teams"][0]["team"]["name"] == "Los Angeles Lakers" {
            assert_eq!(v["teams"][0]["league"]["slug"], "nba");
            found = true;
            break;
        }
    }
    assert!(found, "{:?}", team_requests(&e));
    // Only team lists were read: no schedule, since nobody follows anything.
    assert!(scoreboard_requests(&e).is_empty());
}

#[tokio::test]
async fn a_failing_team_list_backs_off_and_blocks_neither_other_leagues_nor_followed_ones() {
    let e = start_exec(ExecOptions::default()).await;
    let now = OffsetDateTime::now_utc();
    follow_quietly(&e, "nba", now).await;
    e.mock.lock().unwrap().espn_fail = Some(500);
    let r = full_run(&e, now).await;
    // The followed league failed, so the team lists were not tried this run.
    assert_eq!(r["leagues"][0]["status"], "failed", "{r}");
    assert_eq!(r["catalogue_teams"], json!([]), "{r}");
    e.mock.lock().unwrap().espn_fail = None;
    // Once the wait is over the followed league is read, and so are team lists after it.
    e.state
        .store
        .with(|c| {
            c.execute("UPDATE sport_refresh SET next_try_utc = NULL", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let before = team_requests(&e).len();
    let r = full_run(&e, now + time::Duration::hours(1)).await;
    assert_eq!(r["leagues"][0]["status"], "ok", "{r}");
    assert!(!r["catalogue_teams"].as_array().unwrap().is_empty(), "{r}");
    assert!(team_requests(&e).len() > before);
    // A failing team list stops the catalogue reads for this run and is waited out on its own
    // timing, not asked for again at the next run.
    e.mock.lock().unwrap().espn_fail = Some(500);
    let later = now + time::Duration::hours(2);
    let r = full_run(&e, later).await;
    let tried = r["catalogue_teams"].as_array().unwrap();
    assert_eq!(tried.len(), 1, "stops at the first failure: {r}");
    assert_eq!(tried[0]["status"], "failed");
    let failed = tried[0]["league"].as_str().unwrap().to_string();
    let n = team_requests(&e).len();
    let r = full_run(&e, later + time::Duration::minutes(1)).await;
    let again = r["catalogue_teams"].as_array().unwrap();
    assert!(again.iter().all(|x| x["league"] != failed.as_str()), "{r}");
    assert!(team_requests(&e).len() > n, "another league was tried");
}

#[tokio::test]
async fn a_fresh_team_list_is_not_read_again_by_the_worker() {
    let e = start_exec(ExecOptions::default()).await;
    for _ in 0..30 {
        sports_task(&e).await;
    }
    let first = team_requests(&e);
    assert!(
        first.iter().any(|l| l.contains("basketball/nba/teams")),
        "{first:?}"
    );
    for _ in 0..5 {
        sports_task(&e).await;
    }
    assert_eq!(
        team_requests(&e),
        first,
        "nothing is read again within a week"
    );
}

#[tokio::test]
async fn with_sport_off_the_worker_reads_nothing() {
    let e = start_exec(ExecOptions {
        sports: false,
        ..Default::default()
    })
    .await;
    let r = sports_task(&e).await;
    assert_eq!(r["note"], "sport is turned off");
    assert_eq!(espn_requests(&e.mock), 0);
}

#[tokio::test]
async fn team_lists_wait_until_every_due_followed_league_has_been_read() {
    let e = start_exec(ExecOptions::default()).await;
    let now = OffsetDateTime::now_utc();
    // Seven due leagues, one more than a run takes: six are read, the seventh waits.
    for league in [
        "dp-world-tour",
        "liv-golf",
        "lpga-tour",
        "pfl",
        "pga-tour",
        "premier-league",
        "ufc",
    ] {
        follow_quietly(&e, league, now).await;
    }
    let r = full_run(&e, now).await;
    assert_eq!(r["due"], 7, "{r}");
    assert_eq!(r["leagues"].as_array().unwrap().len(), 6, "{r}");
    assert_eq!(r["catalogue_teams"], json!([]), "{r}");
    // The only team list read is the followed league's own, as part of reading it.
    assert!(
        team_requests(&e).iter().all(|l| l.contains("eng.1")),
        "{:?}",
        team_requests(&e)
    );
    // The seventh is read next; once nothing is due the catalogue's lists are read.
    let r = full_run(&e, now + time::Duration::minutes(1)).await;
    assert_eq!(r["due"], 1, "{r}");
    assert_eq!(r["leagues"][0]["status"], "ok", "{r}");
    let before = team_requests(&e).len();
    let r = full_run(&e, now + time::Duration::minutes(2)).await;
    assert_eq!(r["due"], 0, "{r}");
    assert!(!r["catalogue_teams"].as_array().unwrap().is_empty(), "{r}");
    assert!(team_requests(&e).len() > before);
}

#[tokio::test]
async fn a_team_stored_under_its_fallback_name_gets_its_real_name_from_the_next_list() {
    let e = start_exec(ExecOptions::default()).await;
    // The feed's list names a team "Melbourne Victory" (a name the first version refused).
    {
        let mut m = e.mock.lock().unwrap();
        let doc = m.espn_teams.get_mut(EPL).unwrap();
        doc["sports"][0]["leagues"][0]["teams"][0]["team"]["displayName"] =
            json!("Melbourne Victory");
        doc["sports"][0]["leagues"][0]["teams"][0]["team"]["id"] = json!("5328");
    }
    // An install that stored it as `Team 5328` two days ago.
    e.state
        .store
        .with(|c| {
            c.execute(
                "INSERT INTO sport_teams (league, team_id, name) VALUES ('premier-league', '5328', 'Team 5328')",
                [],
            )?;
            c.execute(
                "INSERT INTO sport_refresh (league, teams_utc) VALUES ('premier-league', ?1)
                 ON CONFLICT (league) DO UPDATE SET teams_utc = ?1",
                [(OffsetDateTime::now_utc() - time::Duration::days(2))
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (s, v) = get(&e, "/api/v1/sport/league/premier-league").await;
    assert_eq!(s, 200, "{v}");
    let names: Vec<&str> = v["teams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Melbourne Victory"), "{names:?}");
    assert!(!names.contains(&"Team 5328"), "{names:?}");
    // With no fallback name left, the list is left alone for a week again.
    sports_task(&e).await;
    let reads = team_requests(&e)
        .iter()
        .filter(|l| l.contains("eng.1"))
        .count();
    assert_eq!(reads, 1);
}
