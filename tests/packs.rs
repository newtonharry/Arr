//! A healthy season pack finishes a season: it leaves out owned episodes and downloads that are
//! making progress, takes over idle, unconfirmed and unsent single-episode downloads (cancelled,
//! not blocklisted), and the engine fetches only the files it was grabbed for.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::json;
use std::time::Duration;
use time::OffsetDateTime;

/// Slow Horses S01: E01 and E03 are owned; E02, E04, E05 and E06 are missing.
const SEASON: &str = "season:tvdb:372264:1";

/// Plans and executes one episode on its own. Returns the grab id and the torrent's hash.
async fn single(e: &Exec, ep: u16, tag: &str) -> (String, String) {
    let title = format!("Slow.Horses.S01E{ep:02}.1080p.WEB-DL.x264-{tag}");
    let (hash, mut r) = e.publish(
        &title,
        &title,
        vec![(format!("{title}.mkv"), mkv(1))],
        1.5,
        false,
    );
    // A thin swarm: the pack below is the healthier choice.
    r["seeders"] = json!(4);
    e.search_results(&format!("Slow Horses S01E{ep:02}"), vec![r]);
    let (s, v, text) = e
        .json(e.post("/api/v1/plans").json(&json!({
            "media_id": format!("episode:tvdb:372264:1:{ep}"), "execute": true, "replan": true
        })))
        .await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    (id, hash)
}

/// The season pack: all six episodes, a healthy swarm.
fn publish_pack(e: &Exec) -> String {
    let name = "Slow.Horses.S01.1080p.WEB-DL.x264-PACK";
    let files: Vec<(String, Vec<u8>)> = (1..=6)
        .map(|n| {
            (
                format!("Slow.Horses.S01E{n:02}.1080p.WEB-DL.x264-PACK.mkv"),
                mkv(1),
            )
        })
        .chain([("Slow.Horses.S01.nfo".to_string(), b"nfo".to_vec())])
        .collect();
    let (hash, r) = e.publish(name, name, files, 12.0, false);
    e.search_results("Slow Horses S01", vec![r]);
    hash
}

fn targets(e: &Exec, grab: &str) -> Vec<String> {
    e.grab_row(grab)
        .targets
        .iter()
        .map(|t| t.target.to_string())
        .collect()
}

fn ep(n: u16) -> String {
    format!("episode:tvdb:372264:1:{n}")
}

async fn blocklisted(e: &Exec) -> usize {
    let (_, v, _) = e.json(e.get("/api/v1/blocklist")).await;
    v["items"].as_array().unwrap().len()
}

/// The download has had no progress for ten minutes (the worker records this from its polls;
/// here the clock is set back instead of waiting).
fn make_idle(e: &Exec, grab: &str) {
    let (g, since) = (
        grab.to_string(),
        arr::import::rfc3339(OffsetDateTime::now_utc() - Duration::from_secs(10 * 60)),
    );
    e.state
        .store
        .blocking(move |c| {
            c.execute(
                "UPDATE grabs SET stalled_since = ?2 WHERE id = ?1",
                rusqlite::params![g, since],
            )?;
            Ok(())
        })
        .unwrap();
}

fn mark(e: &Exec, sql: &'static str, grab: &str) {
    let g = grab.to_string();
    e.state
        .store
        .blocking(move |c| {
            c.execute(sql, [g])?;
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn a_healthy_pack_replaces_idle_unsent_and_dead_singles_and_keeps_what_moves() {
    let e = start_exec(ExecOptions::default()).await;

    // E04 and E05 are sent to the client; E05 then makes progress, E04 does not.
    let (g4, h4) = single(&e, 4, "DEAD").await;
    let (g5, h5) = single(&e, 5, "LIVE").await;
    e.tick().await;
    assert_eq!(e.grab_row(&g4).state, "downloading");
    make_idle(&e, &g4);
    make_idle(&e, &g5);
    e.set_state(&h5, "downloading", 0.4);
    e.tick().await;
    assert!(e.grab_row(&g4).stalled_since.is_some());
    assert!(e.grab_row(&g5).stalled_since.is_none(), "E05 moves");
    // E06 is still waiting in the outbox.
    let (g6, _h6) = single(&e, 6, "WAIT").await;
    assert_eq!(e.grab_row(&g6).state, "queued");

    let pack_hash = publish_pack(&e);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": SEASON, "execute": true, "replan": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let plan = &v["plan"];
    assert_eq!(plan["selection"], "season_pack", "{text}");
    let cand = &plan["candidates"][0];
    assert_eq!(cand["covers_episodes"], json!([2, 4, 6]), "{text}");
    assert_eq!(cand["skips_owned"], json!([1, 3]), "{text}");
    assert_eq!(cand["skips_active"], json!([5]), "{text}");
    assert_eq!(cand["supersedes"], json!([4, 6]), "{text}");
    let pack = plan["execution"]["grabs"][0].as_str().unwrap().to_string();
    let replaced: Vec<&str> = plan["execution"]["superseded_grabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    assert_eq!(replaced.len(), 2, "{text}");
    assert!(replaced.contains(&g4.as_str()) && replaced.contains(&g6.as_str()));

    // One grab for the pack, for exactly the episodes it gets.
    let mut got = targets(&e, &pack);
    got.sort();
    assert_eq!(got, [ep(2), ep(4), ep(6)]);
    // The replaced grabs are cancelled with a reason, and nothing is blocklisted.
    for g in [&g4, &g6] {
        let row = e.grab_row(g);
        assert_eq!((row.state.as_str(), row.active), ("cancelled", false));
        assert!(
            row.cleanup_detail
                .as_deref()
                .unwrap_or_default()
                .contains("replaced by a healthier season pack"),
            "{:?}",
            row.cleanup_detail
        );
    }
    assert_eq!(blocklisted(&e).await, 0);
    // The idle one's torrent is out of the client; the one that moves was left alone.
    let (gone, stays) = {
        let fake = e.fake.lock().unwrap();
        (
            !fake.torrents.contains_key(&h4),
            fake.torrents.contains_key(&h5),
        )
    };
    assert!(gone, "the dead torrent is removed");
    assert!(stays, "the live one stays");
    let live = e.grab_row(&g5);
    assert!((live.state.as_str(), live.active) == ("downloading", true));
    assert_eq!(targets(&e, &g5), [ep(5)]);
    // The history says why.
    let (_, v, _) = e.json(e.get("/api/v1/history?limit=50")).await;
    assert!(
        v.to_string()
            .contains("replaced by a healthier season pack"),
        "{v}"
    );

    // The engine is told which episodes the pack is for.
    e.tick().await;
    let sent = e.fake.lock().unwrap().add_fields.clone();
    let add = sent
        .iter()
        .find(|a| a.hash == pack_hash)
        .expect("the pack reached the client");
    let mut want: Vec<String> = add
        .select
        .clone()
        .expect("a selection")
        .iter()
        .map(|t| t.to_string())
        .collect();
    want.sort();
    assert_eq!(want, [ep(2), ep(4), ep(6)]);
}

#[tokio::test]
async fn a_paused_or_chosen_download_is_never_replaced() {
    let e = start_exec(ExecOptions::default()).await;
    let (g4, _) = single(&e, 4, "PAUSED").await;
    let (g5, _) = single(&e, 5, "CHOSEN").await;
    let (g6, _) = single(&e, 6, "OLD").await;
    e.tick().await;
    for g in [&g4, &g5, &g6] {
        make_idle(&e, g);
    }
    // All three are idle. One is paused by the user, one was picked by hand.
    mark(&e, "UPDATE grabs SET user_paused = 1 WHERE id = ?1", &g4);
    mark(
        &e,
        "UPDATE grabs SET origin = 'interactive' WHERE id = ?1",
        &g5,
    );
    e.state.reload_index().await.unwrap();

    publish_pack(&e);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": SEASON, "execute": true, "replan": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let cand = &v["plan"]["candidates"][0];
    assert_eq!(cand["supersedes"], json!([6]), "{text}");
    assert_eq!(cand["skips_active"], json!([4, 5]), "{text}");
    let pack = v["plan"]["execution"]["grabs"][0].as_str().unwrap();
    let mut got = targets(&e, pack);
    got.sort();
    assert_eq!(got, [ep(2), ep(6)]);
    for g in [&g4, &g5] {
        assert_eq!(e.grab_row(g).state, "downloading", "{g} is kept");
    }
    assert_eq!(e.grab_row(&g6).state, "cancelled");
    assert_eq!(blocklisted(&e).await, 0);
}

#[tokio::test]
async fn a_download_that_started_moving_after_the_plan_is_kept_and_its_episode_left_out() {
    let e = start_exec(ExecOptions::default()).await;
    let (g4, h4) = single(&e, 4, "SLOW").await;
    let (g6, _) = single(&e, 6, "DEAD").await;
    e.tick().await;
    make_idle(&e, &g4);
    make_idle(&e, &g6);
    e.state.reload_index().await.unwrap();

    // A plan is made while both are idle, and not executed yet.
    publish_pack(&e);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": SEASON, "execute": false, "replan": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["plan"]["candidates"][0]["supersedes"], json!([4, 6]));
    let plan_id = v["plan"]["id"].as_str().unwrap().to_string();

    // E04 starts to download before the plan runs.
    e.set_state(&h4, "downloading", 0.3);
    e.tick().await;
    assert!(e.grab_row(&g4).stalled_since.is_none());

    let (s, v, text) = e
        .json(e.post(&format!("/api/v1/plans/{plan_id}/execute")))
        .await;
    assert_eq!(s, 200, "{text}");
    let pack = v["plan"]["execution"]["grabs"][0].as_str().unwrap();
    // The pack goes ahead without E04, which keeps its own download. E02 and E05 were never
    // grabbed, and E06's download is still dead.
    let mut got = targets(&e, pack);
    got.sort();
    assert_eq!(got, [ep(2), ep(5), ep(6)]);
    assert_eq!(e.grab_row(&g4).state, "downloading");
    assert_eq!(e.grab_row(&g6).state, "cancelled");
    assert_eq!(
        v["plan"]["execution"]["superseded_grabs"],
        json!([g6]),
        "{text}"
    );
    assert_eq!(blocklisted(&e).await, 0);
}

#[tokio::test]
async fn a_pack_is_not_planned_when_every_missing_episode_keeps_its_download() {
    let e = start_exec(ExecOptions::default()).await;
    // Three downloads are under way and moving; the fourth episode was picked by hand and
    // is still waiting. None of them may be replaced, so nothing is left for a pack.
    let mut sent = vec![];
    for n in [4, 5, 6] {
        sent.push(single(&e, n, "BUSY").await.1);
    }
    let (chosen, _) = single(&e, 2, "HAND").await;
    mark(
        &e,
        "UPDATE grabs SET origin = 'interactive' WHERE id = ?1",
        &chosen,
    );
    e.tick().await;
    for h in &sent {
        e.set_state(h, "downloading", 0.2);
    }
    e.tick().await;
    publish_pack(&e);
    let (s, v, text) = e
        .json(e.post("/api/v1/plans").json(&json!({
            "media_id": SEASON, "execute": false, "replan": true
        })))
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["plan"]["candidates"], json!([]), "{text}");
    assert_eq!(v["plan"]["state"], "in_progress", "{text}");
}
