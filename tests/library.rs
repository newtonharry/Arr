//! Managed catalog and library reconciliation: add (explicit metadata without TMDB, TVmaze
//! series), monitoring, removal, persistence across restart and re-import, wanted/calendar,
//! and per-root scans that never change ownership on unhealthy roots.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};
use std::path::Path;

fn movie(v: &Value, tmdb: u64) -> Option<&Value> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["tmdb_id"] == tmdb)
}

#[tokio::test]
async fn catalog_changes_persist_across_restart() {
    let mut e = start_exec(ExecOptions {
        plex: false,
        ..Default::default()
    })
    .await;

    // Without TMDB, a movie outside the catalog needs explicit metadata.
    let (s, v, _) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "movie:tmdb:900001" })),
        )
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("metadata_unavailable"))
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("supply metadata")
    );
    let (s, v, _) = e
        .json(e.post("/api/v1/library").json(
            &json!({ "media_id": "movie:tmdb:900001", "metadata": { "title": "Leviticus" } }),
        ))
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (400, Some("invalid_body")),
        "year required"
    );
    let (s, v, text) = e.json(e.post("/api/v1/library").json(&json!({ "media_id": "movie:tmdb:900001", "metadata": { "title": "Leviticus", "year": 2025, "runtime_minutes": 86 } }))).await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["created"], true);
    let (_, v, _) = e.json(e.get("/api/v1/library/movies?q=leviticus")).await;
    let m = movie(&v, 900001).unwrap();
    assert_eq!(
        m["quality_profile"], "setup:movies-hd",
        "the only movie profile is chosen"
    );
    assert_eq!(
        m["root_folder"],
        e.movies.to_str().unwrap(),
        "the only movie root is chosen"
    );
    assert_eq!(m["folder"], "Leviticus (2025)");
    assert_eq!(m["provenance"]["source_app"], "unified-media");
    assert_eq!(m["monitored"], true);
    // Adding again re-manages without duplicating.
    let (s, v, _) = e.json(e.post("/api/v1/library").json(&json!({ "media_id": "movie:tmdb:900001", "metadata": { "title": "Leviticus", "year": 2025 } }))).await;
    assert_eq!((s, v["created"].as_bool()), (201, Some(false)));

    // A series from TVmaze with future-only monitoring (all its episodes have aired).
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "series:tvdb:999001", "monitor": "future" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["media_id"], "series:tvdb:999001");
    let (_, v, _) = e
        .json(e.get("/api/v1/wanted?media_id=series:tvdb:999001"))
        .await;
    assert_eq!(
        v["total"], 0,
        "future-only: aired episodes are not monitored"
    );
    let (s, _, _) = e
        .json(
            e.patch("/api/v1/library/season:tvdb:999001:1")
                .json(&json!({ "monitored": true })),
        )
        .await;
    assert_eq!(s, 200);
    let (_, v, _) = e
        .json(e.get("/api/v1/wanted?media_id=series:tvdb:999001"))
        .await;
    assert_eq!(v["total"], 8);
    let (_, v, _) = e
        .json(e.get("/api/v1/calendar?start=2025-02-01T00:00:00Z&end=2025-02-04T00:00:00Z"))
        .await;
    assert_eq!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["series"] == "series:tvdb:999001")
            .count(),
        3
    );

    // User choices.
    let (s, _, _) = e
        .json(
            e.patch("/api/v1/library/movie:tmdb:603")
                .json(&json!({ "monitored": false })),
        )
        .await;
    assert_eq!(s, 200);
    let (s, v, _) = e
        .json(
            e.patch("/api/v1/library/movie:tmdb:424242")
                .json(&json!({ "monitored": false })),
        )
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_managed")));
    let (s, _, _) = e.json(e.delete("/api/v1/library/movie:tmdb:194")).await;
    assert_eq!(s, 200);
    let (_, v, _) = e.json(e.get("/api/v1/library/movies")).await;
    assert!(movie(&v, 194).is_none(), "removed items are hidden");
    assert_eq!(movie(&v, 603).unwrap()["monitored"], false);

    // Restart: everything persists.
    e.restart().await;
    let (_, v, _) = e.json(e.get("/api/v1/library/movies")).await;
    assert!(movie(&v, 900001).is_some());
    assert_eq!(movie(&v, 603).unwrap()["monitored"], false);
    assert!(movie(&v, 194).is_none());
}

fn write_media(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

async fn rescan(e: &Exec) -> Value {
    let (s, v, _) = e
        .json(
            e.post("/api/v1/commands")
                .json(&json!({ "name": "rescan" })),
        )
        .await;
    assert!(s == 202 || s == 200);
    let id = v["id"].as_str().unwrap().to_string();
    e.tick().await;
    let (_, v, _) = e.json(e.get(&format!("/api/v1/commands/{id}"))).await;
    assert_eq!(v["command"]["state"], "completed", "{v}");
    v["command"]["result"].clone()
}

fn owned(e: &Exec, id: &str) -> bool {
    let index = e.state.index();
    match id.parse::<arr::ids::MediaId>().unwrap() {
        arr::ids::MediaId::Movie { tmdb } => index.movie(tmdb).unwrap().has_file,
        arr::ids::MediaId::Episode(k, s, n) => {
            index
                .series(k)
                .unwrap()
                .episodes
                .iter()
                .flatten()
                .find(|e| e.season == s && e.number == n)
                .unwrap()
                .has_file
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn scans_reconcile_healthy_roots_and_change_nothing_on_unhealthy_ones() {
    let e = start_exec(ExecOptions::default()).await;
    // Empty roots while owned items are recorded: suspect, nothing changes.
    for root in [&e.movies, &e.tv] {
        for entry in std::fs::read_dir(root).unwrap().flatten() {
            std::fs::remove_dir_all(entry.path()).unwrap();
        }
    }
    let r = rescan(&e).await;
    assert_eq!(r[0]["status"], "empty_suspect", "{r}");
    assert!(owned(&e, "movie:tmdb:604") && owned(&e, "episode:tvdb:372264:1:3"));

    write_media(
        &e.movies,
        "The Matrix Reloaded (2003)/The.Matrix.Reloaded.2003.1080p.BluRay.x264-GRP.mkv",
        &mkv(2),
    );
    write_media(&e.movies, "Some Other Film (2020)/film.mkv", &mkv(1));
    write_media(
        &e.movies,
        "The Matrix (1999)/The.Matrix.1999.720p.HDTV.x264-GRP.mkv",
        &[1, 2, 3],
    );
    write_media(
        &e.tv,
        "Slow Horses/Season 1/Slow.Horses.S01E01.1080p.WEB-DL.x264-GRP.mkv",
        &mkv(2),
    );
    let r = rescan(&e).await;
    assert_eq!(
        (r[0]["status"].as_str(), r[1]["status"].as_str()),
        (Some("ok"), Some("ok")),
        "{r}"
    );
    assert!(
        r[0]["unmatched"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u == "Some Other Film (2020)")
    );
    assert!(owned(&e, "movie:tmdb:604"), "found on disk");
    assert!(
        owned(&e, "movie:tmdb:603"),
        "a present file is owned immediately, even before probing"
    );
    assert!(owned(&e, "episode:tvdb:372264:1:1"));
    assert!(
        !owned(&e, "episode:tvdb:372264:1:3"),
        "a healthy scan retires the claim of a file that is gone"
    );
    let index = e.state.index();
    let reloaded = index.movie(604).unwrap().file.clone().unwrap();
    assert_eq!(reloaded.source, "scan");
    assert_eq!(
        serde_json::to_value(reloaded.quality).unwrap(),
        "bluray_1080p"
    );
    assert!(reloaded.probe.unwrap().starts_with("ok:"));
    // The corrupt file stays owned but its probe failure is reported.
    let matrix = index.movie(603).unwrap().file.clone().unwrap();
    assert!(matrix.probe.unwrap().starts_with("failed:"));
    let (_, v, _) = e.json(e.get("/api/v1/library/roots")).await;
    assert_eq!(v["roots"].as_array().unwrap().len(), 2);

    // Unavailable root: nothing changes.
    std::fs::rename(&e.tv, e.tv.with_extension("gone")).unwrap();
    let r = rescan(&e).await;
    assert_eq!(r[1]["status"], "unavailable");
    assert!(owned(&e, "episode:tvdb:372264:1:1"));
    std::fs::rename(e.tv.with_extension("gone"), &e.tv).unwrap();

    // A scheduled rescan that would retire most files is held back; an explicit one applies.
    for n in 0..12 {
        write_media(&e.movies, &format!("Filler {n} (2001)/f.mkv"), &mkv(1));
    }
    let _ = rescan(&e).await;
    for entry in std::fs::read_dir(&e.movies).unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("The Matrix")
        {
            std::fs::remove_dir_all(entry.path()).unwrap();
        }
    }
    let report = arr::scan::scan(&e.state, time::OffsetDateTime::now_utc(), false)
        .await
        .unwrap();
    // Only two recorded files would go: below the guard, applied.
    assert_eq!(report[0].status, "ok");
    assert!(!owned(&e, "movie:tmdb:604"));
}

#[tokio::test]
async fn scheduled_mass_missing_is_held_back_but_explicit_rescan_applies() {
    let e = start_exec(ExecOptions::default()).await;
    // "Scan Show" (TVDB 999200): twelve episodes, all on disk and owned.
    for n in 1..=12 {
        write_media(
            &e.tv,
            &format!("Scan Show/Season 1/Scan.Show.S01E{n:02}.720p.HDTV.x264-GRP.mkv"),
            &mkv(1),
        );
    }
    e.add("series:tvdb:999200").await;
    assert!(owned(&e, "episode:tvdb:999200:1:7"));
    // Only one file is actually present (as if a disk were half unmounted).
    for n in 2..=12 {
        std::fs::remove_file(e.tv.join(format!(
            "Scan Show/Season 1/Scan.Show.S01E{n:02}.720p.HDTV.x264-GRP.mkv"
        )))
        .unwrap();
    }
    let report = arr::scan::scan(&e.state, time::OffsetDateTime::now_utc(), false)
        .await
        .unwrap();
    assert_eq!(report[1].status, "suspect_mass_missing", "{:?}", report[1]);
    assert!(
        owned(&e, "episode:tvdb:999200:1:7"),
        "held back: nothing retired"
    );
    let r = rescan(&e).await; // explicit command
    assert_eq!(r[1]["status"], "ok");
    assert!(!owned(&e, "episode:tvdb:999200:1:7"));
    assert!(owned(&e, "episode:tvdb:999200:1:1"));
}

#[tokio::test]
async fn scan_keeps_a_franchise_prefixed_alias_file_and_never_attributes_wrong_names() {
    let e = start_exec(ExecOptions::default()).await;
    e.add_movie(
        990001,
        "Nebula Arin: The Last Keeper",
        2026,
        110,
        "English",
        &FRANCHISE_ALIASES,
    )
    .await;
    write_media(
        &e.movies,
        &format!("{FRANCHISE_FOLDER}/{FRANCHISE_FILE}"),
        &mkv(2),
    );
    write_media(
        &e.movies,
        "The Matrix Reloaded (2003)/The.Matrix.Reloaded.2003.1080p.BluRay.x264-GRP.mkv",
        &mkv(2),
    );
    let r = rescan(&e).await;
    assert_eq!(r[0]["status"], "ok", "{r}");
    assert!(owned(&e, "movie:tmdb:990001"), "{r}");
    assert!(
        r[0]["unmatched"]
            .as_array()
            .unwrap()
            .iter()
            .all(|u| !u.as_str().unwrap().contains("Arin")),
        "{r}"
    );
    let file = e.state.index().movie(990001).unwrap().file.clone().unwrap();
    assert!(
        file.path.as_deref().unwrap().ends_with(FRANCHISE_FILE),
        "{:?}",
        file.path
    );

    // A prefix the metadata does not know, or another year, is never this movie.
    std::fs::remove_file(e.movies.join(FRANCHISE_FOLDER).join(FRANCHISE_FILE)).unwrap();
    for wrong in [
        "Galaxy.The.Legend.of.Arin.The.Last.Keeper.2026.1080p.WEBRip.AAC2.0.x264-GROUP.mp4",
        "Nebula.The.Legend.of.Arin.The.Last.Keeper.2025.1080p.WEBRip.AAC2.0.x264-GROUP.mp4",
    ] {
        write_media(&e.movies, &format!("{FRANCHISE_FOLDER}/{wrong}"), &mkv(1));
    }
    let report = arr::scan::scan(&e.state, time::OffsetDateTime::now_utc(), true)
        .await
        .unwrap();
    assert_eq!(report[0].status, "ok");
    assert!(!owned(&e, "movie:tmdb:990001"));
    let names_other = report[0]
        .unmatched
        .iter()
        .filter(|u| u.starts_with(FRANCHISE_FOLDER) && u.ends_with("(names another title)"))
        .count();
    assert_eq!(names_other, 2, "{:?}", report[0].unmatched);
}

fn episode(e: &Exec, tvdb: u32, s: u16, n: u16) -> arr::catalog::Episode {
    e.state
        .index()
        .series(arr::ids::SeriesKey::Tvdb(tvdb))
        .unwrap()
        .episodes
        .iter()
        .flatten()
        .find(|x| x.season == s && x.number == n)
        .unwrap()
        .clone()
}

/// A published release advertising `gib` (sizes are judged against the item's runtime).
fn published(e: &Exec, title: &str, gib: f64) -> (String, Value) {
    e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        gib,
        false,
    )
}

#[tokio::test]
async fn first_episode_of_a_new_series_grabs_only_that_episode_even_when_added_concurrently() {
    let e = start_exec(ExecOptions::default()).await;
    let (e1_hash, e1) = published(&e, "Parity.Partial.S01E01.1080p.WEB-DL.x264-NEW", 0.05);
    let (_, e2) = published(&e, "Parity.Partial.S01E02.1080p.WEB-DL.x264-NEW", 0.05);
    let (_, pack) = published(&e, "Parity.Partial.S01.1080p.WEB-DL.x264-NEW", 0.05);
    e.search_results("Parity Partial S01E01", vec![e1, pack.clone()]);
    e.search_results("Parity Partial S01E02", vec![e2]);
    e.search_results("Parity Partial S01", vec![pack]);
    // The same default (search) add sent twice at once.
    let add = || {
        e.json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "episode:tvdb:999005:1:1" })),
        )
    };
    let (a, b) = tokio::join!(add(), add());
    assert_eq!((a.0, b.0), (201, 201), "{} {}", a.2, b.2);
    let mut created: Vec<bool> = [&a.1, &b.1]
        .iter()
        .map(|v| v["created"].as_bool().unwrap())
        .collect();
    created.sort();
    assert_eq!(created, [false, true], "one call created the series");
    let states: Vec<&str> = [&a.1, &b.1]
        .iter()
        .map(|v| v["plan"]["state"].as_str().unwrap())
        .collect();
    assert!(
        states.contains(&"download_queued") && states.contains(&"in_progress"),
        "{states:?}"
    );
    for v in [&a.1, &b.1] {
        assert_eq!(v["scope"], "episode:tvdb:999005:1:1");
        let units: Vec<&str> = v["plan"]["units"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["target"].as_str().unwrap())
            .collect();
        assert_eq!(units, ["episode:tvdb:999005:1:1"], "{v}");
    }
    // Only the requested episode is monitored on the new series.
    assert!(episode(&e, 999005, 1, 1).monitored);
    for (s, n) in [(1, 2), (1, 3), (2, 1)] {
        assert!(!episode(&e, 999005, s, n).monitored, "S{s}E{n}");
    }
    e.tick().await;
    let q = e.fake.lock().unwrap();
    assert_eq!(q.add_fields.len(), 1, "one grab: no E02, no season pack");
    assert!(q.torrents.contains_key(&e1_hash));
}

#[tokio::test]
async fn next_missing_episode_of_a_partly_owned_series_keeps_adjacent_choices() {
    let e = start_exec(ExecOptions::default()).await;
    // Imported Slow Horses: S1 E1 and E3 owned, E2/E4-E6 missing. The operator had
    // unmonitored E4 and season 2.
    for scope in ["episode:tvdb:372264:1:4", "season:tvdb:372264:2"] {
        let (s, _, text) = e
            .json(
                e.patch(&format!("/api/v1/library/{scope}"))
                    .json(&json!({ "monitored": false })),
            )
            .await;
        assert_eq!(s, 200, "{text}");
    }
    let (e2_hash, e2) = published(&e, "Slow.Horses.S01E02.1080p.WEB-DL.x264-NEXT", 1.5);
    let (_, e4) = published(&e, "Slow.Horses.S01E04.1080p.WEB-DL.x264-NEXT", 1.5);
    let (_, pack) = published(&e, "Slow.Horses.S01.1080p.WEB-DL.x264-NEXT", 1.5);
    e.search_results("Slow Horses S01E02", vec![e2, pack.clone()]);
    e.search_results("Slow Horses S01E04", vec![e4]);
    e.search_results("Slow Horses S01", vec![pack]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(
        (v["created"].as_bool(), v["plan"]["state"].as_str()),
        (Some(false), Some("download_queued")),
        "{text}"
    );
    assert_eq!(v["plan"]["execution"]["outcome"], "queued");
    let units: Vec<&str> = v["plan"]["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["target"].as_str().unwrap())
        .collect();
    assert_eq!(units, ["episode:tvdb:372264:1:2"], "{text}");
    // Adjacent choices and ownership are untouched.
    assert!(!episode(&e, 372264, 1, 4).monitored, "E4 stays unmonitored");
    assert!(episode(&e, 372264, 1, 5).monitored);
    assert!(episode(&e, 372264, 1, 1).has_file && episode(&e, 372264, 1, 3).has_file);
    let season2: bool = e
        .state
        .store
        .blocking(|c| {
            Ok(c.query_row(
                "SELECT monitored FROM seasons WHERE series_id = 'series:tvdb:372264' AND season = 2",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(!season2, "season 2 stays unmonitored");
    e.tick().await;
    {
        let q = e.fake.lock().unwrap();
        assert_eq!(q.add_fields.len(), 1, "only E02: no E04, no season pack");
        assert!(q.torrents.contains_key(&e2_hash));
    }
    // A replay creates nothing new and leaves no request behind.
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2" })),
        )
        .await;
    assert_eq!(
        (s, v["plan"]["state"].as_str()),
        (201, Some("in_progress")),
        "{text}"
    );
    e.tick().await;
    assert_eq!(e.fake.lock().unwrap().add_fields.len(), 1);
    let (_, r, text) = e.json(e.get("/api/v1/requests")).await;
    assert!(
        r["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["state"] != "active"),
        "{text}"
    );
}

#[tokio::test]
async fn previewing_a_movie_without_an_approved_release_changes_nothing_with_execution_on() {
    let e = start_exec(ExecOptions::default()).await;
    // Only unacceptable releases: wrong year, a quality the profile does not allow, no seeders.
    let (_, wrong_year) = published(&e, "The.Matrix.2021.1080p.WEB-DL.x264-NEW", 0.05);
    let (_, uhd) = published(&e, "The.Matrix.1999.2160p.WEB-DL.x264-NEW", 0.05);
    let (_, mut dead) = published(&e, "The.Matrix.1999.1080p.WEB-DL.x264-DEAD", 0.05);
    dead["seeders"] = json!(0);
    e.search_results("The Matrix 1999", vec![wrong_year, uhd, dead]);
    let t0 = time::OffsetDateTime::now_utc();
    let (s, v, text) = e.json(e.get("/api/v1/media/movie:tmdb:603/releases")).await;
    assert_eq!(s, 200, "{text}");
    assert_no_leaks(&text);
    assert_eq!(v["counts"]["approved"], 0, "{text}");
    let releases = v["releases"].as_array().unwrap();
    assert_eq!(releases.len(), 3);
    assert!(
        releases.iter().all(|r| r["decision"]["approved"] == false
            && !r["decision"]["rejections"].as_array().unwrap().is_empty()),
        "{text}"
    );
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_no_leaks(&text);
    let plan = &v["plan"];
    assert_eq!(plan["state"], "blocked", "{text}");
    assert_eq!(plan["units"][0]["status"], "no_approved_release", "{text}");
    assert_eq!(plan["candidates"], json!([]));
    assert_eq!(plan["execution"]["mode"], "shadow");
    assert!(plan["execution"].get("outcome").is_none());
    // Neither now nor later does anything reach the outbox, qBittorrent or the requests.
    for hours in [0, 2, 26] {
        e.tick_at(t0 + std::time::Duration::from_secs(hours * 3600))
            .await;
    }
    assert!(e.fake.lock().unwrap().add_fields.is_empty());
    let (_, q, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"], json!([]));
    let (_, r, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(r["items"], json!([]));
    let (_, b, _) = e.json(e.get("/api/v1/blocklist")).await;
    assert_eq!(b["items"], json!([]));
}

// ------------------------------------------------------------------ status and numbering
// (moved from migration_review.rs: the behaviour is general, not about Sonarr)

#[tokio::test]
async fn episode_status_uses_actual_coverage_not_the_original_season_request() {
    let e = start_exec(ExecOptions::default()).await;
    for number in [2, 4] {
        let (_, release) = published(
            &e,
            &format!("Slow.Horses.S01E{number:02}.1080p.WEB-DL.x264-UM"),
            1.5,
        );
        e.search_results(&format!("Slow Horses S01E{number:02}"), vec![release]);
    }
    let (status, _, body) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:372264:1" })),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    e.tick().await;
    let (status, episode, body) = e
        .json(e.get("/api/v1/status/episode:tvdb:372264:1:2"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        episode["downloads"].as_array().unwrap().len(),
        1,
        "a broader original season request must not attach the other episode's grab: {body}"
    );
    assert_eq!(
        episode["downloads"][0]["id"],
        episode["episodes"][0]["download"]["id"]
    );
}

#[tokio::test]
async fn season_status_includes_pending_individual_episode_requests() {
    let e = start_exec(ExecOptions::default()).await;
    let (status, _, body) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2" })),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let (_, requests, _) = e.json(e.get("/api/v1/requests")).await;
    assert!(
        requests["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["target"] == "episode:tvdb:372264:1:2" && r["state"] == "active")
    );
    let (status, season, body) = e.json(e.get("/api/v1/status/season:tvdb:372264:1")).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        season["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["target"] == "episode:tvdb:372264:1:2" && r["state"] == "active"),
        "a season must include its episode request despite the different ID kinds: {body}"
    );
    let (status, _, body) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:372264:1" })),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let (_, episode, body) = e
        .json(e.get("/api/v1/status/episode:tvdb:372264:1:2"))
        .await;
    assert!(
        episode["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["target"] == "season:tvdb:372264:1" && r["state"] == "active"),
        "the episode should expose its encompassing season request: {body}"
    );
}

#[tokio::test]
async fn season_status_includes_individual_grabs_with_either_series_namespace() {
    let e = start_exec(ExecOptions::default()).await;
    let (_, release) = published(&e, "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM", 1.5);
    e.search_results("Slow Horses S01E02", vec![release]);
    let (status, _, body) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2" })),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    e.tick().await;
    for scope in ["season:tvdb:372264:1", "season:tvmaze:9001:1"] {
        let (status, season, body) = e.json(e.get(&format!("/api/v1/status/{scope}"))).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            season["downloads"].as_array().unwrap().len(),
            1,
            "season status must contain the episode grab, including canonical-ID aliases: {body}"
        );
        assert_eq!(season["downloads"][0]["target"], "episode:tvdb:372264:1:2");
    }
}
