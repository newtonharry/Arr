//! A magnet nobody shares is blocklisted after its attempts (an unreachable client is not),
//! the home feed shows a failure once per wanted episode, cards for things nobody wants any
//! more go away, and a failed episode researches its season when that can find a pack.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};
use std::time::Duration;
use time::OffsetDateTime;

const SEASON: &str = "season:tvdb:372264:1";

fn ep(n: u16) -> String {
    format!("episode:tvdb:372264:1:{n}")
}

/// Plans and executes one episode on its own; returns the grab id and the torrent hash.
async fn single(e: &Exec, n: u16, tag: &str, magnet: bool) -> (String, String) {
    let title = format!("Slow.Horses.S01E{n:02}.1080p.WEB-DL.x264-{tag}");
    let (hash, mut r) = e.publish(
        &title,
        &title,
        vec![(format!("{title}.mkv"), mkv(1))],
        1.5,
        magnet,
    );
    // A thin swarm on the indexer: the pack below is the healthier choice.
    r["seeders"] = json!(4);
    e.search_results(&format!("Slow Horses S01E{n:02}"), vec![r]);
    let (s, v, text) = e
        .json(e.post("/api/v1/plans").json(&json!({
            "media_id": ep(n), "execute": true, "replan": true
        })))
        .await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    (id, hash)
}

fn publish_pack(e: &Exec) -> String {
    let name = "Slow.Horses.S01.1080p.WEB-DL.x264-PACK";
    let files: Vec<(String, Vec<u8>)> = (1..=6)
        .map(|n| {
            (
                format!("Slow.Horses.S01E{n:02}.1080p.WEB-DL.x264-PACK.mkv"),
                mkv(1),
            )
        })
        .collect();
    let (hash, r) = e.publish(name, name, files, 12.0, false);
    e.search_results("Slow Horses S01", vec![r]);
    hash
}

async fn blocklist(e: &Exec) -> Vec<Value> {
    let (_, v, _) = e.json(e.get("/api/v1/blocklist")).await;
    v["items"].as_array().unwrap().clone()
}

/// Ticks, two hours apart, until the grab leaves the outbox for good (or 30 passes).
async fn until_failed(e: &Exec, id: &str) {
    let t0 = OffsetDateTime::now_utc();
    for i in 0..30u64 {
        e.tick_at(t0 + Duration::from_secs(i * 2 * 3600)).await;
        if e.grab_row(id).state == "failed" {
            return;
        }
    }
    panic!("never failed: {:?}", e.grab_row(id));
}

/// The search commands waiting to run, as their media ids.
fn pending_searches(e: &Exec) -> Vec<String> {
    e.state
        .store
        .blocking(|c| {
            let mut stmt = c.prepare(
                "SELECT media_id FROM commands WHERE name = 'search' AND state IN ('queued', 'running') ORDER BY media_id",
            )?;
            Ok(stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap()
}

fn drop_commands(e: &Exec) {
    e.state
        .store
        .blocking(|c| {
            c.execute("DELETE FROM commands", [])?;
            Ok(())
        })
        .unwrap();
}

async fn fail_now(e: &Exec, id: &str) {
    let g = e.grab_row(id);
    arr::downloads::fail(
        &e.state,
        &g,
        "stalled",
        "no download progress",
        false,
        OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
}

async fn feed(e: &Exec) -> Value {
    let (s, v, text) = e.json(e.get("/api/v1/feed")).await;
    assert_eq!(s, 200, "{text}");
    v
}

#[tokio::test]
async fn a_magnet_that_never_delivers_metadata_is_blocklisted_and_the_pack_is_found() {
    let e = start_exec(ExecOptions::default()).await;
    e.fake.lock().unwrap().no_metadata = true;
    let (g, _) = single(&e, 4, "DEAD", true).await;
    until_failed(&e, &g).await;

    let row = e.grab_row(&g);
    assert_eq!(row.failure.as_deref(), Some("no_metadata"), "{row:?}");
    assert_eq!(row.attempts, arr::downloads::MAX_SUBMIT_ATTEMPTS);
    assert_eq!(row.retry.as_deref(), Some("research"));
    let list = blocklist(&e).await;
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["reason"], "no_metadata");
    assert_eq!(list[0]["release_ref"], row.release_ref.as_str());

    // The research is a season search (the season has other episodes missing); with the pack
    // on offer it gets the pack, and the dead single is not grabbed again.
    assert_eq!(pending_searches(&e), [SEASON]);
    publish_pack(&e);
    e.fake.lock().unwrap().no_metadata = false;
    e.tick().await;
    let (_, q, _) = e.json(e.get("/api/v1/queue")).await;
    let items = q["items"].as_array().unwrap();
    let live: Vec<&Value> = items.iter().filter(|i| i["active"] == true).collect();
    assert_eq!(live.len(), 1, "{q}");
    assert!(
        live[0]["title"].as_str().unwrap().contains("PACK"),
        "the pack, not the dead single: {q}"
    );
    assert_eq!(e.grab_row(&g).state, "failed");

    // Asking for the dead release again by hand is refused too.
    let (s, v, text) = e
        .json(e.post("/api/v1/plans").json(&json!({
            "media_id": ep(4), "execute": true, "replan": true
        })))
        .await;
    assert!(
        s == 201 || s == 200 || s == 409,
        "{text}: the plan answers, it does not crash"
    );
    let grabs = v["plan"]["execution"]["grabs"].as_array().cloned();
    assert!(
        grabs.is_none_or(|g| g
            .iter()
            .all(|id| e.grab_row(id.as_str().unwrap()).release_ref != row.release_ref)),
        "{text}"
    );
}

#[tokio::test]
async fn an_unreachable_client_is_not_the_releases_fault() {
    let e = start_exec(ExecOptions::default()).await;
    e.fake.lock().unwrap().add_fails = true;
    let (g, _) = single(&e, 4, "OK", false).await;
    until_failed(&e, &g).await;

    let row = e.grab_row(&g);
    assert_eq!(row.failure.as_deref(), Some("submission_failed"), "{row:?}");
    assert_eq!(row.retry.as_deref(), Some("research"));
    assert!(blocklist(&e).await.is_empty(), "nothing is blocklisted");
    assert!(!pending_searches(&e).is_empty(), "researched as before");
}

#[tokio::test]
async fn ten_failures_of_three_episodes_make_three_cards_and_each_retry_works() {
    let e = start_exec(ExecOptions::default()).await;
    let mut ids = vec![];
    for (n, times) in [(2u16, 4), (4, 3), (5, 3)] {
        for i in 0..times {
            let (g, _) = single(&e, n, &format!("T{i}"), false).await;
            fail_now(&e, &g).await;
            ids.push((n, g));
        }
    }
    assert_eq!(ids.len(), 10);
    let v = feed(&e).await;
    let cards = v["needs_you"].as_array().unwrap();
    assert_eq!(cards.len(), 3, "{v}");
    let mut counts: Vec<i64> = cards.iter().map(|c| c["count"].as_i64().unwrap()).collect();
    counts.sort();
    assert_eq!(counts, [3, 3, 4]);
    for c in cards {
        // The newest failure of that episode stands for the rest.
        let n: u16 = c["detail"].as_str().unwrap()[4..5].parse().unwrap();
        let newest = ids.iter().rev().find(|(m, _)| *m == n).unwrap();
        assert_eq!(c["grab_id"], newest.1.as_str(), "{c}");
        assert_eq!(c["problem"], "The download didn't work");
    }
    // The old per-item keys are still there, and every card's retry is accepted.
    drop_commands(&e);
    for c in cards {
        let g = c["grab_id"].as_str().unwrap();
        let (s, _, text) = e.json(e.post(&format!("/api/v1/queue/{g}/retry"))).await;
        assert_eq!(s, 200, "{text}");
    }
    // Three failed episodes of one season, one search for the season.
    assert_eq!(pending_searches(&e), [SEASON]);
}

#[tokio::test]
async fn stopping_the_season_clears_its_failed_cards() {
    let e = start_exec(ExecOptions::default()).await;
    for n in [2u16, 4, 5] {
        let (g, _) = single(&e, n, "S", false).await;
        fail_now(&e, &g).await;
    }
    assert_eq!(feed(&e).await["needs_you"].as_array().unwrap().len(), 3);
    let (s, _, text) = e
        .json(e.post("/api/v1/stop").json(&json!({ "media_id": SEASON })))
        .await;
    assert_eq!(s, 200, "{text}");
    let v = feed(&e).await;
    assert_eq!(v["needs_you"], json!([]), "{v}");
    let (_, a, _) = e.json(e.get("/api/v1/attention")).await;
    assert_eq!(a["items"], json!([]), "{a}");
}

#[tokio::test]
async fn a_lone_missing_episode_keeps_its_own_search() {
    let e = start_exec(ExecOptions::default()).await;
    // Only E04 is still wanted: the rest of the season is stopped (E01 and E03 are owned).
    for n in [2u16, 5, 6] {
        let (s, _, text) = e
            .json(e.post("/api/v1/stop").json(&json!({ "media_id": ep(n) })))
            .await;
        assert_eq!(s, 200, "{text}");
    }
    let (g, _) = single(&e, 4, "ALONE", false).await;
    drop_commands(&e);
    fail_now(&e, &g).await;
    assert_eq!(pending_searches(&e), [ep(4)]);
}
