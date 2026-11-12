//! Searching for a person (cast, directors, writers) and what they made. Plex's metadata
//! service and TVmaze are loopback mocks; nothing here reaches the internet.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};

fn ids(group: &Value) -> Vec<&str> {
    group["credits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["media_id"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn people_search_maps_ids_and_scores() {
    let e = start_exec(ExecOptions::default()).await;
    let (s, v, text) = e.json(e.get("/api/v1/people?q=Dana")).await;
    assert_eq!(s, 200, "{text}");
    assert!(!text.contains(PLEX_TOKEN), "the token leaked: {text}");
    let people = v["people"].as_array().unwrap();
    assert_eq!(people.len(), 2, "{v}");
    assert_eq!(people[0]["person_id"], "plex:dir0001");
    assert_eq!(people[0]["name"], "Dana Director");
    assert_eq!(people[0]["known_for"], "Directing");
    assert_eq!(people[0]["score"], 0.92);
    assert_eq!(people[1]["person_id"], "plex:act0001");
    assert_eq!(people[1]["score"], 0.2);
    assert_eq!(
        v["providers"][0],
        json!({ "name": "plex", "status": "ok", "results": 2 })
    );
    let log = e.mock.lock().unwrap().plex_meta_log.clone();
    assert!(
        log.iter().all(|(_, good)| *good) && log[0].0.contains("searchTypes=people"),
        "{log:?}"
    );
    // No query is a bad request, like discover.
    let (s, _, _) = e.json(e.get("/api/v1/people")).await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn a_persons_credits_resolve_to_media_ids_in_the_right_order() {
    let e = start_exec(ExecOptions::default()).await;
    let (s, v, text) = e.json(e.get("/api/v1/people/plex:dir0001")).await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(v["name"], "Dana Director");
    let groups = v["groups"].as_array().unwrap();
    // A director's directing comes first; the writer group (its title cannot be found) and the
    // appearances are gone; the movie they also acted in is listed once.
    let jobs: Vec<&str> = groups.iter().map(|g| g["job"].as_str().unwrap()).collect();
    assert_eq!(jobs, ["Director", "Actor"], "{v}");
    assert_eq!(
        ids(&groups[0]),
        [
            format!("movie:tmdb:{PLEX_RELEASED}"),
            "series:tvdb:372264".to_string()
        ],
        "{v}"
    );
    assert_eq!(
        ids(&groups[1]),
        [format!("movie:tmdb:{PLEX_UNRELEASED}")],
        "{v}"
    );
    let show = &groups[0]["credits"][1];
    assert_eq!(
        (show["kind"].as_str(), show["library_state"].as_str()),
        (Some("series"), Some("in_plex")),
        "{show}"
    );
    assert_eq!(show["in_catalog"], true);
    let movie = &groups[0]["credits"][0];
    assert_eq!(
        (
            movie["kind"].as_str(),
            movie["year"].as_u64(),
            movie["upcoming"].as_bool()
        ),
        (Some("movie"), Some(2024), Some(false)),
        "{movie}"
    );
    assert!(movie["library_state"].is_null());
    let future = &groups[1]["credits"][0];
    assert_eq!(
        (future["role"].as_str(), future["upcoming"].as_bool()),
        (Some("Mayor"), Some(true))
    );

    // The same person again is served from the cache.
    let before = e.mock.lock().unwrap().plex_meta_log.len();
    e.json(e.get("/api/v1/people/plex:dir0001")).await;
    assert_eq!(e.mock.lock().unwrap().plex_meta_log.len(), before);

    let (s, _, _) = e.json(e.get("/api/v1/people/plex:nobody")).await;
    assert_eq!(s, 404);
    let (s, _, _) = e.json(e.get("/api/v1/people/nonsense")).await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn tvmaze_answers_when_plex_is_not_set_up() {
    let e = start_exec(ExecOptions {
        plex: false,
        ..Default::default()
    })
    .await;
    let (s, v, text) = e.json(e.get("/api/v1/people?q=Fargo")).await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(v["people"][0]["person_id"], "tvmaze:7001");
    assert_eq!(v["people"][0]["score"], 0.9);
    let names: Vec<&str> = v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["plex", "tvmaze"], "{v}");
    assert_eq!(v["providers"][0]["status"], "not_configured");

    let (s, v, text) = e.json(e.get("/api/v1/people/tvmaze:7001")).await;
    assert_eq!(s, 200, "{text}");
    let groups = v["groups"].as_array().unwrap();
    assert_eq!(groups[0]["job"], "Actor");
    assert_eq!(
        ids(&groups[0]),
        ["series:tvdb:269613", "series:tvdb:372264"],
        "{v}"
    );
    assert_eq!(groups[0]["credits"][1]["library_state"], "in_plex");
    // The show they also created is listed once, under the first job.
    assert_eq!(groups[1]["job"], "Creator");
    assert_eq!(ids(&groups[1]), ["series:tvdb:999006"], "{v}");
    assert_eq!(groups[1]["credits"][0]["upcoming"], true);
}

#[tokio::test]
async fn a_failing_provider_is_a_status_not_an_error() {
    let e = start_exec(ExecOptions {
        tvmaze: false,
        ..Default::default()
    })
    .await;
    e.mock.lock().unwrap().plex_meta_fail = true;
    let (s, v, text) = e.json(e.get("/api/v1/people?q=Dana")).await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(v["people"], json!([]));
    assert_eq!(v["providers"][0]["name"], "plex");
    assert_eq!(v["providers"][0]["status"], "failed", "{v}");
    assert_eq!(
        v["providers"][0]["error"]["code"], "upstream_unavailable",
        "{v}"
    );
}
