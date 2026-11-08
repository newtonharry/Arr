//! Coexistence mode: the native apps keep running and keep everything that is not adopted
//! here. A fresh full import acquires nothing; adoption through `POST /library` makes exactly
//! the requested scope effective (through re-imports and metadata refreshes too); shared
//! native qBittorrent categories are refused; a native file appearing during a download is
//! never duplicated; readiness and status are computed from live state and recorded evidence.
#![recursion_limit = "256"]

mod common;

use arr::fsutil;
use arr::import::{self, ImportOptions};
use common::*;
use serde_json::{Value, json};

const CATS: Option<(&str, &str)> = Some(("um-movies", "um-tv"));

fn coexistence() -> ExecOptions {
    ExecOptions {
        mode: "coexistence",
        categories: CATS,
        ..Default::default()
    }
}

fn reimport(e: &Exec, snap: &Value) {
    let p = e.dir.join("native-next.json");
    let _ = std::fs::remove_file(&p);
    fsutil::write_new_private_file(&p, &serde_json::to_vec(snap).unwrap()).unwrap();
    import::run_import(&ImportOptions {
        state_dir: e.dir.clone(),
        snapshot: p,
        observed_at: Some(time::macros::datetime!(2026-10-02 12:00 UTC)),
        dry_run: false,
    })
    .unwrap();
}

fn feed_item(e: &Exec, title: &str, gib: f64) -> FeedItem {
    let (hash, _) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        gib,
        false,
    );
    FeedItem {
        title: title.into(),
        guid: format!("guid-{hash}"),
        url: format!(
            "http://{}/prowlarr/1/download?apikey={PROWLARR_KEY}&link={hash}",
            e.mock_addr
        ),
        size: (gib * 1024.0 * 1024.0 * 1024.0) as i64,
        seeders: 20,
        info_hash: Some(hash),
        categories: if title.starts_with("The.Matrix") {
            vec![2040]
        } else {
            vec![5040]
        },
        pub_date: "Mon, 26 Oct 2026 08:00:00 +0000".into(),
    }
}

async fn command(e: &Exec, body: Value) -> Value {
    let (s, v, text) = e.json(e.post("/api/v1/commands").json(&body)).await;
    assert!(s == 202 || s == 200, "{text}");
    let id = v["id"].as_str().unwrap().to_string();
    e.tick().await;
    let (_, v, _) = e.json(e.get(&format!("/api/v1/commands/{id}"))).await;
    assert_eq!(v["command"]["state"], "completed", "{v}");
    v["command"]["result"].clone()
}

fn adds(e: &Exec) -> usize {
    e.qb.lock().unwrap().add_fields.len()
}

/// Effective monitored episodes of a series in the live index.
fn monitored(e: &Exec, tvdb: u32) -> Vec<(u16, u16)> {
    let index = e.state.index();
    let s = index.series(arr::ids::SeriesKey::Tvdb(tvdb)).unwrap();
    if !s.monitored {
        return vec![];
    }
    s.episodes
        .iter()
        .flatten()
        .filter(|x| x.monitored)
        .map(|x| (x.season, x.number))
        .collect()
}

#[tokio::test]
async fn a_full_import_acquires_nothing_until_a_title_is_adopted() {
    let e = start_exec(coexistence()).await;
    let (matrix_hash, matrix) = e.publish(
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        vec![("The.Matrix.1999.1080p.WEB-DL.x264-UM.mkv".into(), mkv(2))],
        0.05,
        false,
    );
    e.search_results("The Matrix 1999", vec![matrix]);
    let (_, ep2) = e.publish(
        "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM",
        "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM",
        vec![("Slow.Horses.S01E02.1080p.WEB-DL.x264-UM.mkv".into(), mkv(2))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E02", vec![ep2]);

    // Everything imported is observed: known, with ownership evidence, but not acquired.
    let (_, o, text) = e.json(e.get("/api/v1/ownership")).await;
    assert_eq!(o["mode"], "coexistence");
    assert_eq!(
        (
            o["counts"]["observed"].as_u64(),
            o["counts"]["adopted"].as_u64()
        ),
        (Some(5), None),
        "{text}"
    );
    assert_eq!(o["items"][0]["acquisition"], "observed");
    let (_, w, _) = e.json(e.get("/api/v1/wanted")).await;
    assert_eq!(w["total"], 0, "nothing is wanted here: {w}");

    // RSS, the worker's searches and every direct execution path acquire nothing.
    // Build fixtures before locking: publish() takes this same mock mutex.
    let feed = feed_xml(&[
        feed_item(&e, "The.Matrix.1999.1080p.WEB-DL.x264-RSS", 0.05),
        feed_item(&e, "Slow.Horses.S01E02.1080p.WEB-DL.x264-RSS", 1.5),
    ]);
    e.mock.lock().unwrap().feeds.insert("1".into(), feed);
    let r = command(&e, json!({ "name": "rss_sync" })).await;
    assert_eq!(r["grabbed"], 0, "{r}");
    let r = command(&e, json!({ "name": "search_missing" })).await;
    assert_eq!(r["grabbed"].as_u64().unwrap_or(0), 0, "{r}");
    let r = command(
        &e,
        json!({ "name": "search", "media_id": "series:tvdb:372264" }),
    )
    .await;
    assert!(r.to_string().contains("nothing wanted"), "{r}");
    let (s, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("not_adopted")));
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("native_handoff_confirmed")
    );
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let plan_id = v["plan"]["id"].as_str().unwrap().to_string();
    let release_ref = v["plan"]["candidates"][0]["release_ref"].clone();
    let (s, v, _) = e
        .json(e.post(&format!("/api/v1/plans/{plan_id}/execute")))
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("not_adopted")));
    let (s, v, _) = e
        .json(
            e.post("/api/v1/grabs")
                .json(&json!({ "media_id": "movie:tmdb:603", "release_ref": release_ref })),
        )
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("not_adopted")));
    // Monitoring a native title here without the handoff is refused too.
    let (s, v, _) = e
        .json(
            e.patch("/api/v1/library/movie:tmdb:194")
                .json(&json!({ "monitored": true })),
        )
        .await;
    assert_eq!(s, 409, "{v}");
    e.tick().await;
    assert_eq!(adds(&e), 0);
    let (_, q, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"], json!([]));
    let (_, r, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(r["items"], json!([]));

    // Adoption needs the native handoff confirmed; then it is a normal one-call add.
    let (s, v, _) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "movie:tmdb:603" })),
        )
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("native_handoff_required"))
    );
    assert_eq!(
        e.state.index().movie(603).unwrap().acquisition.as_deref(),
        Some("observed"),
        "a refused adoption changes nothing"
    );
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "movie:tmdb:603", "native_handoff_confirmed": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["adoption"]["acquisition"], "adopted");
    assert_eq!(v["adoption"]["native_app"], "Radarr");
    assert_eq!(v["plan"]["state"], "download_queued", "{text}");
    e.tick().await;
    assert_eq!(adds(&e), 1);
    assert!(e.qb.lock().unwrap().torrents[&matrix_hash].category == "um-movies");
    let (_, o, _) = e.json(e.get("/api/v1/ownership?state=adopted")).await;
    assert_eq!(o["total"], 1);
    assert_eq!(o["items"][0]["media_id"], "movie:tmdb:603");
    assert_eq!(o["items"][0]["native"]["app"], "radarr");
}

#[tokio::test]
async fn adopting_one_episode_makes_exactly_that_scope_effective_through_reimport_and_refresh() {
    // Natively the series is unmonitored but its episode flags stay set (dormant).
    let mut snap = snapshot();
    snap["sonarr"]["series"][0]["monitored"] = json!(false);
    let mut e = start_exec(ExecOptions {
        snapshot: snap.clone(),
        ..coexistence()
    })
    .await;
    let (e4_hash, e4) = e.publish(
        "Slow.Horses.S01E04.1080p.WEB-DL.x264-UM",
        "Slow.Horses.S01E04.1080p.WEB-DL.x264-UM",
        vec![("Slow.Horses.S01E04.1080p.WEB-DL.x264-UM.mkv".into(), mkv(2))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E04", vec![e4]);
    let (s, v, text) = e
        .json(e.post("/api/v1/library").json(
            &json!({ "media_id": "episode:tvdb:372264:1:4", "native_handoff_confirmed": true }),
        ))
        .await;
    assert_eq!(s, 201, "{text}");
    let units: Vec<&str> = v["plan"]["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["target"].as_str().unwrap())
        .collect();
    assert_eq!(units, ["episode:tvdb:372264:1:4"], "{text}");
    assert_eq!(
        monitored(&e, 372264),
        [(1, 4)],
        "dormant native flags cleared"
    );
    e.tick().await;
    assert_eq!(adds(&e), 1);
    assert!(e.qb.lock().unwrap().torrents.contains_key(&e4_hash));
    let (_, o, text) = e.json(e.get("/api/v1/ownership?state=adopted")).await;
    let scope = &o["items"][0]["scope"][0];
    assert_eq!(
        (
            scope["season"].as_u64(),
            scope["season_monitored"].as_bool(),
            scope["monitored_episodes"].clone(),
            scope["native_monitored_episodes"].as_u64()
        ),
        (Some(1), Some(false), json!([4]), Some(6)),
        "{text}"
    );
    assert_eq!(o["items"][0]["monitor_new_seasons"], false);

    // RSS: the dormant natively monitored episode, an adjacent one, the season pack and a
    // later season are not acquired; neither are the worker's searches.
    // Build fixtures before locking: publish() takes this same mock mutex.
    let feed = feed_xml(&[
        feed_item(&e, "Slow.Horses.S01E02.1080p.WEB-DL.x264-RSS", 1.5),
        feed_item(&e, "Slow.Horses.S01E05.1080p.WEB-DL.x264-RSS", 1.5),
        feed_item(&e, "Slow.Horses.S01.1080p.WEB-DL.x264-RSS", 6.0),
        feed_item(&e, "Slow.Horses.S02E01.1080p.WEB-DL.x264-RSS", 1.5),
    ]);
    e.mock.lock().unwrap().feeds.insert("1".into(), feed);
    let r = command(&e, json!({ "name": "rss_sync" })).await;
    assert_eq!(r["grabbed"], 0, "{r}");
    command(&e, json!({ "name": "search_missing" })).await;
    command(
        &e,
        json!({ "name": "search", "media_id": "series:tvdb:372264" }),
    )
    .await;
    e.tick().await;
    assert_eq!(adds(&e), 1, "still only E04");

    // A re-import brings new natively monitored episodes and a new season: not adopted here.
    let eps = snap["sonarr"]["episode_samples"]["20"]
        .as_array_mut()
        .unwrap();
    eps.push(json!({ "seasonNumber": 1, "episodeNumber": 7, "title": "Episode 7", "airDateUtc": "2022-05-06T02:00:00Z", "runtime": 50, "hasFile": false, "monitored": true }));
    eps.push(json!({ "seasonNumber": 3, "episodeNumber": 1, "title": "Episode 1", "airDateUtc": "2024-01-01T02:00:00Z", "runtime": 50, "hasFile": false, "monitored": true }));
    snap["sonarr"]["series"][0]["seasons"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "seasonNumber": 3, "monitored": true, "statistics": { "episodeFileCount": 0, "episodeCount": 1, "totalEpisodeCount": 1 } }));
    reimport(&e, &snap);
    e.restart().await;
    assert_eq!(monitored(&e, 372264), [(1, 4)], "re-import does not widen");
    let (_, o, _) = e.json(e.get("/api/v1/ownership?state=adopted")).await;
    assert_eq!(o["items"][0]["scope"][0]["native_monitored_episodes"], 7);

    // A metadata refresh appends the next episode, unmonitored with its season.
    e.mock.lock().unwrap().tvmaze_extra.insert(
        "9001".into(),
        vec![json!({ "season": 1, "number": 8, "name": "Ep 8", "type": "regular", "airstamp": "2025-02-20T02:00:00+00:00", "runtime": 60 })],
    );
    command(
        &e,
        json!({ "name": "refresh_metadata", "media_id": "series:tvdb:372264" }),
    )
    .await;
    let index = e.state.index();
    let s = index.series(arr::ids::SeriesKey::Tvdb(372264)).unwrap();
    assert!(
        s.episodes
            .iter()
            .flatten()
            .any(|x| x.season == 1 && x.number == 8)
    );
    drop(index);
    assert_eq!(monitored(&e, 372264), [(1, 4)], "refresh does not widen");
    let (_, w, text) = e.json(e.get("/api/v1/wanted")).await;
    assert_eq!(w["total"], 0, "only E04, which is downloading: {text}");
}

#[tokio::test]
async fn coexistence_refuses_a_qbittorrent_category_any_native_client_uses() {
    // Default categories are the native ones.
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        ..Default::default()
    })
    .await;
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2", "execute": true })),
        )
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("category_shared")),
        "{text}"
    );
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("'tv-sonarr'") && msg.contains("Sonarr") && msg.contains("tv_category"),
        "{msg}"
    );
    assert_no_leaks(&text);
    let (_, r, _) = e.json(e.get("/api/v1/readiness")).await;
    let coex = &r["migration"]["stages"][1];
    assert_eq!(coex["status"], "blocked");
    assert!(
        coex["configuration_blockers"]
            .to_string()
            .contains("category_shared")
    );

    // A dedicated movie category, but the TV category is the native *Radarr* one: refused.
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        categories: Some(("um-movies", "radarr")),
        ..Default::default()
    })
    .await;
    let (s, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2", "execute": true })),
        )
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("category_shared"))
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("native Radarr")
    );
    // Movies (dedicated category) are not blocked by the category check.
    let (_, r, _) = e.json(e.get("/api/v1/readiness")).await;
    let checks = r["checks"].as_array().unwrap();
    let state = |name: &str| {
        checks
            .iter()
            .find(|c| c["name"] == name)
            .map(|c| c["status"].as_str().unwrap().to_string())
    };
    assert_eq!(state("execution_radarr").as_deref(), Some("ok"));
    assert_eq!(state("execution_sonarr").as_deref(), Some("blocked"));
}

#[tokio::test]
async fn a_native_file_appearing_during_the_download_is_never_duplicated() {
    let e = start_exec(coexistence()).await;
    let (hash, r) = e.publish(
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        vec![("The.Matrix.1999.1080p.WEB-DL.x264-UM.mkv".into(), mkv(2))],
        0.05,
        false,
    );
    e.search_results("The Matrix 1999", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "movie:tmdb:603", "native_handoff_confirmed": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    // Meanwhile the native app imports its own copy under another name.
    let folder = e.movies.join("The Matrix (1999)");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("The.Matrix.1999.1080p.BluRay.x264-ARR.mkv"),
        mkv(3),
    )
    .unwrap();
    qb::complete(&e.qb, &hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "import_blocked");
    assert!(
        g.last_error
            .as_deref()
            .unwrap()
            .contains("already has a file"),
        "{:?}",
        g.last_error
    );
    let files: Vec<_> = std::fs::read_dir(&folder)
        .unwrap()
        .map(|f| f.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(files, ["The.Matrix.1999.1080p.BluRay.x264-ARR.mkv"]);
    // Status and attention say so.
    let (_, st, text) = e.json(e.get("/api/v1/status/movie:tmdb:603")).await;
    assert_eq!(st["state"], "owned", "{text}");
    assert_eq!(st["attention"][0]["kind"], "import_blocked", "{text}");
    let (_, a, _) = e.json(e.get("/api/v1/attention")).await;
    assert!(
        a["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["grab_id"] == id.as_str() && i["kind"] == "import_blocked")
    );
}

#[tokio::test]
async fn readiness_follows_configuration_and_recorded_evidence_only() {
    let mut e = start_exec(ExecOptions {
        tmdb: true,
        ..coexistence()
    })
    .await;
    let stages = |r: &Value| -> Vec<(String, String)> {
        r["migration"]["stages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["stage"].as_str().unwrap().to_string(),
                    s["status"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let (_, r, text) = e.json(e.get("/api/v1/readiness")).await;
    assert_eq!(r["ready"], true, "service readiness");
    assert_eq!(
        stages(&r),
        [
            ("limited".to_string(), "pending_verification".to_string()),
            ("coexistence".into(), "pending_verification".into()),
            ("broad".into(), "blocked".into()),
        ],
        "{text}"
    );
    assert!(
        r["migration"]["stages"][0]["pending_verification"]
            .to_string()
            .contains("download_cleanup: verified at loopback_mocks")
    );
    assert!(
        r["migration"]["stages"][2]["configuration_blockers"]
            .to_string()
            .contains("nothing is adopted")
    );
    // Adopt one title, then record production evidence for exactly the capabilities in use.
    let (s, _, text) = e
        .json(e.post("/api/v1/library").json(
            &json!({ "media_id": "movie:tmdb:603", "native_handoff_confirmed": true, "search": false }),
        ))
        .await;
    assert_eq!(s, 201, "{text}");
    let (_, caps, _) = e.json(e.get("/api/v1/capabilities")).await;
    let in_use: Vec<String> = caps["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["implemented"] == true && c["configured"] == true)
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert!(in_use.contains(&"coexistence_guard".to_string()));
    assert!(
        !in_use.contains(&"quality_upgrades".to_string()),
        "unused, not required"
    );
    let dir = e.dir.clone();
    let record = |decision: bool| {
        let mut t = String::from("[verified]\n");
        for c in &in_use {
            t.push_str(&format!("{c} = \"production\"\n"));
        }
        if decision {
            t.push_str("[decisions]\nbroad_migration = \"approved\"\n");
        }
        let p = dir.join("verification.toml");
        let _ = std::fs::remove_file(&p);
        fsutil::write_new_private_file(&p, t.as_bytes()).unwrap();
    };
    record(false);
    e.restart().await;
    let (_, r, text) = e.json(e.get("/api/v1/readiness")).await;
    assert_eq!(
        stages(&r),
        [
            ("limited".to_string(), "prerequisites_met".to_string()),
            ("coexistence".into(), "prerequisites_met".into()),
            ("broad".into(), "pending_verification".into()),
        ],
        "{text}"
    );
    assert!(
        r["migration"]["stages"][2]["pending_verification"]
            .to_string()
            .contains("no explicit user decision")
    );
    record(true);
    e.restart().await;
    let (_, r, text) = e.json(e.get("/api/v1/readiness")).await;
    let broad = &r["migration"]["stages"][2];
    assert_eq!(broad["ready"], true, "{text}");
    // Even then the native side is stated as outside this server's knowledge.
    assert!(
        !broad["external_requirements"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        r["migration"]["note"]
            .as_str()
            .unwrap()
            .contains("not observable")
    );
    assert!(
        r["migration"]["evidence"]["source"]
            .as_str()
            .unwrap()
            .contains("not a live health check")
    );
}

#[tokio::test]
async fn live_status_reports_season_completeness_and_follows_imports() {
    let e = start_exec(coexistence()).await;
    let title = "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM";
    let (hash, r) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E02", vec![r]);
    let (s, _, text) = e
        .json(e.post("/api/v1/library").json(
            &json!({ "media_id": "episode:tvdb:372264:1:2", "native_handoff_confirmed": true }),
        ))
        .await;
    assert_eq!(s, 201, "{text}");
    e.tick().await;
    let (_, st, text) = e.json(e.get("/api/v1/status/season:tvdb:372264:1")).await;
    let season = &st["seasons"][0];
    assert_eq!(
        (
            season["source"].as_str(),
            season["episode_file_count"].as_u64(),
            season["total_episode_count"].as_u64(),
            season["queued"].as_u64(),
            season["missing"].as_u64(),
            season["complete_aired"].as_bool()
        ),
        (
            Some("live"),
            Some(2),
            Some(6),
            Some(1),
            Some(3),
            Some(false)
        ),
        "{text}"
    );
    let ep = |v: &Value, n: u64| {
        v["episodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["episode"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(ep(&st, 1)["state"], "owned");
    assert_eq!(ep(&st, 2)["state"], "downloading");
    assert!(ep(&st, 2)["download"]["id"].is_string());
    assert_eq!(ep(&st, 4)["state"], "missing");
    assert_eq!(ep(&st, 2)["monitored"], true);
    assert_eq!(ep(&st, 4)["monitored"], false);
    qb::complete(&e.qb, &hash);
    e.tick().await;
    let (_, st, text) = e.json(e.get("/api/v1/status/season:tvdb:372264:1")).await;
    assert_eq!(ep(&st, 2)["state"], "owned", "{text}");
    assert_eq!(st["seasons"][0]["episode_file_count"], 3);
    // The library listing shows the live count, not the snapshot's (2).
    let (_, l, _) = e
        .json(e.get("/api/v1/library/series?q=Slow%20Horses"))
        .await;
    let s1 = &l["items"][0]["seasons"][0];
    assert_eq!(
        (s1["source"].as_str(), s1["episode_file_count"].as_u64()),
        (Some("live"), Some(3))
    );
    let (_, ep_status, _) = e
        .json(e.get("/api/v1/status/episode:tvdb:372264:1:2"))
        .await;
    assert_eq!(ep_status["episodes"].as_array().unwrap().len(), 1);
    assert_eq!(ep_status["downloads"][0]["state"], "imported");
}

#[tokio::test]
async fn native_monitor_new_items_and_numbering_are_kept() {
    let mut snap = snapshot();
    snap["sonarr"]["series"][0]["monitorNewItems"] = json!("none");
    let mut e = start_exec(ExecOptions {
        snapshot: snap.clone(),
        ..coexistence()
    })
    .await;
    let (_, o, text) = e.json(e.get("/api/v1/ownership?state=observed")).await;
    let sh = o["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["media_id"] == "series:tvdb:372264")
        .unwrap()
        .clone();
    assert_eq!(sh["monitor_new_seasons"], false, "native none: {text}");
    // Adopting the whole series is an explicit choice to monitor new seasons; a re-import
    // with the native "none" does not undo it.
    let (s, _, text) = e
        .json(e.post("/api/v1/library").json(
            &json!({ "media_id": "series:tvdb:372264", "native_handoff_confirmed": true, "search": false }),
        ))
        .await;
    assert_eq!(s, 201, "{text}");
    reimport(&e, &snap);
    e.restart().await;
    let (_, o, _) = e.json(e.get("/api/v1/ownership?state=adopted")).await;
    assert_eq!(o["items"][0]["monitor_new_seasons"], true);

    // TVmaze has a gap inside the imported season, the next season (declared natively, no
    // episodes yet), a season after a gap, and different air dates.
    e.mock.lock().unwrap().tvmaze_extra.insert(
        "9001".into(),
        vec![
            json!({ "season": 1, "number": 9, "name": "Ep 9", "type": "regular", "airstamp": "2025-03-01T02:00:00+00:00", "runtime": 60 }),
            json!({ "season": 2, "number": 1, "name": "Ep 1", "type": "regular", "airstamp": "2025-09-01T02:00:00+00:00", "runtime": 60 }),
            json!({ "season": 4, "number": 1, "name": "Ep 1", "type": "regular", "airstamp": "2027-09-01T02:00:00+00:00", "runtime": 60 }),
        ],
    );
    let r = command(
        &e,
        json!({ "name": "refresh_metadata", "media_id": "series:tvdb:372264" }),
    )
    .await;
    let result = r["series"][0].clone();
    // The conflicts (a gap in S1, a season after a gap) also hold the next season back.
    assert_eq!(
        (
            result["added"].as_u64(),
            result["skipped"].as_u64(),
            result["held_back"].as_u64(),
            result["hold"].as_str()
        ),
        (Some(0), Some(2), Some(1), Some("set")),
        "{r}"
    );
    let divergence = result["divergence"].to_string();
    assert!(divergence.contains("S04E01"), "a season after a gap: {r}");
    assert!(
        divergence.contains("S01E09") && divergence.contains("not added"),
        "{r}"
    );
    assert!(
        divergence.contains("S01E01") && divergence.contains("kept"),
        "{r}"
    );
    let find = |e: &Exec, season: u16, n: u16| {
        e.state
            .index()
            .series(arr::ids::SeriesKey::Tvdb(372264))
            .unwrap()
            .episodes
            .iter()
            .flatten()
            .find(|x| x.season == season && x.number == n)
            .cloned()
    };
    assert!(
        find(&e, 1, 9).is_none() && find(&e, 4, 1).is_none() && find(&e, 2, 1).is_none(),
        "nothing appended while numbering conflicts"
    );
    assert_eq!(
        find(&e, 1, 1).unwrap().air_date_utc.as_deref(),
        Some("2022-04-01T02:00:00Z"),
        "imported air date kept"
    );
    // The provider's list without the conflicts: the next season is appended, by choice.
    e.mock.lock().unwrap().tvmaze_extra.insert(
        "9001".into(),
        vec![json!({ "season": 2, "number": 1, "name": "Ep 1", "type": "regular", "airstamp": "2025-09-01T02:00:00+00:00", "runtime": 60 })],
    );
    let r = command(
        &e,
        json!({ "name": "refresh_metadata", "media_id": "series:tvdb:372264" }),
    )
    .await;
    assert_eq!(
        (
            r["series"][0]["added"].as_u64(),
            r["series"][0]["hold"].as_str()
        ),
        (Some(1), Some("cleared")),
        "{r}"
    );
    assert!(
        find(&e, 2, 1).unwrap().monitored,
        "the next season, monitored by choice"
    );
}

#[tokio::test]
async fn a_legacy_seeded_state_switched_to_coexistence_refuses_unadopted_items_and_says_so() {
    // An existing deployment seeded every imported item as managed (legacy).
    let mut e = start_exec(ExecOptions {
        mode: "legacy",
        categories: CATS,
        ..Default::default()
    })
    .await;
    let cfg = e.dir.join("config.toml");
    let text = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("mode = 'legacy'", "mode = 'coexistence'");
    std::fs::remove_file(&cfg).unwrap();
    fsutil::write_new_private_file(&cfg, text.as_bytes()).unwrap();
    e.restart().await;
    // Its managed state is kept, but nothing it never adopted is acquired.
    assert_eq!(
        e.state.index().movie(603).unwrap().acquisition.as_deref(),
        Some("legacy_managed")
    );
    let (s, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("not_adopted")));
    let (_, r, text) = e.json(e.get("/api/v1/readiness")).await;
    let coex = &r["migration"]["stages"][1];
    assert_eq!(coex["status"], "blocked", "{text}");
    assert!(
        coex["configuration_blockers"]
            .to_string()
            .contains("legacy import")
    );
    assert_eq!(r["migration"]["ownership"]["legacy_managed"], 5);
}

/// The imported scope of one ownership row: (monitored, monitor_new_seasons, per-season
/// (season, season_monitored, monitored_episodes)).
fn scope_of(o: &Value, media_id: &str) -> Value {
    let row = o["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["media_id"] == media_id)
        .unwrap_or_else(|| panic!("{media_id}: {o}"))
        .clone();
    json!({
        "monitored": row["monitored"],
        "monitor_new_seasons": row["monitor_new_seasons"],
        "scope": row["scope"].as_array().map(|seasons| seasons.iter().map(|x| json!([x["season"], x["season_monitored"], x["monitored_episodes"]])).collect::<Vec<_>>()),
    })
}

async fn refresh_slow_horses(e: &Exec) -> Value {
    command(
        e,
        json!({ "name": "refresh_metadata", "media_id": "series:tvdb:372264" }),
    )
    .await["series"][0]
        .clone()
}

async fn adopted_scope(e: &Exec, media_id: &str) -> Value {
    let (_, o, _) = e.json(e.get("/api/v1/ownership?state=adopted")).await;
    scope_of(&o, media_id)
}

fn searches(e: &Exec) -> usize {
    e.mock
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|q| q.contains("type=search"))
        .count()
}

#[tokio::test]
async fn preserve_monitoring_adopts_an_imported_title_exactly_as_imported() {
    // A partial native setup: the series itself unmonitored, season 1 monitored with only
    // E1/E3/E5 flagged, season 2 unmonitored, new seasons not monitored.
    let mut snap = snapshot();
    snap["sonarr"]["series"][0]["monitored"] = json!(false);
    snap["sonarr"]["series"][0]["monitorNewItems"] = json!("none");
    snap["sonarr"]["series"][0]["seasons"][1]["monitored"] = json!(false);
    for e in snap["sonarr"]["episode_samples"]["20"]
        .as_array_mut()
        .unwrap()
    {
        let n = e["episodeNumber"].as_u64().unwrap();
        e["monitored"] = json!(n % 2 == 1);
    }
    let mut e = start_exec(ExecOptions {
        snapshot: snap.clone(),
        ..coexistence()
    })
    .await;
    let sh = "series:tvdb:372264";
    let (_, o, _) = e.json(e.get("/api/v1/ownership?state=observed")).await;
    let imported = scope_of(&o, sh);
    assert_eq!(
        imported,
        json!({ "monitored": false, "monitor_new_seasons": false, "scope": [[1, true, [1, 3, 5]], [2, false, []]] }),
        "{o}"
    );
    let base = json!({ "media_id": sh, "preserve_monitoring": true, "search": false, "native_handoff_confirmed": true });
    let with = |extra: Value| {
        let mut b = base.clone();
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    };
    // Every misuse is refused before anything changes.
    for (body, status) in [
        (with(json!({ "monitor": "all" })), 400),
        (with(json!({ "monitored": true })), 400),
        (with(json!({ "media_id": "season:tvdb:372264:1" })), 400),
        (with(json!({ "search": true })), 400),
        (with(json!({ "native_handoff_confirmed": false })), 409),
    ] {
        let (s, v, _) = e.json(e.post("/api/v1/library").json(&body)).await;
        assert_eq!(s, status, "{body}: {v}");
    }
    let (_, o, _) = e.json(e.get("/api/v1/ownership?state=observed")).await;
    assert_eq!(scope_of(&o, sh), imported, "refusals changed nothing");
    // Until adopted, an observed title follows the latest import's native intent.
    snap["sonarr"]["episode_samples"]["20"][4]["monitored"] = json!(false);
    reimport(&e, &snap);
    e.restart().await;
    let (_, o, _) = e.json(e.get("/api/v1/ownership?state=observed")).await;
    let imported = scope_of(&o, sh);
    assert_eq!(imported["scope"][0], json!([1, true, [1, 3]]), "{o}");

    // Adoption: managed and adopted, every flag identical, nothing searched or grabbed.
    let before = searches(&e);
    let (s, v, text) = e.json(e.post("/api/v1/library").json(&base)).await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(
        (v["result"].as_str(), v["whole_title"].as_bool()),
        (Some("adopted"), Some(true)),
        "{text}"
    );
    assert!(
        v["adoption"]["note"]
            .as_str()
            .unwrap()
            .contains("not a fresh native query")
    );

    assert_eq!(adopted_scope(&e, sh).await, imported);
    // The series is unmonitored here as it was natively: nothing is wanted or acquired.
    let (_, w, _) = e.json(e.get("/api/v1/wanted")).await;
    assert_eq!(w["total"], 0, "{w}");
    // A whole movie, too.
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&with(json!({ "media_id": "movie:tmdb:603" }))),
        )
        .await;
    assert_eq!((s, v["result"].as_str()), (201, Some("adopted")), "{text}");
    assert!(e.state.index().movie(603).unwrap().monitored);
    e.tick().await;
    assert_eq!(searches(&e), before, "no search was started");
    assert_eq!(adds(&e), 0);

    // A replay changes nothing; a later user choice survives replays and re-imports.
    let (_, v, _) = e.json(e.post("/api/v1/library").json(&base)).await;
    assert_eq!(v["result"], "already_adopted");
    assert_eq!(adopted_scope(&e, sh).await, imported);
    let (s, _, text) = e
        .json(
            e.patch("/api/v1/library/episode:tvdb:372264:1:2")
                .json(&json!({ "monitored": true, "native_handoff_confirmed": true })),
        )
        .await;
    assert_eq!(s, 200, "{text}");
    let chosen = adopted_scope(&e, sh).await;
    assert_eq!(chosen["scope"][0], json!([1, true, [1, 2, 3]]));
    let (_, v, _) = e.json(e.post("/api/v1/library").json(&base)).await;
    assert_eq!(v["result"], "already_adopted");
    reimport(&e, &snap);
    e.restart().await;
    assert_eq!(
        adopted_scope(&e, sh).await,
        chosen,
        "never reset from the snapshot"
    );

    // An API-added title is not a transfer.
    let (s, _, text) =
        e.json(e.post("/api/v1/library").json(
            &json!({ "media_id": "series:tvdb:999005", "search": false, "monitor": "none" }),
        ))
        .await;
    assert_eq!(s, 201, "{text}");
    let (s, v, _) = e
        .json(
            e.post("/api/v1/library")
                .json(&with(json!({ "media_id": "series:tvdb:999005" }))),
        )
        .await;
    assert_eq!(s, 400, "{v}");
}

#[tokio::test]
async fn preserve_monitoring_reports_native_specials_and_refuses_unsupported_titles() {
    let mut snap = snapshot();
    snap["sonarr"]["episode_samples"]["20"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "seasonNumber": 0, "episodeNumber": 1, "title": "Special", "airDateUtc": "2022-05-01T02:00:00Z", "runtime": 20, "hasFile": false, "monitored": true }));
    snap["sonarr"]["series"][1]["seriesType"] = json!("anime");
    let e = start_exec(ExecOptions {
        snapshot: snap,
        ..coexistence()
    })
    .await;
    let body = |id: &str| json!({ "media_id": id, "preserve_monitoring": true, "search": false, "native_handoff_confirmed": true });
    let (s, v, text) = e
        .json(e.post("/api/v1/library").json(&body("series:tvdb:372264")))
        .await;
    assert_eq!(s, 201, "{text}");
    // Not a whole-title takeover: the monitored special stays with Sonarr, and says so.
    assert_eq!(v["whole_title"], false, "{text}");
    assert_eq!(
        (
            v["native_retained"][0]["season"].as_u64(),
            v["native_retained"][0]["monitored_episodes"].as_u64()
        ),
        (Some(0), Some(1)),
        "{text}"
    );
    let (_, o, _) = e.json(e.get("/api/v1/ownership?state=adopted")).await;
    assert_eq!(o["items"][0]["native_specials_monitored"], 1);
    let (_, r, _) = e.json(e.get("/api/v1/readiness")).await;
    assert!(
        r["migration"]["stages"][1]["external_requirements"]
            .to_string()
            .contains("natively monitored specials")
    );
    // An anime series cannot be acquired here: refused for every kind of adoption.
    for b in [
        body("series:tvdb:269613"),
        json!({ "media_id": "series:tvdb:269613", "native_handoff_confirmed": true, "search": false }),
    ] {
        let (s, v, _) = e.json(e.post("/api/v1/library").json(&b)).await;
        assert_eq!(
            (s, v["error"]["code"].as_str()),
            (409, Some("unsupported_scope")),
            "{v}"
        );
    }
    assert_eq!(
        e.state
            .index()
            .series(arr::ids::SeriesKey::Tvdb(269613))
            .unwrap()
            .acquisition
            .as_deref(),
        Some("observed")
    );
    // Legacy mode has nothing to transfer.
    let e = start_exec(ExecOptions {
        mode: "legacy",
        categories: CATS,
        ..Default::default()
    })
    .await;
    let (s, _, _) = e
        .json(e.post("/api/v1/library").json(&body("series:tvdb:372264")))
        .await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn a_metadata_numbering_conflict_is_held_visibly_and_durably_until_a_clean_refresh() {
    let mut e = start_exec(coexistence()).await;
    let (s, _, text) = e
        .json(e.post("/api/v1/library").json(
            &json!({ "media_id": "series:tvdb:372264", "native_handoff_confirmed": true, "search": false }),
        ))
        .await;
    assert_eq!(s, 201, "{text}");
    let has = |e: &Exec, season: u16, n: u16| {
        e.state
            .index()
            .series(arr::ids::SeriesKey::Tvdb(372264))
            .unwrap()
            .episodes
            .iter()
            .flatten()
            .any(|x| x.season == season && x.number == n)
    };
    let holds = |a: &Value| -> Vec<Value> {
        a["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["kind"] == "metadata_hold")
            .cloned()
            .collect()
    };
    // A gap in the imported season plus a genuine-looking future season.
    let conflicting = vec![
        json!({ "season": 1, "number": 9, "name": "Ep 9", "type": "regular", "airstamp": "2025-03-01T02:00:00+00:00", "runtime": 60 }),
        json!({ "season": 2, "number": 1, "name": "A real new season", "type": "regular", "airstamp": "2027-01-01T02:00:00+00:00", "runtime": 60 }),
    ];
    e.mock
        .lock()
        .unwrap()
        .tvmaze_extra
        .insert("9001".into(), conflicting.clone());
    let r = refresh_slow_horses(&e).await;
    assert_eq!(
        (
            r["added"].as_u64(),
            r["held_back"].as_u64(),
            r["hold"].as_str()
        ),
        (Some(0), Some(1), Some("set")),
        "{r}"
    );
    assert!(!has(&e, 1, 9) && !has(&e, 2, 1), "no phantom targets");
    // Visible in attention and in scope status, with the reasons.
    let (_, a, text) = e.json(e.get("/api/v1/attention")).await;
    let h = holds(&a);
    assert_eq!(h.len(), 1, "{text}");
    assert_eq!(h[0]["media_id"], "series:tvdb:372264");
    assert!(h[0]["reasons"].to_string().contains("S01E09"), "{text}");
    let (_, st, text) = e.json(e.get("/api/v1/status/season:tvdb:372264:1")).await;
    assert!(st["metadata_hold"].is_object(), "{text}");
    assert!(
        st["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "metadata_hold"),
        "{text}"
    );
    // Durable across a restart; a refresh with the same conflict keeps it.
    e.restart().await;
    let (_, a, _) = e.json(e.get("/api/v1/attention")).await;
    assert_eq!(holds(&a).len(), 1, "the hold survives a restart");
    let r = refresh_slow_horses(&e).await;
    assert_eq!(r["hold"], "kept", "{r}");
    assert!(!has(&e, 2, 1));
    // The provider's list no longer conflicts: the new season is appended, the hold clears.
    e.mock
        .lock()
        .unwrap()
        .tvmaze_extra
        .insert("9001".into(), vec![conflicting[1].clone()]);
    let r = refresh_slow_horses(&e).await;
    assert_eq!(
        (r["added"].as_u64(), r["hold"].as_str()),
        (Some(1), Some("cleared")),
        "{r}"
    );
    assert!(has(&e, 2, 1));
    let (_, a, _) = e.json(e.get("/api/v1/attention")).await;
    assert!(holds(&a).is_empty());
    // An ordinary valid extension raises nothing.
    let mut extended = vec![conflicting[1].clone()];
    extended.push(json!({ "season": 2, "number": 2, "name": "And the next one", "type": "regular", "airstamp": "2027-01-08T02:00:00+00:00", "runtime": 60 }));
    e.mock
        .lock()
        .unwrap()
        .tvmaze_extra
        .insert("9001".into(), extended);
    let r = refresh_slow_horses(&e).await;
    assert_eq!(
        (
            r["added"].as_u64(),
            r["hold"].as_str(),
            r["skipped"].as_u64()
        ),
        (Some(1), Some("none"), Some(0)),
        "{r}"
    );
    let (_, a, _) = e.json(e.get("/api/v1/attention")).await;
    assert!(holds(&a).is_empty());
}

/// Loopback twin of the opt-in real-client scenario in `tests/sandbox.rs`: an observed title
/// is ignored by the scheduled RSS pass, then picked up by the next scheduled pass after an
/// adoption without search; an unadopted decoy in the same feed is never fetched; replays
/// and a restart create no duplicate.
#[tokio::test]
async fn scheduled_rss_picks_up_only_an_adopted_title_and_never_twice() {
    let mut e = start_exec(coexistence()).await;
    let target = feed_item(&e, "The.Matrix.1999.1080p.WEB-DL.x264-COEX", 0.05);
    let decoy = feed_item(&e, "Slow.Horses.S01E02.1080p.WEB-DL.x264-DECOY", 1.5);
    let (th, dh) = (
        target.info_hash.clone().unwrap(),
        decoy.info_hash.clone().unwrap(),
    );
    let feed = feed_xml(&[target.clone(), decoy.clone()]);
    e.mock.lock().unwrap().feeds.insert("1".into(), feed);
    let status = |v: &Value, title: &str| {
        v["recent"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"] == title)
            .map(|i| i["status"].as_str().unwrap_or("").to_string())
            .unwrap_or_else(|| panic!("{title}: {v}"))
    };
    let t0 = time::OffsetDateTime::now_utc();
    e.tick_at(t0).await;
    let (_, rss, _) = e.json(e.get("/api/v1/rss")).await;
    assert_eq!(status(&rss, &target.title), "ignored");
    assert_eq!(status(&rss, &decoy.title), "ignored");
    let (s, v, text) = e
        .json(e.post("/api/v1/library").json(
            &json!({ "media_id": "movie:tmdb:603", "native_handoff_confirmed": true, "search": false }),
        ))
        .await;
    assert_eq!(s, 201, "{text}");
    assert!(v.get("plan").is_none());
    let t1 = t0 + std::time::Duration::from_secs(31 * 60);
    e.tick_at(t1).await;
    let (_, rss, _) = e.json(e.get("/api/v1/rss")).await;
    assert_eq!(status(&rss, &target.title), "grabbed", "{rss}");
    assert_eq!(status(&rss, &decoy.title), "ignored");
    e.tick_at(t1).await;
    assert!(e.qb.lock().unwrap().torrents.contains_key(&th));
    qb::complete(&e.qb, &th);
    e.tick_at(t1).await;
    assert!(e.state.index().movie(603).unwrap().has_file);
    // Replays and a restart.
    e.tick_at(t1 + std::time::Duration::from_secs(31 * 60))
        .await;
    e.json(e.post("/api/v1/library").json(
        &json!({ "media_id": "movie:tmdb:603", "native_handoff_confirmed": true, "search": false }),
    ))
    .await;
    e.restart().await;
    e.tick_at(t1 + std::time::Duration::from_secs(62 * 60))
        .await;
    let (_, q, text) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"].as_array().unwrap().len(), 1, "{text}");
    assert_eq!(adds(&e), 1);
    let m = e.mock.lock().unwrap();
    assert_eq!(m.downloads, vec![th.clone()], "only the target was fetched");
    assert!(!m.downloads.contains(&dh));
    assert!(m.log.iter().all(|q| !q.contains("type=search")));
}

#[tokio::test]
async fn a_release_sent_during_an_add_is_ordered_after_it_and_refused_by_its_download() {
    let e = start_exec(coexistence()).await;
    let title = "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM";
    let (_, ep2) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E02", vec![ep2]);
    let (s, _, text) = e
        .json(e.post("/api/v1/library").json(
            &json!({ "media_id": "episode:tvdb:372264:1:2", "native_handoff_confirmed": true, "search": false }),
        ))
        .await;
    assert_eq!(s, 201, "{text}");
    // A request inside the confirmed scope, held in its automatic search by a slow indexer.
    e.mock.lock().unwrap().slow_queries.push((
        "Slow Horses S01E02".into(),
        std::time::Duration::from_millis(1500),
    ));
    let add = tokio::spawn(
        e.post("/api/v1/library")
            .json(&json!({ "media_id": "episode:tvdb:372264:1:2" }))
            .send(),
    );
    let in_search = || {
        e.mock
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|q| q.contains("query=Slow+Horses+S01E02"))
    };
    for _ in 0..1000 {
        if in_search() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(in_search(), "the add reached its search");

    // The release cannot fall between the add's handoff check and its changes: it runs
    // after the add, whose download then refuses it.
    let (s, v, text) = e.json(e.delete("/api/v1/library/series:tvdb:372264")).await;
    let added = add.await.unwrap().unwrap();
    assert_eq!(added.status().as_u16(), 201);
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("active_downloads")),
        "{text}"
    );
    let (_, q, _) = e.json(e.get("/api/v1/queue")).await;
    assert_eq!(q["items"].as_array().map(Vec::len), Some(1), "{q}");
    let (_, o, _) = e.json(e.get("/api/v1/ownership?state=adopted")).await;
    let item = o["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["media_id"] == "series:tvdb:372264")
        .unwrap_or_else(|| panic!("still adopted: {o}"));
    assert_eq!(item["handoffs"][0]["scope"], "episode:tvdb:372264:1:2");
}

#[tokio::test]
async fn a_release_committed_while_a_torrent_is_fetched_records_and_submits_nothing() {
    let e = start_exec(coexistence()).await;
    let title = "The.Matrix.1999.1080p.WEB-DL.x264-UM";
    let (hash, rel) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        0.05,
        false,
    );
    e.search_results("The Matrix 1999", vec![rel]);
    let adopt =
        json!({ "media_id": "movie:tmdb:603", "native_handoff_confirmed": true, "search": false });
    let (s, _, text) = e.json(e.post("/api/v1/library").json(&adopt)).await;
    assert_eq!(s, 201, "{text}");
    let (_, v, text) = e.json(e.get("/api/v1/media/movie:tmdb:603/releases")).await;
    let release_ref = v["releases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["decision"]["approved"] == true)
        .unwrap_or_else(|| panic!("an approved release: {text}"))["release_ref"]
        .clone();
    let grab = json!({ "media_id": "movie:tmdb:603", "release_ref": release_ref });

    // The grab passes its adoption check, then its torrent fetch is held open.
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    e.mock.lock().unwrap().download_gate = Some(gate.clone());
    let pending = tokio::spawn(e.post("/api/v1/grabs").json(&grab).send());
    let fetching = || e.mock.lock().unwrap().downloads.contains(&hash);
    for _ in 0..1000 {
        if fetching() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(fetching(), "the torrent fetch started");
    // Nothing is recorded yet, so the release succeeds while the fetch is pending.
    let (s, _, text) = e.json(e.delete("/api/v1/library/movie:tmdb:603")).await;
    assert_eq!(s, 200, "{text}");
    gate.notify_one();
    let r = pending.await.unwrap().unwrap();
    let status = r.status().as_u16();
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (409, Some("not_adopted")),
        "{v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no longer adopted"),
        "{v}"
    );
    // No grab, outbox entry or client add, then or after a worker pass.
    e.tick().await;
    assert_eq!(adds(&e), 0);
    let (_, q, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"], json!([]), "{q}");
    let (_, o, _) = e.json(e.get("/api/v1/ownership")).await;
    let item = o["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["media_id"] == "movie:tmdb:603")
        .unwrap_or_else(|| panic!("{o}"));
    assert_eq!(item["acquisition"], "released", "{item}");
    assert_eq!(item["handoffs"], json!([]), "{item}");

    // A fresh handoff re-adopts the movie, and the same release is grabbed once.
    e.mock.lock().unwrap().download_gate = None;
    let (s, _, text) = e.json(e.post("/api/v1/library").json(&adopt)).await;
    assert_eq!(s, 201, "{text}");
    let (s, _, text) = e.json(e.post("/api/v1/grabs").json(&grab)).await;
    assert_eq!(s, 201, "{text}");
    e.tick().await;
    assert_eq!(adds(&e), 1);
    assert_eq!(e.qb.lock().unwrap().torrents[&hash].category, "um-movies");
}
