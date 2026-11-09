//! Live status follows downloads and imports (season completeness, per-episode states), and a
//! file that appears in the library while a download is under way is never duplicated.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn live_status_reports_season_completeness_and_follows_imports() {
    let e = start_exec(ExecOptions::default()).await;
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
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2" })),
        )
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
            season["complete_aired"].as_bool()
        ),
        (Some("live"), Some(2), Some(6), Some(1), Some(false)),
        "{text}"
    );
    let ep = |v: &serde_json::Value, n: u64| {
        v["episodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["episode"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(ep(&st, 1)["state"], "owned", "{text}");
    assert_eq!(ep(&st, 2)["state"], "downloading", "{text}");
    assert!(ep(&st, 2)["download"]["id"].is_string());
    assert_eq!(ep(&st, 4)["state"], "missing", "{text}");
    assert_eq!(ep(&st, 2)["monitored"], true);

    e.complete(&hash);
    e.tick().await;
    let (_, st, text) = e.json(e.get("/api/v1/status/season:tvdb:372264:1")).await;
    assert_eq!(ep(&st, 2)["state"], "owned", "{text}");
    assert_eq!(st["seasons"][0]["episode_file_count"], 3, "{text}");
    // The library listing shows the same live count.
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
async fn a_file_appearing_during_the_download_is_never_duplicated() {
    let e = start_exec(ExecOptions::default()).await;
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
                .json(&json!({ "media_id": "movie:tmdb:603" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    e.tick().await;
    // Meanwhile another program puts its own copy in the library under another name.
    let folder = e.movies.join("The Matrix (1999)");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("The.Matrix.1999.1080p.BluRay.x264-ARR.mkv"),
        mkv(3),
    )
    .unwrap();
    e.complete(&hash);
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
