//! End-to-end execution against loopback mocks with real generated media: search, grab,
//! qBittorrent submission (4.3.9 and 5.x protocol), completion, ffprobe-validated import,
//! seeding retention, cleanup after a met share limit, Plex refresh, restart reconciliation.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};

async fn approved_ref(e: &Exec, media: &str) -> String {
    let (s, v, text) = e
        .json(e.get(&format!("/api/v1/media/{media}/releases")))
        .await;
    assert_eq!(s, 200, "{text}");
    assert_no_leaks(&text);
    v["releases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["decision"]["approved"] == true)
        .unwrap_or_else(|| panic!("no approved release: {text}"))["release_ref"]
        .as_str()
        .unwrap()
        .to_string()
}

fn matrix_files() -> Vec<(String, Vec<u8>)> {
    vec![
        ("The.Matrix.1999.1080p.WEB-DL.x264-UM.mkv".into(), mkv(3)),
        ("Sample/the.matrix.sample.mkv".into(), mkv(1)),
        (
            "The.Matrix.1999.1080p.WEB-DL.x264-UM.nfo".into(),
            b"nfo".to_vec(),
        ),
    ]
}

#[tokio::test]
async fn movie_grab_import_seed_and_cleanup_end_to_end() {
    let e = start_exec(ExecOptions::default()).await;
    let (hash, r) = e.publish(
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        matrix_files(),
        8.0,
        false,
    );
    e.search_results("The Matrix 1999", vec![r]);
    let rel = approved_ref(&e, "movie:tmdb:603").await;

    let (s, v, text) = e
        .json(
            e.post("/api/v1/grabs")
                .json(&json!({ "media_id": "movie:tmdb:603", "release_ref": rel })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_no_leaks(&text);
    let id = v["grab"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["grab"]["status"], "pending_submission");
    assert!(
        v["grab"].get("info_hash").is_none(),
        "raw hashes stay private"
    );
    assert!(!text.contains(&hash));
    // The movie is now covered: a second grab is refused, the plan reports it in progress.
    let (s, v, _) = e
        .json(
            e.post("/api/v1/grabs")
                .json(&json!({ "media_id": "movie:tmdb:603", "release_ref": rel })),
        )
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("already_queued"))
    );
    let (_, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603" })),
        )
        .await;
    assert_eq!(v["plan"]["state"], "in_progress", "{v}");

    // Submission: one add with only 4.3.9 parameters, our category and tag.
    e.tick().await;
    {
        let q = e.qb.lock().unwrap();
        assert_eq!(q.add_fields.len(), 1);
        let f = &q.add_fields[0];
        assert_eq!(f["category"], "radarr");
        assert_eq!(f["tags"], "unified-media");
        assert_eq!(f["savepath"], "/downloads");
        assert_eq!(f["paused"], "false");
        assert_eq!(f["ratioLimit"], "2", "indexer seed ratio is sent");
        assert!(
            f.contains_key("torrents"),
            "torrent file payload (no magnet in the result)"
        );
        assert!(q.unknown_params.is_empty(), "{:?}", q.unknown_params);
        assert_eq!(q.torrents[&hash].category, "radarr");
    }
    assert_eq!(e.grab_row(&id).state, "downloading");
    // Re-running the pass does not add again.
    e.tick().await;
    assert_eq!(e.qb.lock().unwrap().add_fields.len(), 1);

    // Completion -> import (hardlink, native scene name, ffprobe-validated).
    qb::complete(&e.qb, &hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "imported", "{:?}", g.last_error);
    let dest = e
        .movies
        .join("The Matrix (1999)/The.Matrix.1999.1080p.WEB-DL.x264-UM.mkv");
    let source = e
        .downloads
        .join("The.Matrix.1999.1080p.WEB-DL.x264-UM/The.Matrix.1999.1080p.WEB-DL.x264-UM.mkv");
    assert!(dest.is_file() && source.is_file());
    assert_eq!(
        ino(&dest),
        ino(&source),
        "hardlinked; the source stays for seeding"
    );
    assert!(
        !e.movies
            .join("The Matrix (1999)/the.matrix.sample.mkv")
            .exists(),
        "samples are not imported"
    );
    assert!(
        std::fs::read_dir(e.movies.join("The Matrix (1999)"))
            .unwrap()
            .count()
            == 1,
        "no staging leftovers"
    );
    let (_, v, _) = e.json(e.get("/api/v1/library/movies?q=matrix")).await;
    let m = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["tmdb_id"] == 603)
        .unwrap();
    assert_eq!(m["has_file"], true);
    assert_eq!(m["file"]["source"], "import");
    assert!(
        m["file"]["probe"]
            .as_str()
            .unwrap()
            .starts_with("ok: matroska"),
        "{}",
        m["file"]
    );
    assert_eq!(m["file"]["quality"], "webdl_1080p");
    assert_eq!(m["file"]["revision"]["version"], 1);

    // Seeding: kept until qBittorrent stops it at a met share limit.
    let (_, v, _) = e.json(e.get(&format!("/api/v1/queue/{id}"))).await;
    assert_eq!(v["grab"]["status"], "seeding", "{v}");
    assert_eq!(v["grab"]["cleanup"]["policy"]["remove_completed"], true);
    e.tick().await;
    assert!(source.exists(), "still seeding: kept");
    assert!(
        e.grab_row(&id)
            .cleanup_detail
            .unwrap()
            .contains("still seeding")
    );
    // Manually paused before the goal: still kept.
    qb::seeded(&e.qb, &hash, 0.4, 600, true);
    e.tick().await;
    assert!(source.exists());
    assert!(
        e.grab_row(&id)
            .cleanup_detail
            .unwrap()
            .contains("share limit not reached"),
        "{:?}",
        e.grab_row(&id).cleanup_detail
    );
    // Met: removed from the client with its data; the library file stays readable.
    qb::seeded(&e.qb, &hash, 2.05, 7200, true);
    e.tick().await;
    assert_eq!(e.grab_row(&id).cleanup, "removing");
    assert!(!e.qb.lock().unwrap().torrents.contains_key(&hash));
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.cleanup, "cleaned", "{:?}", g.cleanup_detail);
    assert!(
        !source.exists()
            && !e
                .downloads
                .join("The.Matrix.1999.1080p.WEB-DL.x264-UM")
                .exists()
    );
    assert!(dest.is_file());
    assert!(
        playable(&dest).await,
        "library copy intact after source cleanup"
    );
    let log = e.qb.lock().unwrap().log.clone();
    assert!(
        log.iter().any(|l| l == &format!("delete {hash}")),
        "{log:?}"
    );
    // The movie stays owned and nothing is re-grabbed.
    e.tick().await;
    assert_eq!(e.qb.lock().unwrap().add_fields.len(), 1);
    let (_, v, _) = e.json(e.get("/api/v1/wanted")).await;
    assert!(
        !v["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["target"] == "movie:tmdb:603")
    );

    // Plex refresh ran once for the movie section, with the token.
    assert_eq!(e.mock.lock().unwrap().plex, vec![("1".to_string(), true)]);
    let (_, v, text) = e
        .json(e.get("/api/v1/history?media_id=movie:tmdb:603"))
        .await;
    assert!(!text.contains(&hash), "history carries no raw hash");
    let (_, _, text) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert!(!text.contains(&hash), "queue carries no raw hash");
    let kinds: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    for k in [
        "grabbed",
        "download_started",
        "download_completed",
        "file_imported",
        "import_completed",
        "download_removed",
    ] {
        assert!(kinds.contains(&k), "{k} missing from {kinds:?}");
    }
}

#[tokio::test]
async fn qbittorrent_5_naming_magnets_and_episode_import() {
    let e = start_exec(ExecOptions {
        api: QB_API_5_0,
        ..Default::default()
    })
    .await;
    let files = vec![(
        "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM.mkv".to_string(),
        mkv(2),
    )];
    let (hash, r) = e.publish(
        "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM",
        "unused",
        files,
        1.5,
        true,
    );
    e.search_results("Slow Horses S01E02", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    // Queued in the outbox is not a started download: nothing has reached qBittorrent yet.
    assert_eq!(v["plan"]["state"], "download_queued", "{text}");
    assert_eq!(v["plan"]["execution"]["submitted"], false);
    assert_eq!(v["plan"]["execution"]["grabs"].as_array().unwrap().len(), 1);
    assert_no_leaks(&text);
    let plan_id = v["plan"]["id"].as_str().unwrap().to_string();
    // While the client is unreachable the request stays queued, honestly reported.
    e.qb.lock().unwrap().down = true;
    e.tick().await;
    let (_, p, _) = e.json(e.get(&format!("/api/v1/plans/{plan_id}"))).await;
    assert_eq!(p["plan"]["state"], "download_queued");
    assert_eq!(p["plan"]["execution"]["submitted"], false);
    let gid0 = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    let (_, g, _) = e.json(e.get(&format!("/api/v1/queue/{gid0}"))).await;
    assert_eq!(g["grab"]["status"], "pending_submission");
    assert!(g["grab"]["submitted_at"].is_null());
    e.qb.lock().unwrap().down = false;
    // The outage backed the submission off; the next attempt confirms it.
    e.tick_at(time::OffsetDateTime::now_utc() + std::time::Duration::from_secs(120))
        .await;
    let (_, p, _) = e.json(e.get(&format!("/api/v1/plans/{plan_id}"))).await;
    assert_eq!(p["plan"]["state"], "download_started", "{p}");
    assert_eq!(p["plan"]["execution"]["submitted"], true);
    assert!(p["plan"]["execution"]["confirmed_at"].is_string());
    {
        let q = e.qb.lock().unwrap();
        let f = &q.add_fields[0];
        assert!(
            f["urls"].starts_with("magnet:?"),
            "magnet preferred when present"
        );
        assert_eq!(f["stopped"], "false", "5.x add flag");
        assert!(!f.contains_key("paused"));
        assert_eq!(f["category"], "tv-sonarr");
    }
    let gid = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    // 5.x pause/resume use stop/start.
    let (s, _, text) = e.json(e.post(&format!("/api/v1/queue/{gid}/pause"))).await;
    assert_eq!(s, 200, "{text}");
    assert!(
        e.qb.lock()
            .unwrap()
            .log
            .iter()
            .any(|l| l.starts_with("stop "))
    );
    let (s, _, _) = e.json(e.post(&format!("/api/v1/queue/{gid}/resume"))).await;
    assert_eq!(s, 200);
    qb::complete(&e.qb, &hash);
    e.tick().await;
    let g = e.grab_row(&gid);
    assert_eq!(g.state, "imported", "{:?}", g.last_error);
    let dest =
        e.tv.join("Slow Horses/Season 1/Slow.Horses.S01E02.1080p.WEB-DL.x264-UM.mkv");
    assert!(dest.is_file());
    let (_, v, _) = e
        .json(e.get("/api/v1/plans?target=episode:tvdb:372264:1:2"))
        .await;
    assert_eq!(v["items"][0]["state"], "imported", "plan follows its grabs");
    e.tick().await; // the Plex refresh command queued by the import runs on the next pass
    assert_eq!(e.mock.lock().unwrap().plex, vec![("2".to_string(), true)]);
}

#[tokio::test]
async fn ambiguous_add_is_reconciled_by_hash_without_duplicates_across_restart() {
    let mut e = start_exec(ExecOptions::default()).await;
    let (hash, r) = e.publish(
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        matrix_files(),
        8.0,
        false,
    );
    e.search_results("The Matrix 1999", vec![r]);
    let rel = approved_ref(&e, "movie:tmdb:603").await;
    let (_, v, _) = e
        .json(
            e.post("/api/v1/grabs")
                .json(&json!({ "media_id": "movie:tmdb:603", "release_ref": rel })),
        )
        .await;
    let id = v["grab"]["id"].as_str().unwrap().to_string();
    // qBittorrent accepts the torrent but the answer is lost.
    e.qb.lock().unwrap().withhold_add_answer = true;
    e.qb.lock().unwrap().down = false;
    e.tick().await;
    // Found by hash right after the failed answer: confirmed, one add.
    assert_eq!(e.grab_row(&id).state, "downloading");
    assert_eq!(e.qb.lock().unwrap().add_fields.len(), 1);

    // Second scenario: the client dies after accepting; we restart while unconfirmed.
    let (hash2, r2) = e.publish(
        "Slow.Horses.S01E04.1080p.WEB-DL.x264-UM",
        "x",
        vec![("Slow.Horses.S01E04.1080p.WEB-DL.x264-UM.mkv".into(), mkv(2))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E04", vec![r2]);
    let rel2 = approved_ref(&e, "episode:tvdb:372264:1:4").await;
    let (_, v, _) = e
        .json(
            e.post("/api/v1/grabs")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:4", "release_ref": rel2 })),
        )
        .await;
    let id2 = v["grab"]["id"].as_str().unwrap().to_string();
    {
        let mut q = e.qb.lock().unwrap();
        q.withhold_add_answer = true;
        q.down_after_add = true; // the verification lookup fails too: outcome unknown
    }
    e.tick().await;
    assert_eq!(e.grab_row(&id2).state, "unconfirmed");
    assert!(e.qb.lock().unwrap().torrents.contains_key(&hash2));
    e.qb.lock().unwrap().down = false;
    let recovery = e.restart().await;
    assert!(recovery["imports_to_retry"].is_number());
    e.tick().await;
    assert_eq!(
        e.grab_row(&id2).state,
        "downloading",
        "reconciled by hash after restart"
    );
    assert_eq!(
        e.qb.lock().unwrap().add_fields.len(),
        2,
        "never added twice"
    );
    assert!(e.qb.lock().unwrap().torrents.contains_key(&hash));
}

#[tokio::test]
async fn foreign_torrents_are_never_touched() {
    let e = start_exec(ExecOptions::default()).await;
    let (hash, r) = e.publish(
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        matrix_files(),
        8.0,
        false,
    );
    // The same torrent already exists, added by someone else (native Radarr: no tag).
    e.qb.lock().unwrap().torrents.insert(
        hash.clone(),
        qb::MockTorrent {
            hash: hash.clone(),
            name: "The.Matrix.1999.1080p.WEB-DL.x264-UM".into(),
            category: "radarr".into(),
            tags: "".into(),
            save_path: "/downloads".into(),
            state: "stalledUP".into(),
            progress: 1.0,
            files: vec![],
            ratio: 9.0,
            ratio_limit: -1.0,
            seeding_time: 99999,
            seeding_time_limit: -1,
            priorities: vec![],
        },
    );
    e.search_results("The Matrix 1999", vec![r]);
    let rel = approved_ref(&e, "movie:tmdb:603").await;
    let (_, v, _) = e
        .json(
            e.post("/api/v1/grabs")
                .json(&json!({ "media_id": "movie:tmdb:603", "release_ref": rel })),
        )
        .await;
    let id = v["grab"]["id"].as_str().unwrap().to_string();
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(
        (g.state.as_str(), g.failure.as_deref()),
        ("failed", Some("foreign_torrent"))
    );
    {
        let q = e.qb.lock().unwrap();
        assert!(q.add_fields.is_empty(), "never re-added");
        assert!(
            q.log.iter().all(|l| !l.starts_with("pause")
                && !l.starts_with("delete")
                && !l.starts_with("setCategory")),
            "{:?}",
            q.log
        );
        assert_eq!(q.torrents[&hash].tags, "");
    }
    // Actions on it are refused.
    let (s, _, _) = e.json(e.post(&format!("/api/v1/queue/{id}/pause"))).await;
    assert_eq!(s, 409);
}

#[tokio::test]
async fn session_expiry_relogs_once_and_bad_credentials_are_reported() {
    let e = start_exec(ExecOptions::default()).await;
    let (_, v, _) = e.json(e.get("/api/v1/diagnostics")).await;
    assert_eq!(v["qbittorrent"]["status"], "ok", "{v}");
    assert_eq!(v["qbittorrent"]["webapi"], "2.8.2");
    assert_eq!(v["qbittorrent"]["naming"], "4.x pause/resume");
    assert_eq!(v["ffprobe"]["status"], "ok");
    let logins = e.qb.lock().unwrap().logins;
    e.qb.lock().unwrap().expire_session = true;
    let (_, v, _) = e.json(e.get("/api/v1/diagnostics")).await;
    assert_eq!(v["qbittorrent"]["status"], "ok");
    assert_eq!(
        e.qb.lock().unwrap().logins,
        logins + 1,
        "one re-login after 403"
    );
    let text = serde_json::to_string(&v).unwrap();
    assert!(!text.contains(FAKE_PASSWORD) && !text.contains(qb::SID) && !text.contains(FAKE_USER));
    e.qb.lock().unwrap().down = true;
    let (_, v, _) = e.json(e.get("/api/v1/diagnostics")).await;
    assert_eq!(v["qbittorrent"]["status"], "failed");
    let _: Value = v;
}
