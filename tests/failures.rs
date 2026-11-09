//! Failure classification with grace periods, outage safety, cancel cooldown and blocklist,
//! and explicit request retries with backoff.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::json;
use std::time::Duration;
use time::OffsetDateTime;

async fn downloading_episode(e: &Exec, ep: u16, tag: &str) -> (String, String) {
    let title = format!("Slow.Horses.S01E{ep:02}.1080p.WEB-DL.x264-{tag}");
    let (hash, r) = e.publish(
        &title,
        &title,
        vec![(format!("{title}.mkv"), mkv(1))],
        1.5,
        false,
    );
    e.search_results(&format!("Slow Horses S01E{ep:02}"), vec![r]);
    let (s, v, text) = e.json(e.post("/api/v1/plans").json(&json!({ "media_id": format!("episode:tvdb:372264:1:{ep}"), "execute": true, "replan": true }))).await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    e.tick().await;
    assert_eq!(e.grab_row(&id).state, "downloading");
    (id, hash)
}

#[tokio::test]
async fn client_errors_and_external_removal_are_classified_after_grace() {
    let e = start_exec(ExecOptions::default()).await;
    let t0 = OffsetDateTime::now_utc();
    // Persistent client error: failed and blocklisted after the grace period.
    let (id, hash) = downloading_episode(&e, 2, "ERR").await;
    e.set_state(&hash, "error", 0.2);
    e.tick_at(t0).await;
    assert_eq!(e.grab_row(&id).state, "downloading", "within grace");
    let (_, v, _) = e.json(e.get(&format!("/api/v1/queue/{id}"))).await;
    assert_eq!(v["grab"]["status"], "client_error");
    e.tick_at(t0 + Duration::from_secs(31 * 60)).await;
    let g = e.grab_row(&id);
    assert_eq!(
        (g.state.as_str(), g.failure.as_deref()),
        ("failed", Some("client_error"))
    );
    let (_, v, _) = e.json(e.get("/api/v1/blocklist")).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    // The incomplete torrent was stopped.
    assert!(
        e.fake
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|l| l == &format!("pause {hash}"))
    );

    // Removed from the client by someone else: failed (not blocklisted) after the grace period.
    let (id, hash) = downloading_episode(&e, 4, "GONE").await;
    e.fake.lock().unwrap().torrents.remove(&hash);
    e.tick_at(t0 + Duration::from_secs(32 * 60)).await;
    assert_eq!(e.grab_row(&id).state, "downloading");
    e.tick_at(t0 + Duration::from_secs(43 * 60)).await;
    let g = e.grab_row(&id);
    assert_eq!(
        (g.state.as_str(), g.failure.as_deref()),
        ("failed", Some("removed_from_client"))
    );
    let (_, v, _) = e.json(e.get("/api/v1/blocklist")).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 1, "not blocklisted");

    // Category changed by the user: released (no longer managed), nothing touched.
    let (id, hash) = downloading_episode(&e, 5, "MINE").await;
    e.fake
        .lock()
        .unwrap()
        .torrents
        .get_mut(&hash)
        .unwrap()
        .category = "my-own".into();
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!((g.state.as_str(), g.active), ("released", false));
    assert!(e.fake.lock().unwrap().torrents.contains_key(&hash));
}

#[tokio::test]
async fn a_download_without_progress_fails_at_the_stall_timeout_whatever_its_label() {
    let e = start_exec(ExecOptions {
        stall_timeout_minutes: Some(30),
        ..Default::default()
    })
    .await;
    let (id, hash) = downloading_episode(&e, 6, "PEERS").await;
    let t0 = OffsetDateTime::now_utc();
    let at = |m: u64| t0 + Duration::from_secs(m * 60);
    let clock = |e: &Exec| e.grab_row(&id).stalled_since;
    let still = |e: &Exec, why: &str| {
        let g = e.grab_row(&id);
        assert_eq!(g.state, "downloading", "{why}: {:?}", g.last_error);
    };
    // Downloading with peers connected but nothing arriving. qBittorrent labels a forced
    // download `forcedDL` whatever its speed, the embedded engine `downloading` while any
    // peer is connected: neither is a stalled label, and neither is progress.
    e.set_state(&hash, "forcedDL", 0.2);
    e.tick_at(at(0)).await;
    assert_eq!(clock(&e), None, "progress since the first observation");
    e.tick_at(at(1)).await;
    assert!(clock(&e).is_some(), "no progress while downloading");
    e.set_state(&hash, "downloading", 0.2);
    e.tick_at(at(20)).await;
    still(&e, "19 of 30 min");
    // Progress resets the clock.
    e.set_state(&hash, "downloading", 0.21);
    e.tick_at(at(25)).await;
    assert_eq!(clock(&e), None);
    e.tick_at(at(26)).await;
    e.tick_at(at(50)).await;
    still(&e, "24 min since the last progress");
    // Paused by the user: never failed, however long; resuming starts a new clock.
    let (s, _, text) = e.json(e.post(&format!("/api/v1/queue/{id}/pause"))).await;
    assert_eq!(s, 200, "{text}");
    e.tick_at(at(170)).await;
    still(&e, "paused by the user");
    assert_eq!(clock(&e), None);
    let (s, _, text) = e.json(e.post(&format!("/api/v1/queue/{id}/resume"))).await;
    assert_eq!(s, 200, "{text}");
    e.tick_at(at(171)).await;
    assert!(clock(&e).is_some());
    // Checking, moving or queued in the client: not downloading, the clock resets.
    for (i, label) in ["checkingDL", "moving", "queuedDL"].into_iter().enumerate() {
        e.set_state(&hash, label, 0.21);
        e.tick_at(at(200 + 60 * i as u64)).await;
        still(&e, label);
        assert_eq!(clock(&e), None, "{label}");
    }
    e.set_state(&hash, "downloading", 0.21);
    e.tick_at(at(400)).await;
    e.tick_at(at(420)).await;
    still(&e, "20 min observed");
    // Two hours without an answer from the client do not count: 20 min observed before,
    // 9 after is still within the timeout.
    e.fake.lock().unwrap().down = true;
    e.tick_at(at(480)).await;
    e.tick_at(at(540)).await;
    e.fake.lock().unwrap().down = false;
    e.tick_at(at(540)).await;
    still(&e, "the outage is not time without progress");
    e.tick_at(at(549)).await;
    still(&e, "29 min observed");
    // 31 min observed without progress: failed and blocklisted as any stall is.
    e.tick_at(at(551)).await;
    let g = e.grab_row(&id);
    assert_eq!(
        (g.state.as_str(), g.failure.as_deref()),
        ("failed", Some("stalled")),
        "{:?}",
        g.last_error
    );
    let msg = g.last_error.unwrap_or_default();
    assert!(
        msg.contains("no download progress for 31 min (downloading)"),
        "{msg}"
    );
    let (_, v, _) = e.json(e.get("/api/v1/blocklist")).await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["reason"], "stalled");
    // The incomplete torrent was stopped (once by the user earlier, once at the failure).
    let pauses = e
        .fake
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|l| *l == &format!("pause {hash}"))
        .count();
    assert_eq!(pauses, 2);
}

#[tokio::test]
async fn client_outage_changes_nothing() {
    let e = start_exec(ExecOptions::default()).await;
    let (id, hash) = downloading_episode(&e, 2, "OUT").await;
    // The last answer saw it downloading without progress: its stall clock runs. It stalls,
    // still without progress.
    let clock = e.grab_row(&id).stalled_since;
    assert!(clock.is_some());
    let progress = e.grab_row(&id).progress.unwrap();
    e.set_state(&hash, "stalledDL", progress);
    e.fake.lock().unwrap().down = true;
    let t0 = OffsetDateTime::now_utc();
    for h in [1u64, 5, 14, 30] {
        e.tick_at(t0 + Duration::from_secs(h * 3600)).await;
    }
    let g = e.grab_row(&id);
    assert_eq!(
        g.state, "downloading",
        "no absence or stall inferred without answers"
    );
    assert!(g.absent_since.is_none() && g.stalled_since == clock);
    let (_, v, _) = e.json(e.get("/api/v1/system/tasks")).await;
    let dl = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "downloads")
        .unwrap()
        .clone();
    assert!(
        dl["last_result"]["poll"]["client_error"]
            .as_str()
            .unwrap()
            .contains("unavailable"),
        "{dl}"
    );
    // Answers again after 30 h, still without progress: those hours were not observed, so
    // they are not time without progress (the 12 h timeout is far from reached).
    e.fake.lock().unwrap().down = false;
    e.tick_at(t0 + Duration::from_secs(30 * 3600)).await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "downloading", "{:?}", g.last_error);
    assert_eq!(g.client_state.as_deref(), Some("stalled"));
}

#[tokio::test]
async fn cancel_blocklists_on_request_and_holds_automatic_searches() {
    let e = start_exec(ExecOptions::default()).await;
    let (id, hash) = downloading_episode(&e, 2, "CXL").await;
    let (s, v, _) = e
        .json(
            e.post(&format!("/api/v1/queue/{id}/cancel"))
                .json(&json!({ "blocklist": true })),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["grab"]["state"], "cancelled");
    assert!(
        e.fake.lock().unwrap().torrents.contains_key(&hash),
        "kept by default"
    );
    assert_eq!(
        e.fake.lock().unwrap().torrents[&hash].state,
        "pausedDL",
        "incomplete torrent stopped"
    );
    let (_, v, _) = e.json(e.get("/api/v1/blocklist")).await;
    let entry = v["items"][0].clone();
    assert_eq!(entry["reason"], "cancelled");
    // Cooldown: an explicit backlog search skips the held episode.
    let (_, v, _) = e
        .json(e.get("/api/v1/wanted?media_id=episode:tvdb:372264:1:2"))
        .await;
    assert!(v["items"][0]["next_search_at"].is_string(), "{v}");
    let before = e.mock.lock().unwrap().log.len();
    let (_, c, _) = e
        .json(
            e.post("/api/v1/commands")
                .json(&json!({ "name": "search_missing", "media_id": "series:tvdb:372264" })),
        )
        .await;
    e.tick().await;
    let queries: Vec<String> = e.mock.lock().unwrap().log[before..].to_vec();
    assert!(!queries.iter().any(|q| q.contains("S01E02")), "{queries:?}");
    let _ = c;
    // The blocklisted release is never grabbed again; removing the entry allows it.
    let (_, v, _) = e
        .json(e.post("/api/v1/plans").json(
            &json!({ "media_id": "episode:tvdb:372264:1:2", "execute": true, "replan": true }),
        ))
        .await;
    assert!(
        v["plan"]["execution"]["grabs"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{v}"
    );
    let (s, _, _) = e
        .json(e.delete(&format!("/api/v1/blocklist/{}", entry["id"])))
        .await;
    assert_eq!(s, 200);
    // The old torrent is still in the client (kept): a new grab of the same hash is refused
    // as foreign-free duplicate only if active; it was cancelled, so it reconciles.
    let (_, v, _) = e
        .json(e.post("/api/v1/plans").json(
            &json!({ "media_id": "episode:tvdb:372264:1:2", "execute": true, "replan": true }),
        ))
        .await;
    assert_eq!(
        v["plan"]["execution"]["grabs"].as_array().unwrap().len(),
        1,
        "{v}"
    );
}

#[tokio::test]
async fn explicit_requests_retry_with_backoff_until_satisfied() {
    let e = start_exec(ExecOptions::default()).await;
    let t0 = OffsetDateTime::now_utc();
    // Nothing available yet.
    let (s, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:6", "execute": true })),
        )
        .await;
    assert_eq!(s, 201);
    assert_eq!(v["plan"]["state"], "blocked");
    let (_, v, _) = e.json(e.get("/api/v1/requests")).await;
    let req = v["items"][0].clone();
    assert_eq!(
        (
            req["target"].as_str(),
            req["state"].as_str(),
            req["attempts"].as_i64()
        ),
        (Some("episode:tvdb:372264:1:6"), Some("active"), Some(1))
    );
    // Not retried before its time; retried after (still nothing), with a longer backoff.
    e.tick_at(t0 + Duration::from_secs(30 * 60)).await;
    let (_, v, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(v["items"][0]["attempts"], 1);
    e.tick_at(t0 + Duration::from_secs(61 * 60)).await;
    let (_, v, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(v["items"][0]["attempts"], 2);
    let next = v["items"][0]["next_attempt_at"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        next.as_str() > arr::import::rfc3339(t0 + Duration::from_secs(61 * 60 + 110 * 60)).as_str(),
        "doubling backoff: {next}"
    );
    // An indexer outage defers without counting an attempt.
    e.mock.lock().unwrap().mode = Mode::Status(503);
    e.tick_at(t0 + Duration::from_secs(4 * 3600)).await;
    let (_, v, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(v["items"][0]["attempts"], 2);
    e.mock.lock().unwrap().mode = Mode::Normal;
    // A release appears: the next retry grabs it and the request is satisfied.
    let title = "Slow.Horses.S01E06.1080p.WEB-DL.x264-LATE";
    let (_, r) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(1))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E06", vec![r]);
    e.tick_at(t0 + Duration::from_secs(6 * 3600)).await;
    let (_, v, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(v["items"][0]["state"], "satisfied", "{v}");
    let (_, q, _) = e.json(e.get("/api/v1/queue")).await;
    assert!(
        q["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["title"] == title)
    );
}

#[tokio::test]
async fn leased_commands_are_requeued_after_restart_and_removal_waits_for_downloads() {
    let mut e = start_exec(ExecOptions::default()).await;
    // A command left running by a dead instance.
    let started = arr::import::rfc3339(OffsetDateTime::now_utc() - Duration::from_secs(60));
    e.state
        .store
        .blocking(move |c| {
            Ok(c.execute(
                "INSERT INTO commands (id, name, media_id, state, created_at, started_at, lease_owner, lease_until, attempts)
                 VALUES ('cmd_000000000000000000000001', 'rss_sync', NULL, 'running', ?1, ?1, 'w_dead', ?1, 0)",
                [started],
            )?)
        })
        .unwrap();
    let recovery = e.restart().await;
    assert_eq!(recovery["commands_requeued"], 1, "{recovery}");
    e.tick().await;
    let (_, v, _) = e
        .json(e.get("/api/v1/commands/cmd_000000000000000000000001"))
        .await;
    assert_eq!(
        (
            v["command"]["state"].as_str(),
            v["command"]["attempts"].as_i64()
        ),
        (Some("completed"), Some(1)),
        "{v}"
    );

    // Removing an item from management waits for its active downloads.
    let (id, _) = downloading_episode(&e, 2, "BUSY").await;
    let (s, v, _) = e.json(e.delete("/api/v1/library/series:tvdb:372264")).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("active_downloads"))
    );
    e.json(e.post(&format!("/api/v1/queue/{id}/cancel"))).await;
    let (s, _, _) = e.json(e.delete("/api/v1/library/series:tvdb:372264")).await;
    assert_eq!(s, 200);
}

// ------------------------------------------------------------------ races with the monitored-only gate
// (the general parts of the coexistence suite's adoption races)

#[tokio::test]
async fn a_release_sent_during_an_add_is_ordered_after_it_and_refused_by_its_download() {
    let e = start_exec(ExecOptions::default()).await;
    let title = "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM";
    let (_, ep2) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E02", vec![ep2]);
    // A request for a monitored episode, held in its automatic search by a slow indexer.
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
            .any(|q| q.contains("q=Slow+Horses+S01E02"))
    };
    for _ in 0..1000 {
        if in_search() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(in_search(), "the add reached its search");

    // The removal cannot fall between the add's checks and its changes: it runs after the
    // add, whose download it then finds active.
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
}

#[tokio::test]
async fn a_release_committed_while_a_torrent_is_fetched_records_and_submits_nothing() {
    let e = start_exec(ExecOptions::default()).await;
    let title = "The.Matrix.1999.1080p.WEB-DL.x264-UM";
    let (hash, rel) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        0.05,
        false,
    );
    e.search_results("The Matrix 1999", vec![rel]);
    let (_, v, text) = e.json(e.get("/api/v1/media/movie:tmdb:603/releases")).await;
    let release_ref = v["releases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["decision"]["approved"] == true)
        .unwrap_or_else(|| panic!("an approved release: {text}"))["release_ref"]
        .clone();
    let grab = json!({ "media_id": "movie:tmdb:603", "release_ref": release_ref });

    // The grab passes its check, then its torrent fetch is held open.
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
    // Nothing is recorded yet, so the removal succeeds while the fetch is pending.
    let (s, _, text) = e.json(e.delete("/api/v1/library/movie:tmdb:603")).await;
    assert_eq!(s, 200, "{text}");
    gate.notify_one();
    let r = pending.await.unwrap().unwrap();
    let status = r.status().as_u16();
    let v: serde_json::Value = r.json().await.unwrap();
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
    assert!(e.fake.lock().unwrap().add_fields.is_empty());
    let (_, q, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"], json!([]), "{q}");

    // Adding the movie again, the same release is grabbed once.
    e.mock.lock().unwrap().download_gate = None;
    e.add("movie:tmdb:603").await;
    let (s, _, text) = e.json(e.post("/api/v1/grabs").json(&grab)).await;
    assert_eq!(s, 201, "{text}");
    e.tick().await;
    assert_eq!(e.fake.lock().unwrap().add_fields.len(), 1);
    assert_eq!(e.fake.lock().unwrap().torrents[&hash].category, "um-movies");
}

#[tokio::test]
async fn an_unmonitored_title_is_never_acquired_by_any_path() {
    let e = start_exec(ExecOptions::default()).await;
    let title = "The.Matrix.1999.1080p.WEB-DL.x264-UM";
    let (_, rel) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        0.05,
        false,
    );
    e.search_results("The Matrix 1999", vec![rel]);
    let (s, _, text) = e
        .json(
            e.patch("/api/v1/library/movie:tmdb:603")
                .json(&json!({ "monitored": false })),
        )
        .await;
    assert_eq!(s, 200, "{text}");
    let (_, v, text) = e.json(e.get("/api/v1/media/movie:tmdb:603/releases")).await;
    let release_ref = v["releases"][0]["release_ref"].clone();
    for (path, body) in [
        (
            "/api/v1/plans",
            json!({ "media_id": "movie:tmdb:603", "execute": true }),
        ),
        (
            "/api/v1/grabs",
            json!({ "media_id": "movie:tmdb:603", "release_ref": release_ref }),
        ),
    ] {
        let (s, v, text) = e.json(e.post(path).json(&body)).await;
        let refused = s == 409 || v["plan"]["execution"]["grabs"] == json!([]);
        assert!(refused, "{path}: {s} {text}");
    }
    e.tick().await;
    assert!(e.fake.lock().unwrap().add_fields.is_empty(), "{text}");
    let (_, q, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"], json!([]), "{q}");
}
