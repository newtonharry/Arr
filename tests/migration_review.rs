//! Independent migration review cases: scope IDs have different kinds, and a metadata
//! provider can renumber an existing season as a supposedly new one. Neither should hide
//! pending work or cause duplicate acquisition. These use only loopback fixtures.
#![recursion_limit = "256"]
mod common;

use arr::ids::SeriesKey;
use common::*;
use serde_json::json;

#[tokio::test]
async fn an_empty_metadata_response_cannot_resolve_a_known_numbering_conflict() {
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        categories: Some(("um-review-movies", "um-review-tv")),
        ..Default::default()
    })
    .await;
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "series:tvdb:372264", "native_handoff_confirmed": true, "search": false
        })))
        .await;
    assert_eq!(status, 201, "{body}");
    let mut renumbered = e
        .state
        .index()
        .series(SeriesKey::Tvdb(372264))
        .unwrap()
        .episodes
        .as_ref()
        .unwrap()[0]
        .clone();
    let known_anchor = renumbered.clone();
    renumbered.season = 3;
    renumbered.number = 1;
    let id = "series:tvdb:372264".parse().unwrap();
    let now = time::OffsetDateTime::now_utc();
    e.state
        .store
        .with(move |c| arr::library::upsert_episodes(c, id, &[renumbered], now))
        .await
        .unwrap();
    e.state.reload_index().await.unwrap();
    assert!(
        e.state
            .index()
            .series(SeriesKey::Tvdb(372264))
            .unwrap()
            .metadata_hold
            .is_some()
    );
    e.state
        .store
        .with(move |c| arr::library::upsert_episodes(c, id, &[], now))
        .await
        .unwrap();
    e.state.reload_index().await.unwrap();
    assert!(
        e.state
            .index()
            .series(SeriesKey::Tvdb(372264))
            .unwrap()
            .metadata_hold
            .is_some(),
        "an empty provider response is not evidence that the numbering disagreement was resolved"
    );
    e.state
        .store
        .with(move |c| arr::library::upsert_episodes(c, id, &[known_anchor], now))
        .await
        .unwrap();
    e.state.reload_index().await.unwrap();
    assert!(
        e.state
            .index()
            .series(SeriesKey::Tvdb(372264))
            .unwrap()
            .metadata_hold
            .is_some(),
        "a partial response missing other imported episodes cannot prove that the numbering conflict was resolved"
    );
}

#[tokio::test]
async fn episode_status_uses_actual_coverage_not_the_original_season_request() {
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        categories: Some(("um-review-movies", "um-review-tv")),
        ..Default::default()
    })
    .await;
    for number in [2, 4] {
        let title = format!("Slow.Horses.S01E{number:02}.1080p.WEB-DL.x264-UM");
        let (_, release) = e.publish(
            &title,
            &title,
            vec![(format!("{title}.mkv"), mkv(2))],
            1.5,
            false,
        );
        e.search_results(&format!("Slow Horses S01E{number:02}"), vec![release]);
    }
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "season:tvdb:372264:1", "native_handoff_confirmed": true
        })))
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
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        categories: Some(("um-review-movies", "um-review-tv")),
        ..Default::default()
    })
    .await;
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "episode:tvdb:372264:1:2",
            "native_handoff_confirmed": true
        })))
        .await;
    assert_eq!(status, 201, "{body}");
    let (_, requests, _) = e.json(e.get("/api/v1/requests")).await;
    assert!(
        requests["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| { r["target"] == "episode:tvdb:372264:1:2" && r["state"] == "active" })
    );
    let (status, season, body) = e.json(e.get("/api/v1/status/season:tvdb:372264:1")).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        season["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| { r["target"] == "episode:tvdb:372264:1:2" && r["state"] == "active" }),
        "a season must include its episode request despite the different ID kinds: {body}"
    );
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "season:tvdb:372264:1", "native_handoff_confirmed": true
        })))
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
            .any(|r| { r["target"] == "season:tvdb:372264:1" && r["state"] == "active" }),
        "the episode should expose its encompassing season request: {body}"
    );
}

#[tokio::test]
async fn season_status_includes_individual_grabs_with_either_series_namespace() {
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        categories: Some(("um-review-movies", "um-review-tv")),
        ..Default::default()
    })
    .await;
    let title = "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM";
    let (_, release) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(2))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E02", vec![release]);
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "episode:tvdb:372264:1:2", "native_handoff_confirmed": true
        })))
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

#[tokio::test]
async fn metadata_must_not_append_known_episodes_renumbered_as_a_new_season() {
    let mut snap = snapshot();
    let native_id = snap["sonarr"]["series"][0]["id"]
        .as_i64()
        .unwrap()
        .to_string();
    let original = &mut snap["sonarr"]["episode_samples"][&native_id][0];
    original["title"] = json!("A distinctive existing episode");
    original["airDateUtc"] = json!("2022-04-01T02:00:00Z");
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        categories: Some(("um-review-movies", "um-review-tv")),
        snapshot: snap,
        ..Default::default()
    })
    .await;
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "series:tvdb:372264",
            "native_handoff_confirmed": true,
            "search": false
        })))
        .await;
    assert_eq!(status, 201, "{body}");
    // The same episode title AND air date at a different season/number is evidence of
    // numbering disagreement. A larger season number alone does not justify appending it.
    e.mock.lock().unwrap().tvmaze_extra.insert(
        "9001".into(),
        vec![
            json!({
                "season": 3, "number": 1, "name": "A distinctive existing episode",
                "type": "regular", "airstamp": "2022-04-01T02:00:00Z", "runtime": 60
            }),
            json!({
                "season": 4, "number": 1, "name": "A genuinely future provider episode",
                "type": "regular", "airstamp": "2027-04-01T02:00:00Z", "runtime": 60
            }),
        ],
    );
    let (status, command, body) = e
        .json(e.post("/api/v1/commands").json(&json!({
            "name": "refresh_metadata", "media_id": "series:tvdb:372264"
        })))
        .await;
    assert!(status == 200 || status == 202, "{body}");
    e.tick().await;
    let (_, result, body) = e
        .json(e.get(&format!(
            "/api/v1/commands/{}",
            command["id"].as_str().unwrap()
        )))
        .await;
    assert_eq!(result["command"]["state"], "completed", "{body}");
    let index = e.state.index();
    let series = index.series(SeriesKey::Tvdb(372264)).unwrap();
    assert!(
        !series
            .episodes
            .iter()
            .flatten()
            .any(|ep| (ep.season == 3 || ep.season == 4) && ep.number == 1),
        "the provider renumbered an existing episode; appending it would create a phantom target: {body}"
    );
}

/// Regression: an MCP replay of an add failed with native_handoff_required AFTER the exact
/// scope had been adopted with the native monitoring ledger checked. The conversational
/// tool cannot confirm a handoff; a repeat in the already confirmed scope must work, while
/// widening it still refuses.
#[tokio::test]
async fn confirmed_scope_replays_without_handoff_but_never_expands_via_sibling_or_parent() {
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        categories: Some(("um-review-movies", "um-review-tv")),
        ..Default::default()
    })
    .await;
    let target = "episode:tvdb:372264:1:2";
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": target, "search": false, "native_handoff_confirmed": true
        })))
        .await;
    assert_eq!(status, 201, "{body}");
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": target, "search": false
        })))
        .await;
    assert_eq!(
        status, 201,
        "A confirmed exact-scope replay must not require the MCP to assert a handoff again: {body}"
    );
    for broader in [
        "episode:tvdb:372264:1:4",
        "season:tvdb:372264:1",
        "series:tvdb:372264",
    ] {
        let (status, response, body) = e
            .json(e.post("/api/v1/library").json(&json!({
                "media_id": broader, "search": false
            })))
            .await;
        assert_eq!(
            status, 409,
            "Widening {broader} needs a fresh native handoff: {body}"
        );
        assert_eq!(
            response["error"]["code"], "native_handoff_required",
            "{body}"
        );
    }
    let (_, wanted, _) = e.json(e.get("/api/v1/wanted")).await;
    let wanted: Vec<&str> = wanted["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["target"].as_str().unwrap())
        .collect();
    assert_eq!(wanted, [target]);
    let (status, _, body) = e.json(e.delete("/api/v1/library/series:tvdb:372264")).await;
    assert_eq!(status, 200, "{body}");
    let (status, response, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": target, "search": false
        })))
        .await;
    assert_eq!(
        status, 409,
        "release must revoke the earlier handoff: {body}"
    );
    assert_eq!(response["error"]["code"], "native_handoff_required");
}

/// A metadata-only/unmonitored registration was not a native handoff. Reusing an
/// adopted row must never turn that two-step registration into permission to acquire.
#[tokio::test]
async fn unmonitored_registration_cannot_stand_in_for_a_confirmed_native_handoff() {
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        categories: Some(("um-review-movies", "um-review-tv")),
        ..Default::default()
    })
    .await;
    for registration in [
        json!({ "media_id": "movie:tmdb:603", "monitored": false, "search": false }),
        json!({ "media_id": "series:tvdb:372264", "monitor": "none", "search": false }),
    ] {
        let (status, _, body) = e.json(e.post("/api/v1/library").json(&registration)).await;
        assert!(status == 200 || status == 201, "{body}");
    }
    for target in ["movie:tmdb:603", "episode:tvdb:372264:1:2"] {
        let (status, response, body) = e
            .json(e.post("/api/v1/library").json(&json!({
                "media_id": target, "search": false
            })))
            .await;
        assert_eq!(
            status, 409,
            "unmonitored registration did not confirm a handoff for {target}: {body}"
        );
        assert_eq!(response["error"]["code"], "native_handoff_required");
    }
    let (_, queue, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    let (_, requests, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(queue["items"], json!([]));
    assert_eq!(requests["items"], json!([]));
}

/// A currently single-episode season is not a season handoff. Promoting it would
/// silently make later metadata episodes acquire here while Sonarr still owns them.
#[tokio::test]
async fn a_single_known_episode_does_not_authorize_its_seasons_future_episodes() {
    let mut snapshot = snapshot();
    snapshot["sonarr"]["episode_samples"]["20"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["seasonNumber"] == 1 && e["episodeNumber"] == 2);
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        categories: Some(("um-review-movies", "um-review-tv")),
        snapshot,
        ..Default::default()
    })
    .await;
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "episode:tvdb:372264:1:2", "search": false,
            "native_handoff_confirmed": true
        })))
        .await;
    assert_eq!(status, 201, "{body}");
    let (status, response, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "season:tvdb:372264:1", "search": false
        })))
        .await;
    assert_eq!(
        status, 409,
        "one known adopted episode is not authority to monitor the whole season: {body}"
    );
    assert_eq!(response["error"]["code"], "native_handoff_required");
    assert!(
        !e.state
            .index()
            .series(SeriesKey::Tvdb(372264))
            .unwrap()
            .seasons
            .iter()
            .find(|s| s.number == 1)
            .unwrap()
            .monitored
    );
}

/// Ownership is not the same as current monitoring. An operator can hand over a
/// whole native title while preserving its disabled flags; a later user request in
/// that confirmed title must work without an assistant inventing a new handoff.
#[tokio::test]
async fn an_explicit_whole_title_handoff_survives_disabled_monitoring_and_reactivation() {
    let mut snapshot = snapshot();
    snapshot["sonarr"]["series"][0]["monitored"] = json!(false);
    let e = start_exec(ExecOptions {
        mode: "coexistence",
        categories: Some(("um-review-movies", "um-review-tv")),
        snapshot,
        ..Default::default()
    })
    .await;
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "series:tvdb:372264", "search": false,
            "preserve_monitoring": true, "native_handoff_confirmed": true
        })))
        .await;
    assert_eq!(status, 201, "{body}");
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "episode:tvdb:372264:1:2", "search": false
        })))
        .await;
    assert_eq!(
        status, 201,
        "the operator already handed over this entire title; current disabled flags are not loss of authority: {body}"
    );
    let (status, _, body) = e
        .json(
            e.patch("/api/v1/library/episode:tvdb:372264:1:2")
                .json(&json!({"monitored": false})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, _, body) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "episode:tvmaze:9001:1:2", "search": false
        })))
        .await;
    assert_eq!(
        status, 201,
        "the same confirmed scope, including its canonical alias, can be reactivated: {body}"
    );
}
