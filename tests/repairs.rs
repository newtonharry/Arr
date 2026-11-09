//! Regressions for real-client sandbox findings: payload identity, scan-before-search,
//! scoped one-call adds, effective quality from media info, and movie availability.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};
use std::time::Duration;
use time::OffsetDateTime;

/// Publishes a release whose search-result title is `title` but whose payload is `files`
/// (torrent name = the single file's name, or `name` for several files).
fn release_of(
    e: &Exec,
    title: &str,
    name: &str,
    files: Vec<(String, Vec<u8>)>,
    seeders: i64,
    magnet_dn: bool,
) -> (String, Value) {
    let (hash, mut r) = e.publish(title, name, files, 0.05, magnet_dn);
    r["seeders"] = json!(seeders);
    if !magnet_dn {
        r["magnetUrl"] = Value::Null;
    }
    (hash, r)
}

fn episode_state(e: &Exec, tvdb: u32, s: u16, n: u16) -> arr::catalog::Episode {
    let index = e.state.index();
    let series = index.series(arr::ids::SeriesKey::Tvdb(tvdb)).unwrap();
    series
        .episodes
        .iter()
        .flatten()
        .find(|x| x.season == s && x.number == n)
        .unwrap()
        .clone()
}

fn qb_adds(e: &Exec) -> usize {
    e.fake.lock().unwrap().add_fields.len()
}

#[tokio::test]
async fn season_add_scans_first_and_grabs_only_the_requested_scope() {
    let e = start_exec(ExecOptions::default()).await;
    // E01 is already in the library (named 1080p, actually 720p).
    let existing =
        e.tv.join("Parity Partial/Season 1/Parity.Partial.S01E01.1080p.WEB-DL.x264-OLD.mkv");
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::fs::write(&existing, mkv_sized(2, 1280, 720)).unwrap();
    let (_, e1) = release_of(
        &e,
        "Parity.Partial.S01E01.1080p.WEB-DL.x264-NEW",
        "",
        vec![(
            "Parity.Partial.S01E01.1080p.WEB-DL.x264-NEW.mkv".into(),
            mkv(2),
        )],
        50,
        false,
    );
    let (e2_hash, e2) = release_of(
        &e,
        "Parity.Partial.S01E02.1080p.WEB-DL.x264-NEW",
        "",
        vec![(
            "Parity.Partial.S01E02.1080p.WEB-DL.x264-NEW.mkv".into(),
            mkv(3),
        )],
        50,
        false,
    );
    let (_, s2) = release_of(
        &e,
        "Parity.Partial.S02E01.1080p.WEB-DL.x264-NEW",
        "",
        vec![(
            "Parity.Partial.S02E01.1080p.WEB-DL.x264-NEW.mkv".into(),
            mkv(1),
        )],
        50,
        false,
    );
    e.search_results("Parity Partial S01E01", vec![e1]);
    e.search_results("Parity Partial S01E02", vec![e2]);
    e.search_results("Parity Partial S02E01", vec![s2.clone()]);
    e.search_results("Parity Partial S02", vec![s2]);

    // One call, default search, season scope.
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:999005:1" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["created"], true);
    assert_eq!(v["scope"], "season:tvdb:999005:1");
    assert_eq!(v["scan"]["status"], "ok", "{text}");
    assert_eq!(v["scan"]["files"], 1);
    let units: Vec<(String, String)> = v["plan"]["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["label"].as_str().unwrap().into(),
                u["status"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        units,
        [
            ("S01E01".into(), "owned".into()),
            ("S01E02".into(), "planned".into()),
            ("S01E03".into(), "not_aired".into())
        ],
        "{text}"
    );
    assert_eq!(v["plan"]["state"], "download_queued");
    // The existing file's recorded quality follows its real resolution.
    let e01 = episode_state(&e, 999005, 1, 1);
    assert!(e01.has_file);
    assert_eq!(
        serde_json::to_value(e01.file.unwrap().quality).unwrap(),
        "webdl_720p"
    );
    // Only the requested season is monitored on the new series.
    assert!(
        episode_state(&e, 999005, 1, 3).monitored,
        "future episodes of the season stay monitored"
    );
    assert!(!episode_state(&e, 999005, 2, 1).monitored);
    e.tick().await;
    assert_eq!(qb_adds(&e), 1);
    assert!(e.fake.lock().unwrap().torrents.contains_key(&e2_hash));

    // Replays and concurrent calls never duplicate.
    let (s, v, _) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:999005:1" })),
        )
        .await;
    assert_eq!((s, v["created"].as_bool()), (201, Some(false)));
    assert_eq!(v["plan"]["state"], "in_progress", "{v}");
    let (a, b) = tokio::join!(
        e.json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:999005:1" }))
        ),
        e.json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:999005:1" }))
        ),
    );
    assert_eq!((a.0, b.0), (201, 201));
    e.tick().await;
    assert_eq!(qb_adds(&e), 1, "still one download");
    let (_, w, _) = e
        .json(e.get("/api/v1/wanted?media_id=series:tvdb:999005"))
        .await;
    assert_eq!(
        w["total"], 0,
        "S01E02 is downloading; season 2 is not monitored: {w}"
    );

    // Later metadata: a new S1 episode is monitored (its season is), a new season is not.
    e.mock.lock().unwrap().tvmaze_extra.insert(
        "9005".into(),
        vec![
            json!({ "season": 1, "number": 4, "name": "Ep 4", "type": "regular", "airstamp": "2025-02-01T02:00:00+00:00", "runtime": 1 }),
            json!({ "season": 3, "number": 1, "name": "Ep 1", "type": "regular", "airstamp": "2025-09-01T02:00:00+00:00", "runtime": 1 }),
        ],
    );
    let (s, _, _) = e
        .json(
            e.post("/api/v1/commands")
                .json(&json!({ "name": "refresh_metadata", "media_id": "series:tvdb:999005" })),
        )
        .await;
    assert_eq!(s, 202);
    e.tick().await;
    assert!(
        episode_state(&e, 999005, 1, 4).monitored,
        "the operator refresh bypassed the metadata cache"
    );
    assert!(!episode_state(&e, 999005, 3, 1).monitored);

    // Explicit monitor-only add of the other season: no search.
    let (s, v, _) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:999005:2", "search": false })),
        )
        .await;
    assert_eq!(s, 201);
    assert!(v.get("plan").is_none());
    assert!(episode_state(&e, 999005, 2, 1).monitored);
    e.tick().await;
    assert_eq!(qb_adds(&e), 1);
    // Scope options that contradict a scoped add are refused.
    let (s, _, _) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:999005:1", "monitor": "all" })),
        )
        .await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn contradictory_payloads_are_blocklisted_and_replaced_by_a_valid_alternative() {
    let e = start_exec(ExecOptions::default()).await;
    let (s, _, _) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:999005:1", "search": false })),
        )
        .await;
    assert_eq!(s, 201);
    // The higher-seeded result claims Parity Partial; its .torrent is Wrong Show.
    let (wrong, fake) = release_of(
        &e,
        "Parity.Partial.S01E02.1080p.WEB-DL.x264-FAKE",
        "",
        vec![(
            "Wrong.Show.S01E02.1080p.WEB-DL.x264-FAKE.mkv".into(),
            mkv(2),
        )],
        90,
        false,
    );
    let (good_hash, good) = release_of(
        &e,
        "Parity.Partial.S01E02.1080p.WEB-DL.x264-GOOD",
        "",
        vec![(
            "Parity.Partial.S01E02.1080p.WEB-DL.x264-GOOD.mkv".into(),
            mkv(3),
        )],
        10,
        false,
    );
    e.search_results("Parity Partial S01E02", vec![fake, good]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:999005:1:2", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(
        v["plan"]["execution"]["grabs"].as_array().unwrap().len(),
        1,
        "{text}"
    );
    assert!(
        v["plan"]["candidates"][0]["title"]
            .as_str()
            .unwrap()
            .ends_with("-GOOD")
    );
    let (_, b, btext) = e
        .json(e.get("/api/v1/blocklist?media_id=series:tvdb:999005"))
        .await;
    assert_eq!(b["items"][0]["reason"], "wrong_payload");
    assert!(b["items"][0]["title"].as_str().unwrap().ends_with("-FAKE"));
    assert!(!btext.contains(&wrong));
    e.tick().await;
    assert_eq!(qb_adds(&e), 1, "the wrong payload never reached the client");
    e.complete(&good_hash);
    e.tick().await;
    assert!(
        e.tv.join("Parity Partial/Season 1/Parity.Partial.S01E02.1080p.WEB-DL.x264-GOOD.mkv")
            .is_file()
    );
    assert!(episode_state(&e, 999005, 1, 2).has_file);
}

#[tokio::test]
async fn a_wrong_payload_found_at_import_fails_and_recovers_automatically() {
    let e = start_exec(ExecOptions::default()).await;
    e.json(
        e.post("/api/v1/library")
            .json(&json!({ "media_id": "season:tvdb:999005:1", "search": false })),
    )
    .await;
    // A magnet without a display name: the identity is only visible once downloaded.
    let (wrong_hash, mut fake) = release_of(
        &e,
        "Parity.Partial.S01E02.1080p.WEB-DL.x264-FAKE",
        "",
        vec![(
            "Wrong.Show.S01E02.1080p.WEB-DL.x264-FAKE.mkv".into(),
            mkv(2),
        )],
        90,
        true,
    );
    fake["magnetUrl"] = json!(format!("magnet:?xt=urn:btih:{wrong_hash}"));
    let (good_hash, good) = release_of(
        &e,
        "Parity.Partial.S01E02.1080p.WEB-DL.x264-GOOD",
        "",
        vec![(
            "Parity.Partial.S01E02.1080p.WEB-DL.x264-GOOD.mkv".into(),
            mkv(3),
        )],
        10,
        false,
    );
    e.search_results("Parity Partial S01E02", vec![fake, good]);
    let (_, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:999005:1:2", "execute": true })),
        )
        .await;
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    e.complete(&wrong_hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(
        (g.state.as_str(), g.failure.as_deref(), g.retry.as_deref()),
        ("failed", Some("wrong_payload"), Some("research")),
        "{:?}",
        g.last_error
    );
    assert!(
        !episode_state(&e, 999005, 1, 2).has_file,
        "never imported as the requested episode"
    );
    assert!(
        !e.tv
            .join("Parity Partial/Season 1/Wrong.Show.S01E02.1080p.WEB-DL.x264-FAKE.mkv")
            .exists()
    );
    // The queued re-search picks the valid alternative (the wrong one is blocklisted); a grab
    // queued during a pass may be submitted by the next one (timestamps are whole seconds).
    e.tick().await;
    e.tick().await;
    assert!(
        e.fake.lock().unwrap().torrents.contains_key(&good_hash),
        "alternative grabbed"
    );
    e.complete(&good_hash);
    e.tick().await;
    assert!(episode_state(&e, 999005, 1, 2).has_file);
}

#[tokio::test]
async fn movie_payloads_naming_another_year_or_title_are_refused() {
    let e = start_exec(ExecOptions::default()).await;
    let (_, yr) = release_of(
        &e,
        "The.Matrix.1999.1080p.WEB-DL.x264-YR",
        "",
        vec![("The.Matrix.2021.1080p.WEB-DL.x264-YR.mkv".into(), mkv(2))],
        90,
        false,
    );
    let (_, other) = release_of(
        &e,
        "The.Matrix.1999.1080p.WEB-DL.x264-OT",
        "Other.Movie.2020.1080p.WEB-DL.x264-OT",
        vec![
            ("Other.Movie.2020.1080p.WEB-DL.x264-OT.mkv".into(), mkv(2)),
            ("notes.txt".into(), b"x".to_vec()),
        ],
        80,
        false,
    );
    // Obfuscated file name inside a correctly named torrent: accepted on context.
    let (ok_hash, ok) = release_of(
        &e,
        "The.Matrix.1999.1080p.WEB-DL.x264-OK",
        "The.Matrix.1999.1080p.WEB-DL.x264-OK",
        vec![
            ("a1b2c3.mkv".into(), mkv(3)),
            ("readme.txt".into(), b"x".to_vec()),
        ],
        10,
        false,
    );
    e.search_results("The Matrix 1999", vec![yr, other, ok]);
    let (_, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    assert!(
        v["plan"]["candidates"][0]["title"]
            .as_str()
            .unwrap()
            .ends_with("-OK"),
        "{text}"
    );
    let (_, b, _) = e
        .json(e.get("/api/v1/blocklist?media_id=movie:tmdb:603"))
        .await;
    assert_eq!(b["items"].as_array().unwrap().len(), 2);
    e.tick().await;
    e.complete(&ok_hash);
    e.tick().await;
    assert!(
        e.movies
            .join("The Matrix (1999)/The.Matrix.1999.1080p.WEB-DL.x264-OK.mkv")
            .is_file(),
        "single obfuscated file takes the download title"
    );
}

#[tokio::test]
async fn scans_do_not_attribute_files_that_name_another_title() {
    let e = start_exec(ExecOptions::default()).await;
    let season = e.tv.join("Parity Partial/Season 1");
    std::fs::create_dir_all(&season).unwrap();
    std::fs::write(season.join("Wrong.Show.S01E02.1080p.WEB-DL.mkv"), mkv(1)).unwrap();
    std::fs::write(
        season.join("Parity.Partial.S01E01.1080p.WEB-DL.mkv"),
        mkv(1),
    )
    .unwrap();
    let (_, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:999005:1", "search": false })),
        )
        .await;
    assert_eq!(v["scan"]["files"], 1, "{text}");
    assert!(
        v["scan"]["unmatched"].as_array().unwrap().iter().any(|u| u
            .as_str()
            .unwrap()
            .contains("Wrong.Show")
            && u.as_str().unwrap().contains("names another title"))
    );
    assert!(episode_state(&e, 999005, 1, 1).has_file);
    assert!(
        !episode_state(&e, 999005, 1, 2).has_file,
        "a folder alone never establishes ownership"
    );
}

#[tokio::test]
async fn measured_resolution_decides_quality_and_profile_admission() {
    let e = start_exec(ExecOptions::default()).await;
    // Advertised 1080p, actually 2160p (the HD profile does not allow 2160p).
    let (uhd_hash, uhd) = release_of(
        &e,
        "The.Matrix.1999.1080p.WEB-DL.x264-UHD",
        "",
        vec![(
            "The.Matrix.1999.1080p.WEB-DL.x264-UHD.mkv".into(),
            mkv_sized(2, 3840, 2160),
        )],
        90,
        false,
    );
    // Cropped widescreen 1920x800 is still 1080p.
    let (crop_hash, crop) = release_of(
        &e,
        "The.Matrix.1999.1080p.WEB-DL.x264-CROP",
        "",
        vec![(
            "The.Matrix.1999.1080p.WEB-DL.x264-CROP.mkv".into(),
            mkv_sized(3, 1920, 800),
        )],
        10,
        false,
    );
    e.search_results("The Matrix 1999", vec![uhd, crop]);
    let (_, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    e.complete(&uhd_hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(
        g.failure.as_deref(),
        Some("wrong_quality"),
        "{:?}",
        g.last_error
    );
    let err = g.last_error.unwrap();
    assert!(
        err.contains("webdl_2160p") && err.contains("3840x2160") && err.contains("does not allow"),
        "{err}"
    );
    assert!(
        !e.movies
            .join("The Matrix (1999)/The.Matrix.1999.1080p.WEB-DL.x264-UHD.mkv")
            .exists()
    );
    // Automatic alternative: the cropped HD release imports as webdl_1080p (re-search, then
    // the next pass submits it).
    e.tick().await;
    e.tick().await;
    e.complete(&crop_hash);
    e.tick().await;
    let index = e.state.index();
    let m = index.movie(603).unwrap();
    assert!(m.has_file);
    assert_eq!(
        serde_json::to_value(m.file.as_ref().unwrap().quality).unwrap(),
        "webdl_1080p"
    );
}

#[tokio::test]
async fn movie_availability_follows_release_evidence_and_refreshes() {
    let e = start_exec(ExecOptions {
        tmdb: true,
        ..Default::default()
    })
    .await;
    let today = OffsetDateTime::now_utc().date();
    let soon = (today + time::Duration::days(2)).to_string();
    e.mock.lock().unwrap().tmdb_movies.insert(
        "900002".into(),
        tmdb_movie_with(900002, "Parity Feature", 2026, &soon),
    );
    let (feature_hash, feature) = release_of(
        &e,
        "Parity.Feature.2026.1080p.WEB-DL.x264-UM",
        "",
        vec![(
            "Parity.Feature.2026.1080p.WEB-DL.x264-UM.mkv".into(),
            mkv(2),
        )],
        20,
        false,
    );
    e.search_results("Parity Feature 2026", vec![feature]);
    // Added (search on by default) before its release: monitored, nothing grabbed.
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "movie:tmdb:900002" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["plan"]["state"], "blocked", "{text}");
    assert!(text.contains("not released yet"), "{text}");
    let (_, r, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(r["items"][0]["target"], "movie:tmdb:900002");
    // After the digital release the retried request grabs it.
    let later = OffsetDateTime::now_utc() + Duration::from_secs(3 * 86_400);
    e.tick_at(later).await; // the request retry plans and queues the grab
    e.tick_at(later + Duration::from_secs(60)).await; // the next pass submits it
    assert!(
        e.fake.lock().unwrap().torrents.contains_key(&feature_hash),
        "grabbed once released"
    );

    // Unknown evidence is explicit, never guessed: a far-future TMDB date, then a refresh.
    e.mock.lock().unwrap().tmdb_movies.insert(
        "900004".into(),
        tmdb_movie_with(900004, "Parity Later", 2026, "2099-01-01"),
    );
    let (s, v, _) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "movie:tmdb:900004", "search": false })),
        )
        .await;
    assert_eq!(s, 201, "{v}");
    let (_, w, _) = e
        .json(e.get("/api/v1/wanted?media_id=movie:tmdb:900004"))
        .await;
    assert_eq!(w["total"], 0);
    e.mock.lock().unwrap().tmdb_movies.insert(
        "900004".into(),
        tmdb_movie_with(900004, "Parity Later", 2026, "2020-01-01"),
    );
    e.json(
        e.post("/api/v1/commands")
            .json(&json!({ "name": "refresh_metadata", "media_id": "movie:tmdb:900004" })),
    )
    .await;
    e.tick().await;
    let (_, w, _) = e
        .json(e.get("/api/v1/wanted?media_id=movie:tmdb:900004"))
        .await;
    assert_eq!(
        w["total"], 1,
        "available after the refreshed release evidence: {w}"
    );
}

#[tokio::test]
async fn operator_release_evidence() {
    let e = start_exec(ExecOptions::default()).await;
    // A movie whose digital release is in 2099 is not available before then.
    let (s, _, text) = e.json(e.post("/api/v1/library").json(&json!({ "media_id": "movie:tmdb:900007", "search": false, "metadata": { "title": "Parity Future", "year": 2026, "runtime_minutes": 100, "digital_release": "2099-01-01" } }))).await;
    assert_eq!(s, 201, "{text}");
    let (_, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:900007" })),
        )
        .await;
    assert_eq!(v["plan"]["blockers"][0]["code"], "not_available", "{text}");
    assert!(text.contains("2099-01-01"));
    // Operator metadata without dates: added and monitored, explicitly not available.
    let (s, v, text) = e.json(e.post("/api/v1/library").json(&json!({ "media_id": "movie:tmdb:900003", "metadata": { "title": "Parity Undated", "year": 2026 } }))).await;
    assert_eq!(s, 201, "{text}");
    assert!(
        v["plan"]["blockers"][0]["message"]
            .as_str()
            .unwrap()
            .contains("no release date is known"),
        "{text}"
    );
    // With an explicit past digital release it is available.
    let (s, v, text) = e.json(e.post("/api/v1/library").json(&json!({ "media_id": "movie:tmdb:900005", "search": false, "metadata": { "title": "Parity Dated", "year": 2025, "digital_release": "2025-02-01" } }))).await;
    assert_eq!(s, 201, "{text}");
    let _ = v;
    let (_, w, _) = e
        .json(e.get("/api/v1/wanted?media_id=movie:tmdb:900005"))
        .await;
    assert_eq!(w["total"], 1);
    let (s, _, _) = e.json(e.post("/api/v1/library").json(&json!({ "media_id": "movie:tmdb:900006", "metadata": { "title": "Bad Date", "year": 2025, "digital_release": "soon" } }))).await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn payload_source_markers_outrank_the_indexer_claim() {
    let e = start_exec(ExecOptions::default()).await;
    // Listed as WEB-DL; the payload itself says HDTV (and is 1080p): recorded as hdtv_1080p.
    let (hash, r) = release_of(
        &e,
        "The.Matrix.1999.1080p.WEB-DL.x264-SRC",
        "",
        vec![("The.Matrix.1999.1080p.HDTV.x264-SRC.mkv".into(), mkv(2))],
        20,
        false,
    );
    e.search_results("The Matrix 1999", vec![r]);
    e.json(
        e.post("/api/v1/plans")
            .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
    )
    .await;
    e.tick().await;
    e.complete(&hash);
    e.tick().await;
    let index = e.state.index();
    let m = index.movie(603).unwrap();
    assert!(m.has_file);
    assert_eq!(
        serde_json::to_value(m.file.as_ref().unwrap().quality).unwrap(),
        "hdtv_1080p"
    );
}

#[tokio::test]
async fn explicit_payload_language_is_checked_against_the_profile() {
    let e = start_exec(ExecOptions::default()).await;
    // The movie profile wants the original language (English). The listing is unmarked (approved); the payload says RUSSIAN.
    let (ru_hash, ru) = release_of(
        &e,
        "The.Matrix.1999.1080p.WEB-DL.x264-RU",
        "",
        vec![(
            "The.Matrix.1999.RUSSIAN.1080p.WEB-DL.x264-RU.mkv".into(),
            mkv(2),
        )],
        90,
        false,
    );
    // Unmarked English alternative.
    let (en_hash, en) = release_of(
        &e,
        "The.Matrix.1999.1080p.WEB-DL.x264-EN",
        "",
        vec![("The.Matrix.1999.1080p.WEB-DL.x264-EN.mkv".into(), mkv(3))],
        10,
        false,
    );
    e.search_results("The Matrix 1999", vec![ru, en]);
    let (_, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    e.complete(&ru_hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(
        (g.failure.as_deref(), g.retry.as_deref()),
        (Some("wrong_language"), Some("research")),
        "{:?}",
        g.last_error
    );
    assert!(g.last_error.unwrap().contains("russian"));
    assert!(!e.state.index().movie(603).unwrap().has_file);
    let (_, b, _) = e
        .json(e.get("/api/v1/blocklist?media_id=movie:tmdb:603"))
        .await;
    assert_eq!(b["items"][0]["reason"], "wrong_language");
    // The automatic alternative (unmarked, approved for English) imports (re-search, then the
    // next pass submits it).
    e.tick().await;
    e.tick().await;
    assert!(e.fake.lock().unwrap().torrents.contains_key(&en_hash));
    e.complete(&en_hash);
    e.tick().await;
    assert!(e.state.index().movie(603).unwrap().has_file);

    // Sonarr v4 profile: any language. An explicitly Russian episode payload imports.
    let title = "Slow.Horses.S01E02.RUSSIAN.1080p.WEB-DL.x264-RU";
    let (tv_hash, r) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E02", vec![r]);
    let (_, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2", "execute": true })),
        )
        .await;
    let gid = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    e.tick().await;
    e.complete(&tv_hash);
    e.tick().await;
    assert_eq!(e.grab_row(&gid).state, "imported");
}
