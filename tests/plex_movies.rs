//! Movies come from Plex's own metadata service (the Plex sign-in), with TMDB as an optional
//! backup. The Plex metadata service is a loopback mock; nothing here reaches the internet.
#![recursion_limit = "256"]

mod common;

use arr::catalog::Availability;
use arr::ids::MediaId;
use common::*;
use serde_json::{Value, json};

fn movie_id(tmdb: u32) -> MediaId {
    MediaId::Movie { tmdb }
}

fn today() -> String {
    time::OffsetDateTime::now_utc().date().to_string()
}

async fn add(e: &Exec, tmdb: u32) -> Value {
    let (s, v, text) = e
        .json(
            e.post("/api/v1/quick-add")
                .json(&json!({ "media_id": format!("movie:tmdb:{tmdb}") })),
        )
        .await;
    assert_eq!(s, 200, "{text}");
    v
}

#[tokio::test]
async fn plex_alone_finds_movies_without_a_tmdb_key() {
    let e = start_exec(ExecOptions::default()).await;
    assert!(e.state.cfg.tmdb.is_none());

    let (s, v, text) = e.json(e.get("/api/v1/discover?kind=movie&q=Plex")).await;
    assert_eq!(s, 200, "{text}");
    assert!(!text.contains(PLEX_TOKEN), "the token leaked: {text}");
    // The movie Plex has no TMDB id for is skipped; the others keep their TMDB ids.
    let ids: Vec<&str> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["media_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "movie:tmdb:910001",
            "movie:tmdb:910002",
            "movie:tmdb:910003"
        ],
        "{v}"
    );
    assert_eq!(v["results"][2]["original_title"], "Le Film Etranger");
    assert_eq!(v["results"][0]["year"], 2024);
    let plex = v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "plex")
        .unwrap();
    assert_eq!(
        (&plex["status"], &plex["results"]),
        (&json!("ok"), &json!(3))
    );
    assert!(
        v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["name"] != "tmdb")
    );

    // Every call carried Arr's Plex headers and the token (the mock refuses otherwise).
    let log = e.mock.lock().unwrap().plex_meta_log.clone();
    assert!(
        !log.is_empty() && log.iter().all(|(_, good)| *good),
        "{log:?}"
    );
    assert!(log[0].0.contains("searchTypes=movies") && log[0].0.contains("includeMetadata=1"));
    // One search, then one details call per hit (the unmatched one included).
    assert_eq!(log.len(), 5, "{log:?}");
    // The same search again is served from the cache.
    e.json(e.get("/api/v1/discover?kind=movie&q=Plex")).await;
    assert_eq!(e.mock.lock().unwrap().plex_meta_log.len(), 6);

    // Movies are on everywhere they used to depend on a TMDB key.
    let (_, caps, _) = e.json(e.get("/api/v1/capabilities")).await;
    let cap = caps["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "movie_discovery")
        .unwrap()
        .clone();
    assert_eq!(cap["state"], "available", "{cap}");
    let (_, feed, _) = e.json(e.get("/api/v1/feed")).await;
    assert_eq!(feed["movies_enabled"], true);
    assert_eq!(feed["tmdb_backup"], false);
    let (_, ready, text) = e.json(e.get("/api/v1/readiness")).await;
    assert!(!text.contains("Movie search isn't set up"), "{ready}");
    assert!(!text.contains(PLEX_TOKEN));
}

#[tokio::test]
async fn a_plex_movie_maps_to_the_movie_record() {
    let e = start_exec(ExecOptions::default()).await;
    add(&e, PLEX_FOREIGN).await;
    let index = e.state.index();
    let m = index.movie(PLEX_FOREIGN).expect("added");
    assert_eq!(m.title, "Plex Foreign Movie");
    assert_eq!(m.year, Some(2001));
    assert_eq!(m.runtime_minutes, Some(100));
    assert_eq!(m.imdb_id.as_deref(), Some("tt9100003"));
    // The language comes from the first country; the original title is an alternative title.
    assert_eq!(m.original_language.as_deref(), Some("french"));
    assert_eq!(m.alternate_titles, ["Le Film Etranger"]);
    assert_eq!(m.in_cinemas.as_deref(), Some("2001-04-25"));
    // It has an offer, so it counts as released digitally as of now; nothing else is invented.
    assert_eq!(m.digital_release.as_deref(), Some(today().as_str()));
    assert_eq!(m.physical_release, None);

    let (s, v, text) = e
        .json(
            e.post("/api/v1/quick-add")
                .json(&json!({ "media_id": "movie:tmdb:910004" })),
        )
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (404, Some("metadata_unavailable")),
        "{text}"
    );
}

#[tokio::test]
async fn released_plex_movies_are_looked_for_and_unreleased_ones_wait() {
    let e = start_exec(ExecOptions::default()).await;
    let v = add(&e, PLEX_RELEASED).await;
    assert_eq!(v["outcome"], "looking", "{v}");
    assert_eq!(v["title"], "Plex Released Movie");
    let now = time::OffsetDateTime::now_utc();
    let m = e.state.index().movie(PLEX_RELEASED).cloned().unwrap();
    assert_eq!(m.original_language.as_deref(), Some("english"));
    assert_eq!(
        m.availability(now, 0),
        Availability::Available,
        "offers mean released"
    );

    let v = add(&e, PLEX_UNRELEASED).await;
    assert_eq!(v["outcome"], "waiting", "{v}");
    // No offers: not released; the date is the cinema date plus the usual 90 days.
    assert_eq!(v["until"], "2099-04-01", "{v}");
}

#[tokio::test]
async fn an_unreleased_movie_is_rechecked_by_the_metadata_refresh() {
    let e = start_exec(ExecOptions::default()).await;
    add(&e, PLEX_UNRELEASED).await;
    let now = time::OffsetDateTime::now_utc();
    let delay = e.state.availability_delay_days();
    let before = e.state.index().movie(PLEX_UNRELEASED).cloned().unwrap();
    assert!(matches!(
        before.availability(now, delay),
        Availability::NotYet(_)
    ));
    assert_eq!(before.digital_release, None);

    // Plex starts listing places to watch it; the next refresh notices.
    e.mock
        .lock()
        .unwrap()
        .plex_extra_offers
        .push("fut0001".into());
    let r = arr::worker::refresh_metadata(&e.state, Some(movie_id(PLEX_UNRELEASED)), now)
        .await
        .unwrap();
    assert!(r["movies"][0]["digital_release"].is_string(), "{r}");
    let after = e.state.index().movie(PLEX_UNRELEASED).cloned().unwrap();
    assert_eq!(after.digital_release.as_deref(), Some(today().as_str()));
    assert_eq!(after.availability(now, 0), Availability::Available);
    // A later refresh never moves an observed date forward.
    let later = now + time::Duration::days(3);
    arr::worker::refresh_metadata(&e.state, Some(movie_id(PLEX_UNRELEASED)), later)
        .await
        .unwrap();
    let again = e.state.index().movie(PLEX_UNRELEASED).cloned().unwrap();
    assert_eq!(again.digital_release, after.digital_release);
}

#[tokio::test]
async fn a_tmdb_key_wins_for_language_titles_and_dates_and_backs_plex_up() {
    let e = start_exec(ExecOptions {
        tmdb: true,
        ..Default::default()
    })
    .await;
    let mut tmdb = tmdb_movie_with(PLEX_RELEASED, "Plex Released Movie", 2024, "2024-07-01");
    tmdb["original_language"] = json!("fr");
    tmdb["alternative_titles"] = json!({ "titles": [{ "title": "Un Autre Titre" }] });
    e.mock
        .lock()
        .unwrap()
        .tmdb_movies
        .insert(PLEX_RELEASED.to_string(), tmdb);
    add(&e, PLEX_RELEASED).await;
    let m = e.state.index().movie(PLEX_RELEASED).cloned().unwrap();
    // TMDB's richer data wins where both answer; Plex's "on sale today" doesn't override it.
    assert_eq!(m.original_language.as_deref(), Some("french"));
    assert_eq!(m.alternate_titles, ["Un Autre Titre"]);
    assert_eq!(m.digital_release.as_deref(), Some("2024-07-01"));
    let (_, feed, _) = e.json(e.get("/api/v1/feed")).await;
    assert_eq!(feed["tmdb_backup"], true);

    // Plex has no such movie: TMDB answers (search and details).
    let (_, v, text) = e
        .json(e.get("/api/v1/discover?kind=movie&q=Leviticus"))
        .await;
    assert_eq!(v["results"][0]["media_id"], "movie:tmdb:900001", "{text}");
    let status = |name: &str| {
        v["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .map(|p| p["status"].clone())
    };
    assert_eq!(status("plex"), Some(json!("ok")));
    assert_eq!(status("tmdb"), Some(json!("ok")));
    let v = add(&e, 900001).await;
    assert_eq!(v["title"], "Leviticus", "{v}");

    // Plex failing outright: TMDB is the backup, and the failure is reported.
    e.mock.lock().unwrap().plex_meta_fail = true;
    let (_, v, text) = e
        .json(e.get("/api/v1/discover?kind=movie&q=Leviticus"))
        .await;
    assert_eq!(v["results"][0]["media_id"], "movie:tmdb:900001", "{text}");
    let plex = v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "plex")
        .unwrap();
    assert_eq!(plex["status"], "failed", "{v}");
    assert!(!text.contains(PLEX_TOKEN));
}

#[tokio::test]
async fn plex_failing_without_a_backup_is_a_clear_error() {
    let e = start_exec(ExecOptions::default()).await;
    e.mock.lock().unwrap().plex_meta_fail = true;
    let (s, v, text) = e.json(e.get("/api/v1/discover?kind=movie&q=Plex")).await;
    assert_eq!(s, 200, "{text}");
    assert!(v["results"].as_array().unwrap().is_empty());
    assert_eq!(v["providers"][1]["status"], "failed", "{v}");
}

#[tokio::test]
async fn an_imdb_only_movie_is_resolved_through_plex() {
    let e = start_exec(ExecOptions::default()).await;
    let movies = e.state.movies.as_ref().unwrap();
    assert_eq!(
        movies.find_imdb("tt9100001").await.unwrap(),
        Some(PLEX_RELEASED)
    );
    // Plex knows this one but has no TMDB id for it; junk is never sent.
    assert_eq!(movies.find_imdb("tt9100004").await.unwrap(), None);
    assert_eq!(movies.find_imdb("../x").await.unwrap(), None);
}
