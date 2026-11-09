//! Torznab RSS sync through Prowlarr: routine acquisition of new releases for managed,
//! monitored items only, idempotent across replays and restarts, outage-safe, never a bulk
//! backlog search.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};
use std::time::Duration;

fn item(e: &Exec, title: &str, files: Vec<(String, Vec<u8>)>, gib: f64, cats: &[u32]) -> FeedItem {
    let (hash, _) = e.publish(title, title, files, gib, false);
    FeedItem {
        title: title.into(),
        guid: format!("guid-{hash}"),
        url: format!("http://{}/tz/dl/{hash}", e.mock_addr),
        size: (gib * 1024.0 * 1024.0 * 1024.0) as i64,
        seeders: 20,
        info_hash: Some(hash),
        categories: cats.to_vec(),
        pub_date: "Mon, 26 Oct 2026 08:00:00 +0000".into(),
    }
}

fn set_feed(e: &Exec, items: &[FeedItem]) {
    e.mock
        .lock()
        .unwrap()
        .feeds
        .insert("1".into(), feed_xml(items));
}

async fn rss_sync(e: &Exec) -> Value {
    let (s, v, text) = e
        .json(
            e.post("/api/v1/commands")
                .json(&json!({ "name": "rss_sync" })),
        )
        .await;
    assert!(s == 202 || s == 200, "{text}");
    let id = v["id"].as_str().unwrap().to_string();
    e.tick().await;
    let (_, v, _) = e.json(e.get(&format!("/api/v1/commands/{id}"))).await;
    assert_eq!(v["command"]["state"], "completed", "{v}");
    v["command"]["result"].clone()
}

fn searches(e: &Exec) -> usize {
    e.mock
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|q| q.contains("t=search"))
        .count()
}

#[tokio::test]
async fn feed_grabs_only_tracked_monitored_wanted_items_and_replays_are_idempotent() {
    let mut e = start_exec(ExecOptions::default()).await;
    // E04 unmonitored by the user.
    let (s, _, _) = e
        .json(
            e.patch("/api/v1/library/episode:tvdb:372264:1:4")
                .json(&json!({ "monitored": false })),
        )
        .await;
    assert_eq!(s, 200);
    let items = vec![
        item(
            &e,
            "Slow.Horses.S01E02.1080p.WEB-DL.x264-RSS",
            vec![(
                "Slow.Horses.S01E02.1080p.WEB-DL.x264-RSS.mkv".into(),
                mkv(2),
            )],
            1.5,
            &[5000, 5040],
        ),
        item(
            &e,
            "Slow.Horses.S01E04.1080p.WEB-DL.x264-RSS",
            vec![(
                "Slow.Horses.S01E04.1080p.WEB-DL.x264-RSS.mkv".into(),
                mkv(2),
            )],
            1.5,
            &[5040],
        ),
        item(
            &e,
            "Slow.Horses.S01E01.1080p.WEB-DL.x264-RSS",
            vec![(
                "Slow.Horses.S01E01.1080p.WEB-DL.x264-RSS.mkv".into(),
                mkv(2),
            )],
            1.5,
            &[5040],
        ),
        item(
            &e,
            "Unknown.Show.S01E01.1080p.WEB-DL.x264-RSS",
            vec![("u.mkv".into(), mkv(1))],
            1.5,
            &[5040],
        ),
        item(
            &e,
            "Slow.Horses.S03E01.1080p.WEB-DL.x264-RSS",
            vec![("s3.mkv".into(), mkv(1))],
            1.5,
            &[5040],
        ),
        item(
            &e,
            "The.Matrix.1999.1080p.WEB-DL.x264-RSS",
            vec![("The.Matrix.1999.1080p.WEB-DL.x264-RSS.mkv".into(), mkv(2))],
            8.0,
            &[2000, 2040],
        ),
        item(
            &e,
            "The.Matrix.Reloaded.2003.1080p.WEB-DL.x264-RSS",
            vec![("r.mkv".into(), mkv(1))],
            8.0,
            &[2040],
        ),
        item(
            &e,
            "Slow.Horses.S01E05.720p.HDTV.x264-OTHERCAT",
            vec![("o.mkv".into(), mkv(1))],
            1.0,
            &[2040],
        ),
    ];
    set_feed(&e, &items);
    let before = searches(&e);
    let r = rss_sync(&e).await;
    assert_eq!(r["grabbed"], 2, "{r}");
    assert_eq!(r["retrieval"], "complete");
    assert_eq!(r["provider_completeness"], "complete_window");
    assert_eq!(searches(&e), before, "RSS never runs searches");
    {
        let feed_log = e.mock.lock().unwrap().feed_log.clone();
        assert!(
            feed_log
                .iter()
                .any(|l| l.starts_with("1 t=search") && l.contains("limit=100")),
            "{feed_log:?}"
        );
    }
    // Statuses are recorded per item; links never appear.
    let (_, v, text) = e.json(e.get("/api/v1/rss")).await;
    assert_no_leaks(&text);
    assert!(!text.contains("apikey") && !text.contains("/download"));
    let status = |t: &str| {
        v["recent"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"] == t)
            .map(|i| {
                (
                    i["status"].as_str().unwrap().to_string(),
                    i["detail"].as_str().unwrap_or("").to_string(),
                )
            })
            .unwrap()
    };
    assert_eq!(
        status("Slow.Horses.S01E02.1080p.WEB-DL.x264-RSS").0,
        "grabbed"
    );
    assert_eq!(status("The.Matrix.1999.1080p.WEB-DL.x264-RSS").0, "grabbed");
    let (s4, d4) = status("Slow.Horses.S01E04.1080p.WEB-DL.x264-RSS");
    assert_eq!(s4, "rejected");
    assert!(d4.contains("not_monitored"), "{d4}");
    assert!(
        status("Slow.Horses.S01E01.1080p.WEB-DL.x264-RSS")
            .1
            .contains("covers_owned")
    );
    assert_eq!(
        status("Unknown.Show.S01E01.1080p.WEB-DL.x264-RSS").0,
        "ignored"
    );
    assert_ne!(
        status("Slow.Horses.S03E01.1080p.WEB-DL.x264-RSS").0,
        "grabbed",
        "wrong season never grabbed"
    );
    assert!(
        status("The.Matrix.Reloaded.2003.1080p.WEB-DL.x264-RSS")
            .1
            .contains("covers_owned")
    );
    assert_eq!(
        status("Slow.Horses.S01E05.720p.HDTV.x264-OTHERCAT").0,
        "ignored",
        "TV numbering in a movie category"
    );
    e.tick().await;
    assert_eq!(e.fake.lock().unwrap().add_fields.len(), 2);

    // Replay of the same feed, and again after a restart: no duplicate grabs.
    let r = rss_sync(&e).await;
    assert_eq!(r["grabbed"], 0, "{r}");
    assert_eq!(r["already_grabbed"], 2);
    e.restart().await;
    let r = rss_sync(&e).await;
    assert_eq!(r["grabbed"], 0);
    e.tick().await;
    assert_eq!(e.fake.lock().unwrap().add_fields.len(), 2);
    let (_, v, _) = e.json(e.get("/api/v1/rss")).await;
    let ix = &v["indexers"][0];
    assert_eq!(ix["last_status"], "ok");
    assert_eq!(ix["newest_published_at"], "2026-10-26T08:00:00Z");
    assert_eq!(ix["gap_possible"], false);
}

/// Backlog Show (TVDB 999100): `seasons` x 50 old missing episodes plus one episode that
/// aired yesterday, all from TVmaze.
async fn backlog(e: &Exec, seasons: u16) {
    let yesterday =
        arr::import::rfc3339(time::OffsetDateTime::now_utc() - Duration::from_secs(86_400));
    let mut eps: Vec<Value> = vec![];
    for season in 1..=seasons {
        for n in 1..=50 {
            eps.push(json!({ "season": season, "number": n, "name": format!("Ep {n}"), "type": "regular", "airstamp": "2012-01-01T02:00:00+00:00", "runtime": 45 }));
        }
    }
    eps.push(json!({ "season": seasons + 1, "number": 1, "name": "Ep 1", "type": "regular", "airstamp": yesterday, "runtime": 45 }));
    e.mock
        .lock()
        .unwrap()
        .tvmaze_extra
        .insert("9100".into(), eps);
    e.add("series:tvdb:999100").await;
}

#[tokio::test]
async fn new_episode_is_grabbed_while_a_large_backlog_is_never_bulk_searched() {
    let e = start_exec(ExecOptions::default()).await;
    backlog(&e, 30).await;
    let (_, v, _) = e
        .json(e.get("/api/v1/wanted?media_id=series:tvdb:999100&limit=1"))
        .await;
    assert_eq!(v["total"], 1501);
    let fresh = item(
        &e,
        "Backlog.Show.S31E01.1080p.WEB-DL.x264-NEW",
        vec![(
            "Backlog.Show.S31E01.1080p.WEB-DL.x264-NEW.mkv".into(),
            mkv(2),
        )],
        1.2,
        &[5040],
    );
    set_feed(&e, &[fresh]);
    let before = searches(&e);
    // Ordinary worker passes (the scheduled RSS sync runs on the first one).
    e.tick().await;
    e.tick().await;
    assert_eq!(searches(&e), before, "no backlog search on its own");
    let (_, v, _) = e.json(e.get("/api/v1/queue")).await;
    let titles: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Backlog.Show.S31E01.1080p.WEB-DL.x264-NEW"]);
    let (_, v, _) = e.json(e.get("/api/v1/requests")).await;
    assert!(v["items"].as_array().unwrap().is_empty());
    let (_, v, _) = e.json(e.get("/api/v1/system/tasks")).await;
    let rss = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "rss_sync")
        .unwrap()
        .clone();
    assert_eq!(rss["last_status"], "ok", "{v}");
    assert!(rss["next_run"].is_string());

    // An explicit backlog search is bounded per pass and leaves room for other work.
    let (s, v, _) = e
        .json(
            e.post("/api/v1/commands")
                .json(&json!({ "name": "search_missing", "media_id": "series:tvdb:999100" })),
        )
        .await;
    assert_eq!(s, 202);
    let id = v["id"].as_str().unwrap().to_string();
    e.tick().await;
    let (_, v, _) = e.json(e.get(&format!("/api/v1/commands/{id}"))).await;
    assert_eq!(
        v["command"]["state"], "queued",
        "continues in later passes: {v}"
    );
    assert_eq!(v["command"]["result"]["searched_this_pass"], 5);
    assert_eq!(v["command"]["result"]["remaining_units"], 25);
    let searched_once = searches(&e) - before;
    assert!(
        searched_once <= 5 * 26,
        "season query plus bounded episode queries per unit: {searched_once}"
    );
    for _ in 0..6 {
        e.tick().await;
    }
    let (_, v, _) = e.json(e.get(&format!("/api/v1/commands/{id}"))).await;
    assert_eq!(v["command"]["state"], "completed", "{v}");
}

#[tokio::test]
async fn explicit_season_search_still_finds_packs() {
    // Season 2 of Slow Horses: all aired and missing, with an episode list.
    let e = start_exec(ExecOptions::default()).await;
    let name = "Slow.Horses.S02.1080p.WEB-DL.x264-PACK";
    let files: Vec<(String, Vec<u8>)> = (1..=6)
        .map(|n| {
            (
                format!("Slow.Horses.S02E{n:02}.1080p.WEB-DL.x264-PACK.mkv"),
                mkv(1),
            )
        })
        .collect();
    let (_, r) = e.publish(name, name, files, 9.0, false);
    e.search_results("Slow Horses S02", vec![r]);
    let (s, v, _) = e
        .json(
            e.post("/api/v1/commands")
                .json(&json!({ "name": "search", "media_id": "season:tvdb:372264:2" })),
        )
        .await;
    assert_eq!(s, 202);
    let id = v["id"].as_str().unwrap().to_string();
    e.tick().await;
    let (_, v, _) = e.json(e.get(&format!("/api/v1/commands/{id}"))).await;
    assert_eq!(v["command"]["state"], "completed", "{v}");
    let (_, q, _) = e.json(e.get("/api/v1/queue")).await;
    assert_eq!(q["items"][0]["title"], name);
    assert_eq!(q["items"][0]["full_season"], true);
    assert_eq!(q["items"][0]["targets"].as_array().unwrap().len(), 6);
}

#[tokio::test]
async fn newly_aired_metadata_becomes_matchable() {
    let e = start_exec(ExecOptions::default()).await;
    // TVmaze does not list Slow Horses S01E07 yet: RSS cannot map it.
    let it = item(
        &e,
        "Slow.Horses.S01E07.1080p.WEB-DL.x264-RSS",
        vec![(
            "Slow.Horses.S01E07.1080p.WEB-DL.x264-RSS.mkv".into(),
            mkv(2),
        )],
        2.0,
        &[5040],
    );
    set_feed(&e, &[it]);
    let r = rss_sync(&e).await;
    assert_eq!(r["grabbed"], 0);
    let (_, v, _) = e.json(e.get("/api/v1/rss")).await;
    assert_eq!(v["recent"][0]["detail"], "unknown_episode", "{v}");
    // The episode appears in TVmaze; a metadata refresh adds it, and it is missing.
    e.mock.lock().unwrap().tvmaze_extra.insert(
        "9001".into(),
        vec![json!({ "season": 1, "number": 7, "name": "Ep 7", "type": "regular", "airstamp": "2025-02-20T02:00:00+00:00", "runtime": 60 })],
    );
    let (s, v, _) = e
        .json(
            e.post("/api/v1/commands")
                .json(&json!({ "name": "refresh_metadata", "media_id": "series:tvdb:372264" })),
        )
        .await;
    assert_eq!(s, 202, "{v}");
    e.tick().await;
    let (_, v, _) = e
        .json(e.get("/api/v1/calendar?start=2025-02-19T00:00:00Z&end=2025-02-21T00:00:00Z"))
        .await;
    assert!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["target"] == "episode:tvdb:372264:1:7" && i["status"] == "missing"),
        "{v}"
    );
    let r = rss_sync(&e).await;
    assert_eq!(r["grabbed"], 1, "{r}");
}

#[tokio::test]
async fn outages_back_off_without_advancing_and_hostile_feeds_are_refused() {
    let e = start_exec(ExecOptions::default()).await;
    e.mock.lock().unwrap().feed_status = Some(500);
    let r = rss_sync(&e).await;
    assert_eq!(r["retrieval"], "incomplete");
    assert_eq!(r["indexers"][0]["status"], "failed");
    let (_, v, _) = e.json(e.get("/api/v1/rss")).await;
    let ix = v["indexers"][0].clone();
    assert_eq!(ix["failures"], 1);
    assert!(
        ix["last_success_at"].is_null() && ix["newest_published_at"].is_null(),
        "no progress recorded on failure"
    );
    assert!(v["recent"].as_array().unwrap().is_empty());
    // A scheduled pass inside the backoff skips the feed; a forced sync retries it.
    let report = arr::rss::sync(&e.state, time::OffsetDateTime::now_utc(), false)
        .await
        .unwrap();
    assert_eq!(report.indexers[0].status, "backoff");
    e.mock.lock().unwrap().feed_status = None;
    // Hostile documents: DTD/entity expansion refused, Torznab error codes reported without text.
    e.mock.lock().unwrap().feeds.insert("1".into(), r#"<?xml version="1.0"?><!DOCTYPE r [<!ENTITY x SYSTEM "file:///etc/passwd">]><rss><channel><item><title>&x;</title></item></channel></rss>"#.into());
    let r = rss_sync(&e).await;
    assert!(
        r["indexers"][0]["error"].as_str().unwrap().contains("DTD"),
        "{r}"
    );
    // An off-origin link is dropped: the item cannot be grabbed.
    let mut bad = item(
        &e,
        "The.Matrix.1999.1080p.WEB-DL.x264-EVIL",
        vec![("e.mkv".into(), mkv(1))],
        8.0,
        &[2040],
    );
    bad.url = format!("https://evil.example/download?apikey={PROWLARR_KEY}");
    set_feed(&e, &[bad]);
    let r = rss_sync(&e).await;
    assert_eq!(r["grabbed"], 0);
    assert_eq!(r["indexers"][0]["links_dropped"], 2);
    let (_, v, text) = e.json(e.get("/api/v1/rss")).await;
    assert!(!text.contains("evil.example") && !text.contains(PROWLARR_KEY));
    let evil = v["recent"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title"] == "The.Matrix.1999.1080p.WEB-DL.x264-EVIL")
        .unwrap()
        .clone();
    assert!(
        evil["detail"]
            .as_str()
            .unwrap()
            .contains("no_download_link"),
        "{evil}"
    );
    // A full window entirely newer than the last sync is flagged as a possible gap.
    let many: Vec<FeedItem> = (0..100)
        .map(|i| FeedItem {
            title: format!("Unrelated.Show.S01E{:02}.1080p.WEB-DL", i % 99 + 1),
            guid: format!("g{i}"),
            url: format!("http://{}/prowlarr/1/download?link=x{i}", e.mock_addr),
            size: 1_000_000_000,
            seeders: 1,
            info_hash: None,
            categories: vec![5040],
            pub_date: "Tue, 27 Oct 2026 08:00:00 +0000".into(),
        })
        .collect();
    let first = FeedItem {
        pub_date: "Mon, 26 Oct 2026 08:00:00 +0000".into(),
        ..many[0].clone()
    };
    set_feed(&e, &[first]);
    rss_sync(&e).await;
    set_feed(&e, &many);
    let r = rss_sync(&e).await;
    assert_eq!(r["indexers"][0]["gap_possible"], true, "{r}");
}

#[tokio::test]
async fn a_new_series_added_for_one_episode_is_matched_by_rss_for_that_episode_only() {
    let e = start_exec(ExecOptions::default()).await;
    let t0 = time::OffsetDateTime::now_utc();
    // The default episode add of a new series, before anything is released.
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "episode:tvdb:999005:1:1" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["plan"]["state"], "blocked", "{text}");
    assert_eq!(v["plan"]["units"][0]["status"], "no_release_observed");
    let (_, r, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(
        (
            r["items"][0]["target"].as_str(),
            r["items"][0]["state"].as_str()
        ),
        (Some("episode:tvdb:999005:1:1"), Some("active"))
    );
    let monitored = |e: &Exec| {
        let index = e.state.index();
        let s = index.series(arr::ids::SeriesKey::Tvdb(999005)).unwrap();
        assert!(s.monitored);
        s.episodes
            .iter()
            .flatten()
            .filter(|x| x.monitored)
            .map(|x| (x.season, x.number))
            .collect::<Vec<_>>()
    };
    assert_eq!(monitored(&e), [(1, 1)], "only the episode, not its season");

    let offer = |e: &Exec, t: &str| item(e, t, vec![(format!("{t}.mkv"), mkv(2))], 0.05, &[5040]);
    let wanted = "Parity.Partial.S01E01.1080p.WEB-DL.x264-RSS";
    let others = [
        "Parity.Partial.S01E02.1080p.WEB-DL.x264-RSS",
        "Parity.Partial.S01E01E02.1080p.WEB-DL.x264-RSS",
        "Parity.Partial.S01.1080p.WEB-DL.x264-RSS",
        "Parity.Partial.S02E01.1080p.WEB-DL.x264-RSS",
    ];
    let mut items = vec![offer(&e, wanted)];
    let wanted_hash = items[0].info_hash.clone().unwrap();
    items.extend(others.iter().map(|t| offer(&e, t)));
    set_feed(&e, &items);
    let r = rss_sync(&e).await;
    assert_eq!(r["grabbed"], 1, "{r}");
    let status = |v: &Value, t: &str| {
        let i = v["recent"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"] == t)
            .unwrap_or_else(|| panic!("{t}: {v}"))
            .clone();
        (
            i["status"].as_str().unwrap().to_string(),
            i["detail"].as_str().unwrap_or("").to_string(),
        )
    };
    let (_, v, _) = e.json(e.get("/api/v1/rss")).await;
    assert_eq!(status(&v, wanted).0, "grabbed");
    // The adjacent episode, a multi-episode release including it, the season pack and a
    // later season stay out of scope: each covers an episode that is not monitored.
    for t in others {
        let (s, d) = status(&v, t);
        assert_eq!(s, "rejected", "{t}: {d}");
        assert!(d.contains("not_monitored"), "{t}: {d}");
    }
    e.tick().await;
    {
        let q = e.fake.lock().unwrap();
        assert_eq!(q.add_fields.len(), 1);
        assert!(q.torrents.contains_key(&wanted_hash));
    }
    // The request kept from the add retries once after its backoff, finds the episode being
    // downloaded and settles without another grab.
    e.tick_at(t0 + Duration::from_secs(2 * 3600)).await;
    let (_, r, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(r["items"][0]["state"], "satisfied", "{r}");
    assert_eq!(e.fake.lock().unwrap().add_fields.len(), 1);

    // Metadata refresh keeps the scope: new episodes of the season and a new season are not
    // monitored, so they do not match either.
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
    assert_eq!(monitored(&e), [(1, 1)]);
    let later = [
        "Parity.Partial.S01E04.1080p.WEB-DL.x264-RSS",
        "Parity.Partial.S03E01.1080p.WEB-DL.x264-RSS",
    ];
    items.extend(later.iter().map(|t| offer(&e, t)));
    set_feed(&e, &items);
    let r = rss_sync(&e).await;
    assert_eq!(r["grabbed"], 0, "{r}");
    let (_, v, _) = e.json(e.get("/api/v1/rss")).await;
    for t in later.iter().chain(&others) {
        let (s, d) = status(&v, t);
        assert_eq!(s, "rejected", "{t}: {d}");
        assert!(d.contains("not_monitored"), "{t}: {d}");
    }
    e.tick().await;
    assert_eq!(
        e.fake.lock().unwrap().add_fields.len(),
        1,
        "still one download"
    );
}

#[tokio::test]
async fn an_episode_added_to_an_unmonitored_season_is_matched_alone_by_rss() {
    let e = start_exec(ExecOptions::default()).await;
    let t0 = time::OffsetDateTime::now_utc();
    // Imported Slow Horses (S1 E1/E3 owned). The operator left seasons 1 and 2 unmonitored.
    for scope in ["season:tvdb:372264:1", "season:tvdb:372264:2"] {
        let (s, _, text) = e
            .json(
                e.patch(&format!("/api/v1/library/{scope}"))
                    .json(&json!({ "monitored": false })),
            )
            .await;
        assert_eq!(s, 200, "{text}");
    }
    // The default add of the next missing episode, before it is released.
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(
        (v["created"].as_bool(), v["plan"]["state"].as_str()),
        (Some(false), Some("blocked")),
        "{text}"
    );
    let scope = |e: &Exec| {
        let index = e.state.index();
        let s = index.series(arr::ids::SeriesKey::Tvdb(372264)).unwrap();
        let seasons: Vec<(u16, bool)> = e
            .state
            .store
            .blocking(|c| {
                let mut st = c.prepare(
                    "SELECT season, monitored FROM seasons WHERE series_id = 'series:tvdb:372264' ORDER BY season",
                )?;
                Ok(st
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap();
        assert_eq!(
            seasons,
            [(1, false), (2, false)],
            "season flags stay as set"
        );
        s.episodes
            .iter()
            .flatten()
            .filter(|x| x.monitored)
            .map(|x| (x.season, x.number))
            .collect::<Vec<_>>()
    };
    assert_eq!(scope(&e), [(1, 2)]);

    let offer = |e: &Exec, t: &str| item(e, t, vec![(format!("{t}.mkv"), mkv(2))], 1.5, &[5040]);
    let wanted = "Slow.Horses.S01E02.1080p.WEB-DL.x264-RSS";
    let others = [
        "Slow.Horses.S01E04.1080p.WEB-DL.x264-RSS",
        "Slow.Horses.S01E02E04.1080p.WEB-DL.x264-RSS",
        "Slow.Horses.S01.1080p.WEB-DL.x264-RSS",
        "Slow.Horses.S02E01.1080p.WEB-DL.x264-RSS",
    ];
    let mut items = vec![offer(&e, wanted)];
    let wanted_hash = items[0].info_hash.clone().unwrap();
    items.extend(others.iter().map(|t| offer(&e, t)));
    set_feed(&e, &items);
    let r = rss_sync(&e).await;
    assert_eq!(r["grabbed"], 1, "{r}");
    let status = |v: &Value, t: &str| {
        let i = v["recent"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"] == t)
            .unwrap_or_else(|| panic!("{t}: {v}"))
            .clone();
        (
            i["status"].as_str().unwrap().to_string(),
            i["detail"].as_str().unwrap_or("").to_string(),
        )
    };
    let (_, v, _) = e.json(e.get("/api/v1/rss")).await;
    assert_eq!(status(&v, wanted).0, "grabbed");
    for t in others {
        let (s, d) = status(&v, t);
        assert_eq!(s, "rejected", "{t}: {d}");
        assert!(d.contains("not_monitored"), "{t}: {d}");
    }
    e.tick().await;
    {
        let q = e.fake.lock().unwrap();
        assert_eq!(q.add_fields.len(), 1);
        assert!(q.torrents.contains_key(&wanted_hash));
    }
    // The request kept from the add settles on its retry without another grab.
    e.tick_at(t0 + Duration::from_secs(2 * 3600)).await;
    let (_, r, _) = e.json(e.get("/api/v1/requests")).await;
    let req = r["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["target"] == "episode:tvdb:372264:1:2")
        .unwrap()
        .clone();
    assert_eq!(req["state"], "satisfied", "{r}");

    // A metadata refresh keeps every flag; an episode new to the unmonitored season is not
    // monitored and does not match.
    e.mock.lock().unwrap().tvmaze_extra.insert(
        "9001".into(),
        vec![json!({ "season": 1, "number": 7, "name": "Ep 7", "type": "regular", "airstamp": "2025-02-20T02:00:00+00:00", "runtime": 60 })],
    );
    let (s, _, _) = e
        .json(
            e.post("/api/v1/commands")
                .json(&json!({ "name": "refresh_metadata", "media_id": "series:tvdb:372264" })),
        )
        .await;
    assert_eq!(s, 202);
    e.tick().await;
    assert_eq!(scope(&e), [(1, 2)]);
    let index = e.state.index();
    let e7 = index
        .series(arr::ids::SeriesKey::Tvdb(372264))
        .unwrap()
        .episodes
        .iter()
        .flatten()
        .find(|x| x.season == 1 && x.number == 7)
        .cloned()
        .expect("the refresh added S01E07");
    assert!(!e7.monitored);
    let late = "Slow.Horses.S01E07.1080p.WEB-DL.x264-RSS";
    items.push(offer(&e, late));
    set_feed(&e, &items);
    let r = rss_sync(&e).await;
    assert_eq!(r["grabbed"], 0, "{r}");
    let (_, v, _) = e.json(e.get("/api/v1/rss")).await;
    assert_eq!(status(&v, late).0, "rejected");
    assert!(status(&v, late).1.contains("not_monitored"));
    e.tick().await;
    assert_eq!(
        e.fake.lock().unwrap().add_fields.len(),
        1,
        "still one download"
    );
}
