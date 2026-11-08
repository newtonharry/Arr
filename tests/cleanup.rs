//! Completed/failed download cleanup (native removeCompletedDownloads/removeFailedDownloads):
//! only owned, confined, journaled download data, only after a verified import and a met
//! share limit; library files always intact; crash-safe at the removal boundary.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

struct Imported {
    id: String,
    hash: String,
    dest: PathBuf,
    source: PathBuf,
}

const NAME: &str = "The.Matrix.1999.1080p.WEB-DL.x264-UM";

async fn imported_movie(e: &Exec) -> Imported {
    let files = vec![
        (format!("{NAME}.mkv"), mkv(2)),
        ("Sample/sample.mkv".to_string(), mkv(1)),
    ];
    let (hash, r) = e.publish(NAME, NAME, files, 8.0, false);
    e.search_results("The Matrix 1999", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    qb::complete(&e.qb, &hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "imported", "{:?}", g.last_error);
    Imported {
        id,
        hash,
        dest: e.movies.join(format!("The Matrix (1999)/{NAME}.mkv")),
        source: e.downloads.join(format!("{NAME}/{NAME}.mkv")),
    }
}

async fn assert_library_intact(i: &Imported) {
    assert!(i.dest.is_file(), "library file kept");
    assert!(playable(&i.dest).await, "library file still playable");
}

#[cfg(unix)] // Unix permissions or symlinks
#[tokio::test]
async fn ownership_moves_and_symlinks_refuse_cleanup() {
    // Category changed by the user: treated as user-managed.
    let e = start_exec(ExecOptions::default()).await;
    let i = imported_movie(&e).await;
    qb::seeded(&e.qb, &i.hash, 3.0, 9000, true);
    e.qb.lock()
        .unwrap()
        .torrents
        .get_mut(&i.hash)
        .unwrap()
        .category = "mine".into();
    e.tick().await;
    let g = e.grab_row(&i.id);
    assert_eq!(g.cleanup, "blocked");
    assert!(g.cleanup_detail.unwrap().contains("user-managed"));
    assert!(i.source.exists() && e.qb.lock().unwrap().torrents.contains_key(&i.hash));
    let (_, v, _) = e.json(e.get(&format!("/api/v1/queue/{}", i.id))).await;
    assert_eq!(v["grab"]["status"], "cleanup_blocked");
    assert_library_intact(&i).await;

    // Moved by the user to another directory.
    let e = start_exec(ExecOptions::default()).await;
    let i = imported_movie(&e).await;
    qb::seeded(&e.qb, &i.hash, 3.0, 9000, true);
    e.qb.lock()
        .unwrap()
        .torrents
        .get_mut(&i.hash)
        .unwrap()
        .save_path = "/downloads/elsewhere".into();
    e.tick().await;
    let g = e.grab_row(&i.id);
    assert_eq!(g.cleanup, "blocked");
    assert!(
        g.cleanup_detail.unwrap().contains("moved"),
        "{:?}",
        e.grab_row(&i.id).cleanup_detail
    );
    assert!(i.source.exists());

    // A directory inside the download replaced by a symlink into the library.
    let e = start_exec(ExecOptions::default()).await;
    let i = imported_movie(&e).await;
    qb::seeded(&e.qb, &i.hash, 3.0, 9000, true);
    let sample_dir = e.downloads.join(format!("{NAME}/Sample"));
    std::fs::remove_dir_all(&sample_dir).unwrap();
    std::os::unix::fs::symlink(e.movies.join("The Matrix (1999)"), &sample_dir).unwrap();
    e.tick().await;
    let g = e.grab_row(&i.id);
    assert_eq!(g.cleanup, "blocked", "{:?}", g.cleanup_detail);
    assert!(g.cleanup_detail.unwrap().contains("symbolic link"));
    assert!(
        e.qb.lock().unwrap().torrents.contains_key(&i.hash),
        "the client was never asked to delete"
    );
    assert_library_intact(&i).await;
    // Retry re-evaluates (still blocked while the link exists).
    let (s, v, _) = e
        .json(e.post(&format!("/api/v1/queue/{}/retry", i.id)))
        .await;
    assert_eq!((s, v["action"].as_str()), (200, Some("cleanup")));
}

#[tokio::test]
async fn restart_at_the_removal_boundary_finishes_cleanup_and_never_redownloads() {
    let mut e = start_exec(ExecOptions::default()).await;
    let i = imported_movie(&e).await;
    qb::seeded(&e.qb, &i.hash, 2.5, 9000, true);
    // Boundary 1: removal journaled, crash before the client was asked.
    let files = json!([{ "path": i.source, "size": std::fs::metadata(&i.source).unwrap().len(), "ino": ino(&i.source) }]).to_string();
    let (id, f2) = (i.id.clone(), files.clone());
    e.state
        .store
        .blocking(move |c| Ok(c.execute("UPDATE grabs SET cleanup = 'removing', cleanup_files = ?2, cleanup_at = '2026-01-01T00:00:00Z' WHERE id = ?1", rusqlite::params![id, f2])?))
        .unwrap();
    e.restart().await;
    e.tick().await; // still present and ours: asked again
    assert!(!e.qb.lock().unwrap().torrents.contains_key(&i.hash));
    e.tick().await; // gone: finished
    let g = e.grab_row(&i.id);
    assert_eq!(
        (g.state.as_str(), g.cleanup.as_str(), g.active),
        ("imported", "cleaned", false),
        "{:?}",
        g.cleanup_detail
    );
    assert!(!i.source.exists());
    assert_library_intact(&i).await;

    // Boundary 2: the client removed the torrent but its data was not deleted (crash in
    // between): our own confined deletion finishes it on restart.
    let mut e = start_exec(ExecOptions::default()).await;
    let i = imported_movie(&e).await;
    let files = json!([{ "path": i.source, "size": std::fs::metadata(&i.source).unwrap().len(), "ino": ino(&i.source) }]).to_string();
    let id = i.id.clone();
    e.state
        .store
        .blocking(move |c| Ok(c.execute("UPDATE grabs SET cleanup = 'removing', cleanup_files = ?2, cleanup_at = '2026-01-01T00:00:00Z' WHERE id = ?1", rusqlite::params![id, files])?))
        .unwrap();
    e.qb.lock().unwrap().torrents.remove(&i.hash);
    e.restart().await;
    let report = e.tick().await;
    assert!(report.tasks.contains(&"downloads".to_string()));
    let g = e.grab_row(&i.id);
    assert_eq!(
        (g.state.as_str(), g.cleanup.as_str()),
        ("imported", "cleaned"),
        "{:?}",
        g.cleanup_detail
    );
    assert!(!i.source.exists(), "journaled file removed by us");
    assert!(
        e.downloads
            .join(format!("{NAME}/Sample/sample.mkv"))
            .exists(),
        "untracked by the journal: left alone"
    );
    assert_library_intact(&i).await;
    // Nothing treats the vanished torrent as a failed download or re-grabs.
    e.tick_at(time::OffsetDateTime::now_utc() + Duration::from_secs(3600))
        .await;
    assert_eq!(e.grab_row(&i.id).state, "imported");
    assert_eq!(e.qb.lock().unwrap().add_fields.len(), 1);
    let (_, v, _) = e.json(e.get("/api/v1/blocklist")).await;
    assert!(v["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn disabled_flags_keep_downloads() {
    // Native flags off.
    let mut snap = snapshot();
    snap["radarr"]["downloadclient"][0]["removeCompletedDownloads"] = json!(false);
    let e = start_exec(ExecOptions {
        snapshot: snap,
        ..Default::default()
    })
    .await;
    let i = imported_movie(&e).await;
    let g = e.grab_row(&i.id);
    assert_eq!(g.cleanup, "disabled");
    assert!(!g.remove_completed);
    qb::seeded(&e.qb, &i.hash, 5.0, 99999, true);
    e.tick().await;
    assert!(i.source.exists() && e.qb.lock().unwrap().torrents.contains_key(&i.hash));
    let (_, v, _) = e.json(e.get(&format!("/api/v1/queue/{}", i.id))).await;
    assert_eq!(v["grab"]["status"], "imported");
    assert_eq!(v["grab"]["cleanup"]["state"], "disabled");

    // Operator kill switch.
    let e = start_exec(ExecOptions {
        cleanup: false,
        ..Default::default()
    })
    .await;
    let i = imported_movie(&e).await;
    assert_eq!(e.grab_row(&i.id).cleanup, "disabled");
    qb::seeded(&e.qb, &i.hash, 5.0, 99999, true);
    e.tick().await;
    assert!(i.source.exists());
    let (_, v, _) = e.json(e.get("/api/v1/capabilities")).await;
    let cap = v["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "download_cleanup")
        .unwrap()
        .clone();
    assert_eq!(cap["configured"], false);
}

#[tokio::test]
async fn unlimited_or_unknown_share_limits_keep_seeding() {
    let e = start_exec(ExecOptions::default()).await;
    let i = imported_movie(&e).await;
    // The user set the torrent to unlimited and paused it: never removed automatically.
    {
        let mut q = e.qb.lock().unwrap();
        let t = q.torrents.get_mut(&i.hash).unwrap();
        t.ratio_limit = -1.0;
        t.seeding_time_limit = -1;
    }
    qb::seeded(&e.qb, &i.hash, 7.0, 999999, true);
    e.tick().await;
    assert!(i.source.exists());
    assert!(
        e.grab_row(&i.id)
            .cleanup_detail
            .unwrap()
            .contains("no share limit applies")
    );
    // Global limits: enabled at ratio 1.5 but the indexer goal (2.0) is not met yet.
    {
        let mut q = e.qb.lock().unwrap();
        q.prefs = json!({ "max_ratio_enabled": true, "max_ratio": 1.5, "max_seeding_time_enabled": false, "max_seeding_time": -1 });
        let t = q.torrents.get_mut(&i.hash).unwrap();
        t.ratio_limit = -2.0;
        t.seeding_time_limit = -2;
    }
    qb::seeded(&e.qb, &i.hash, 1.6, 999999, true);
    e.tick().await;
    assert!(i.source.exists());
    assert!(
        e.grab_row(&i.id)
            .cleanup_detail
            .unwrap()
            .contains("indexer seed goal"),
        "{:?}",
        e.grab_row(&i.id).cleanup_detail
    );
    qb::seeded(&e.qb, &i.hash, 2.0, 999999, true);
    e.tick().await;
    e.tick().await;
    assert_eq!(e.grab_row(&i.id).cleanup, "cleaned");
    assert_library_intact(&i).await;
}

#[tokio::test]
async fn failed_download_cleanup_keeps_blocklist_and_retry_choice() {
    let mut snap = snapshot();
    snap["radarr"]["config/downloadclient"]["autoRedownloadFailed"] = json!(true);
    let e = start_exec(ExecOptions {
        snapshot: snap,
        extra: "".into(),
        ..Default::default()
    })
    .await;
    let (hash, r) = e.publish(
        NAME,
        NAME,
        vec![(format!("{NAME}.mkv"), mkv(2))],
        8.0,
        false,
    );
    let (_, alt) = e.publish(
        "The.Matrix.1999.1080p.WEB-DL.x264-ALT",
        "alt",
        vec![("The.Matrix.1999.1080p.WEB-DL.x264-ALT.mkv".into(), mkv(3))],
        8.0,
        false,
    );
    let mut r = r;
    r["seeders"] = json!(90); // ranked first
    e.search_results("The Matrix 1999", vec![r, alt]);
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
    let t0 = time::OffsetDateTime::now_utc();
    e.tick_at(t0).await;
    // Partially downloaded data exists, then it stalls for longer than the stall timeout.
    // (single-file torrent: the data lives directly in the save path)
    std::fs::write(e.downloads.join(format!("{NAME}.mkv")), b"partial").unwrap();
    std::fs::write(e.downloads.join("someone-elses.mkv"), b"untracked").unwrap();
    qb::set_state(&e.qb, &hash, "stalledDL", 0.3);
    e.tick_at(t0 + Duration::from_secs(60)).await; // progress since the last poll
    e.tick_at(t0 + Duration::from_secs(90)).await; // no progress: the stall timer starts
    e.tick_at(t0 + Duration::from_secs(60 + 13 * 3600)).await; // beyond the 12 h stall timeout
    let g = e.grab_row(&id);
    assert_eq!(
        (g.state.as_str(), g.failure.as_deref(), g.retry.as_deref()),
        ("failed", Some("stalled"), Some("research")),
        "{:?}",
        g.last_error
    );
    // Incomplete failed download: removed with its data (journaled), blocklist kept.
    e.tick_at(t0 + Duration::from_secs(60 + 13 * 3600 + 5))
        .await;
    e.tick_at(t0 + Duration::from_secs(60 + 13 * 3600 + 10))
        .await;
    let g = e.grab_row(&id);
    assert_eq!(g.cleanup, "cleaned", "{:?}", g.cleanup_detail);
    assert!(
        !e.downloads.join(format!("{NAME}.mkv")).exists(),
        "tracked partial data removed"
    );
    assert!(
        e.downloads.join("someone-elses.mkv").exists(),
        "untracked data untouched"
    );
    let (_, v, _) = e
        .json(e.get("/api/v1/blocklist?media_id=movie:tmdb:603"))
        .await;
    let items = v["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["reason"], "stalled");
    // The re-search ran (command) and grabbed the alternative, never the blocklisted release.
    let (_, v, _) = e.json(e.get("/api/v1/commands")).await;
    assert!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "search" && c["media_id"] == "movie:tmdb:603")
    );
    let (_, v, _) = e.json(e.get("/api/v1/queue")).await;
    let active: Vec<_> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| g["active"] == true)
        .collect();
    assert_eq!(active.len(), 1, "{v}");
    assert!(active[0]["title"].as_str().unwrap().ends_with("-ALT"));
}

#[tokio::test]
async fn explicit_removal_needs_met_seed_goal_or_override() {
    let e = start_exec(ExecOptions::default()).await;
    let i = imported_movie(&e).await;
    // Default cancel keeps everything.
    let (s, v, _) = e
        .json(e.post(&format!("/api/v1/queue/{}/cancel", i.id)))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["grab"]["cleanup"]["state"], "kept");
    assert!(i.source.exists() && e.qb.lock().unwrap().torrents.contains_key(&i.hash));
    // Explicit data removal while the goal is unmet: refused, actionable.
    let (s, v, _) = e
        .json(
            e.post(&format!("/api/v1/queue/{}/cancel", i.id))
                .json(&json!({ "remove": "torrent_and_data" })),
        )
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("seed_criteria_unmet")),
        "{v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ignore_seed_criteria")
    );
    // Override: removes only our torrent and its tracked data.
    let (s, v, _) = e
        .json(
            e.post(&format!("/api/v1/queue/{}/cancel", i.id))
                .json(&json!({ "remove": "torrent_and_data", "ignore_seed_criteria": true })),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert!(!e.qb.lock().unwrap().torrents.contains_key(&i.hash));
    e.tick().await;
    assert_eq!(e.grab_row(&i.id).cleanup, "cleaned");
    assert!(!i.source.exists());
    assert_library_intact(&i).await;
    // Unknown body fields are rejected.
    let (s, _, _) = e
        .json(
            e.post(&format!("/api/v1/queue/{}/cancel", i.id))
                .json(&json!({ "delete_everything": true })),
        )
        .await;
    assert_eq!(s, 422);
}
