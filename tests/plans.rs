//! Shadow-plan scenarios: end-to-end season planning, idempotency, concurrency, restart
//! durability, fail-closed policy variants and the inert download switch.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};

async fn body(resp: reqwest::Response) -> (u16, Value, String) {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::Null),
        text,
    )
}

fn slow_horses_releases(mock: &Mock) {
    let mut m = mock.lock().unwrap();
    m.releases.insert(
        "Slow Horses S01".into(),
        vec![
            release(
                "Slow.Horses.S01.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb",
                12.0,
                50,
            ),
            release(
                "Slow.Horses.S01-S04.1080p.ATVP.WEB-DL.DDP5.1.H.264",
                60.0,
                80,
            ),
            release("Slow.Horses.S01.2160p.ATVP.WEB-DL.DDP5.1.HEVC", 40.0, 30),
            release("Slow.Horses.S01.720p.WEBRip.x264", 4.0, 0),
            release("Slow.Horses.S01E02.1080p.WEB-DL.x264-GRP", 2.5, 20),
        ],
    );
    m.releases.insert(
        "Slow Horses S01E04".into(),
        vec![
            release("Slow.Horses.S01E04.720p.WEBRip.x264-GRP", 1.2, 5),
            release("Slow.Horses.S01E04.1080p.WEB-DL.x264-DEAD", 2.4, 0),
        ],
    );
    m.releases.insert(
        "Slow Horses S01E05".into(),
        vec![
            release("Slow.Horses.S01E05.2160p.WEB-DL.HEVC", 9.0, 50),
            release("Slow.Horses.S01E05.1080p.BluRay.x264", 4.0, 50),
        ],
    );
    m.releases.insert(
        "Slow Horses S01E06".into(),
        vec![release("Slow.Horses.S01E06.1080p.WEB-DL.x264-GRP", 2.6, 9)],
    );
}

#[tokio::test]
async fn slow_horses_season_plan_end_to_end() {
    let (addr, mock) = start_mock().await;
    slow_horses_releases(&mock);
    let app = start_app(&snapshot(), "", Some(addr), false).await;

    // Search result: packs are found but rejected, with explanations.
    let (s, v, text) = body(
        app.get("/api/v1/media/season:tvdb:372264:1/releases")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200, "{text}");
    assert_no_leaks(&text);
    let rel = v["releases"].as_array().unwrap();
    let codes = |t: &str| -> Vec<String> {
        rel.iter().find(|r| r["title"] == t).unwrap()["decision"]["rejections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["code"].as_str().unwrap().to_string())
            .collect()
    };
    let pack = rel
        .iter()
        .find(|r| r["title"] == "Slow.Horses.S01.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb")
        .unwrap();
    assert_eq!(pack["decision"]["owned_overlap"], json!([1, 3]));
    assert_eq!(pack["decision"]["full_season"], true);
    assert_eq!(
        codes("Slow.Horses.S01.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb"),
        ["covers_owned"]
    );
    assert_eq!(
        codes("Slow.Horses.S01-S04.1080p.ATVP.WEB-DL.DDP5.1.H.264"),
        ["multi_season_pack"]
    );
    assert!(
        codes("Slow.Horses.S01.2160p.ATVP.WEB-DL.DDP5.1.HEVC")
            .contains(&"quality_not_allowed".into())
    );
    assert!(codes("Slow.Horses.S01.720p.WEBRip.x264").contains(&"below_minimum_seeders".into()));
    assert_eq!(rel[0]["title"], "Slow.Horses.S01E02.1080p.WEB-DL.x264-GRP");
    assert_eq!(rel[0]["decision"]["approved"], true);
    assert!(
        v["uncertainty"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u.as_str().unwrap().contains("per-episode hasFile"))
    );

    // Plan: packs ineligible (owned E01/E03), missing episodes searched individually.
    let (s, v, text) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "season:tvdb:372264:1" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 201, "{text}");
    assert_no_leaks(&text);
    let plan = &v["plan"];
    assert_eq!(plan["state"], "shadow_partial");
    assert_eq!(plan["selection"], "episodes");
    assert_eq!(plan["execution"]["submitted"], false);
    assert_eq!(plan["execution"]["download_submission"], "disabled");
    let status: Vec<(String, String)> = plan["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["label"].as_str().unwrap().into(),
                u["status"].as_str().unwrap().into(),
            )
        })
        .collect();
    let expect = [
        ("S01E01", "owned"),
        ("S01E02", "planned"),
        ("S01E03", "owned"),
        ("S01E04", "planned"),
        ("S01E05", "no_approved_release"),
        ("S01E06", "planned"),
    ];
    assert_eq!(status, expect.map(|(a, b)| (a.to_string(), b.to_string())));
    assert_eq!(plan["candidates"].as_array().unwrap().len(), 3);
    let e5 = &plan["units"][4]["reason"];
    assert!(
        e5.as_str().unwrap().contains("none approved")
            && e5.as_str().unwrap().contains("quality_not_allowed"),
        "{e5}"
    );
    assert!(plan["notes"].as_array().unwrap().iter().any(|n| {
        n.as_str()
            .unwrap()
            .contains("re-download owned episodes [1, 3]")
    }));
    let counts = &plan["search"]["rejection_counts"];
    assert!(
        counts["covers_owned"].as_u64().unwrap() >= 1
            && counts["multi_season_pack"].as_u64().unwrap() == 1
    );
    // E02 came from the season query, so it was not searched again.
    let log = mock.lock().unwrap().log.clone();
    assert!(!log.iter().any(|q| q.contains("S01E02")), "{log:?}");
    assert!(log.iter().any(|q| q.contains("query=Slow+Horses+S01E05")));
    assert!(
        log.iter()
            .all(|q| q.contains("categories=5000") && !q.contains("categories=2000"))
    );

    // Plans persist and are listable/filterable.
    let id = plan["id"].as_str().unwrap();
    let (s, got, _) = body(
        app.get(&format!("/api/v1/plans/{id}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((s, &got["plan"]["id"]), (200, &plan["id"]));
    let (_, list, _) = body(
        app.get("/api/v1/plans?state=shadow_partial")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list["total"], 1);
    let (_, list, _) = body(
        app.get("/api/v1/plans?state=download_started")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list["total"], 0, "shadow mode never executes a plan");

    // Single owned episode -> already satisfied without searching.
    let (s, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "episode:tvdb:372264:1:3" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["plan"]["state"].as_str()),
        (201, Some("already_satisfied"))
    );
    // Single missing episode with an approved release.
    let (s, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "episode:tvdb:372264:1:6" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["plan"]["state"].as_str()),
        (201, Some("shadow_planned"))
    );
    assert_eq!(v["plan"]["candidates"][0]["covers_episodes"], json!([6]));
    // Episode outside the known list.
    let (_, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "episode:tvdb:372264:1:9" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["plan"]["state"], "blocked");
    assert_eq!(v["plan"]["blockers"][0]["code"], "unknown_episode");
}

#[tokio::test]
async fn idempotency_concurrency_replan_cancel_and_restart() {
    let (addr, mock) = start_mock().await;
    mock.lock().unwrap().releases.insert(
        "The Matrix 1999".into(),
        vec![release(
            "The.Matrix.1999.1080p.WEB-DL.DDP5.1.H.264-GRP",
            8.0,
            40,
        )],
    );
    let app = start_app(&snapshot(), "", Some(addr), false).await;
    let req = json!({ "media_id": "movie:tmdb:603" });

    // Concurrent same-target requests produce exactly one plan.
    let mut tasks = vec![];
    for _ in 0..8 {
        let r = app.post("/api/v1/plans").json(&req);
        tasks.push(tokio::spawn(async move { r.send().await.unwrap() }));
    }
    let mut created = 0;
    let mut ids = std::collections::BTreeSet::new();
    for t in tasks {
        let (s, v, text) = body(t.await.unwrap()).await;
        assert!(s == 201 || s == 200, "{text}");
        created += usize::from(v["created"] == true);
        ids.insert(v["plan"]["id"].as_str().unwrap().to_string());
    }
    assert_eq!(
        (created, ids.len()),
        (1, 1),
        "exactly one plan for one target"
    );
    let first = ids.into_iter().next().unwrap();
    let (_, v, _) = body(app.post("/api/v1/plans").json(&req).send().await.unwrap()).await;
    assert_eq!(
        (v["created"].as_bool(), v["plan"]["id"].as_str()),
        (Some(false), Some(first.as_str()))
    );
    assert_eq!(v["plan"]["state"], "shadow_planned");

    // Idempotency keys: replay returns the original; reuse for another request conflicts.
    let (s, a, _) = body(
        app.post("/api/v1/plans")
            .header("idempotency-key", "k-1")
            .json(&json!({ "media_id": "season:tvdb:372264:2" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 201);
    let (s, b, _) = body(
        app.post("/api/v1/plans")
            .header("idempotency-key", "k-1")
            .json(&json!({ "media_id": "season:tvdb:372264:2" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((s, &b["plan"]["id"]), (200, &a["plan"]["id"]));
    let (s, c, _) = body(
        app.post("/api/v1/plans")
            .header("idempotency-key", "k-1")
            .json(&req)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, c["error"]["code"].as_str()),
        (409, Some("idempotency_key_reused"))
    );

    // Replan supersedes atomically.
    let (s, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "movie:tmdb:603", "replan": true }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 201);
    let second = v["plan"]["id"].as_str().unwrap().to_string();
    assert_ne!(second, first);
    let (_, old, _) = body(
        app.get(&format!("/api/v1/plans/{first}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(old["plan"]["state"], "superseded");
    assert_eq!(old["plan"]["superseded_by"], second.as_str());

    // Restart: a fresh server over the same state directory sees the same plans.
    let base2 = serve_dir(&app.dir).await;
    let c2 = reqwest::Client::new();
    let (s, v, _) = body(
        c2.get(format!("{base2}/api/v1/plans/{second}"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["plan"]["state"].as_str()),
        (200, Some("shadow_planned"))
    );
    let (_, v, _) = body(
        c2.post(format!("{base2}/api/v1/plans"))
            .bearer_auth(TOKEN)
            .json(&req)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (v["created"].as_bool(), v["plan"]["id"].as_str()),
        (Some(false), Some(second.as_str())),
        "active plan survives restart"
    );
    let (_, v, _) = body(
        c2.post(format!("{base2}/api/v1/plans"))
            .bearer_auth(TOKEN)
            .header("idempotency-key", "k-1")
            .json(&json!({ "media_id": "season:tvdb:372264:2" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        v["plan"]["id"], a["plan"]["id"],
        "idempotency survives restart"
    );

    // Cancel (local state only); cancelling twice conflicts; a new plan may then be created.
    let (s, v, _) = body(
        c2.post(format!("{base2}/api/v1/plans/{second}/cancel"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((s, v["plan"]["state"].as_str()), (200, Some("cancelled")));
    let (s, v, _) = body(
        c2.post(format!("{base2}/api/v1/plans/{second}/cancel"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("plan_not_active"))
    );
    let (s, _, _) = body(
        c2.post(format!("{base2}/api/v1/plans"))
            .bearer_auth(TOKEN)
            .json(&req)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 201);
    for f in ["state.sqlite", "state.sqlite-wal", "state.sqlite-shm"] {
        let p = app.dir.join(f);
        if p.exists() {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o077,
                0,
                "{f} must be owner-only"
            );
        }
    }
    let (_, list, _) = body(
        c2.get(format!("{base2}/api/v1/plans?target=movie:tmdb:603"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list["total"], 3, "superseded + cancelled + new");
    let active: Vec<&Value> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["state"] == "shadow_planned")
        .collect();
    assert_eq!(active.len(), 1);
}

#[tokio::test]
async fn unsupported_policy_blocks_plans_fail_closed() {
    let (addr, mock) = start_mock().await;
    mock.lock().unwrap().releases.insert(
        "The Matrix 1999".into(),
        vec![release(
            "The.Matrix.1999.1080p.WEB-DL.DDP5.1.H.264-GRP",
            8.0,
            40,
        )],
    );
    let mut snap = snapshot();
    snap["radarr"]["customformat"] = json!([{ "id": 1, "name": "x265", "specifications": [] }]);
    snap["radarr"]["qualityprofile"][0]["formatItems"] =
        json!([{ "format": 1, "name": "x265", "score": -1000 }]);
    let app = start_app(&snap, "", Some(addr), false).await;

    let (_, caps, _) = body(app.get("/api/v1/capabilities").send().await.unwrap()).await;
    assert_eq!(
        caps["policy_blocking_issues"][0]["code"],
        "custom_format_scoring"
    );

    // The otherwise-perfect release is shown but not approved.
    let (s, v, _) = body(
        app.get("/api/v1/media/movie:tmdb:603/releases")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["releases"][0]["decision"]["approved"], false);
    assert_eq!(
        v["releases"][0]["decision"]["rejections"][0]["code"],
        "policy_unsupported"
    );
    assert_eq!(v["counts"]["approved"], 0);

    let (s, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "movie:tmdb:603" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 201);
    assert_eq!(v["plan"]["state"], "blocked");
    assert_eq!(v["plan"]["blockers"][0]["code"], "policy_unsupported");
    assert!(v["plan"]["candidates"].as_array().unwrap().is_empty());
    // TV is unaffected by a Radarr profile issue.
    let (_, v, _) = body(
        app.get("/api/v1/media/season:tvdb:372264:1/releases")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(v["policy_blockers"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn tag_scoped_indexer_and_ownership_unknown_block() {
    let (addr, _mock) = start_mock().await;
    let mut snap = snapshot();
    snap["sonarr"]["indexer"][0]["tags"] = json!([3]);
    let app = start_app(&snap, "", Some(addr), true).await;
    let (_, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "season:tvdb:269613:5" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["plan"]["state"], "blocked");
    assert_eq!(v["plan"]["blockers"][0]["code"], "policy_unsupported");
    assert!(
        v["plan"]["blockers"][0]["message"]
            .as_str()
            .unwrap()
            .contains("tag_routing")
    );

    // Without the tag: Fargo S04 has 3/11 files but no per-episode sample -> ownership unknown.
    let app = start_app(&snapshot(), "", Some(addr), true).await;
    let (_, v, text) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "season:tvdb:269613:4" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["plan"]["state"], "blocked", "{text}");
    assert_eq!(v["plan"]["blockers"][0]["code"], "ownership_unknown");

    // Stats say "all 11 on disk" and TVmaze lists 11: already satisfied, no search.
    let mut snap = snapshot();
    snap["sonarr"]["series"][1]["seasons"][0]["statistics"] =
        json!({ "episodeFileCount": 11, "episodeCount": 11, "totalEpisodeCount": 11 });
    let app = start_app(&snap, "", Some(addr), true).await;
    let (_, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "season:tvdb:269613:4" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["plan"]["state"], "already_satisfied");
    // Stats say "all 10 on disk" but TVmaze now lists 11: the extra episode's ownership is unknown.
    snap["sonarr"]["series"][1]["seasons"][0]["statistics"] =
        json!({ "episodeFileCount": 10, "episodeCount": 10, "totalEpisodeCount": 10 });
    let app = start_app(&snap, "", Some(addr), true).await;
    let (_, v, text) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "season:tvdb:269613:4" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["plan"]["state"], "blocked", "{text}");
    assert_eq!(v["plan"]["blockers"][0]["code"], "ownership_unknown");
    assert!(
        v["plan"]["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("counts 10 episodes"))
    );
}

#[tokio::test]
async fn execution_switch_requires_client_and_paths_and_shadow_stays_shadow() {
    let (addr, mock) = start_mock().await;
    mock.lock().unwrap().releases.insert(
        "The Matrix 1999".into(),
        vec![release(
            "The.Matrix.1999.1080p.WEB-DL.DDP5.1.H.264-GRP",
            8.0,
            40,
        )],
    );
    // Turning execution on without a client and library paths is a config error.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    write_state(
        &dir,
        &snapshot(),
        "[execution]\nenabled = true\n",
        Some(addr),
        false,
    );
    let err = arr::config::load(&dir.join("config.toml")).unwrap_err();
    assert!(
        format!("{err:#}").contains("requires a [qbittorrent] section"),
        "{err:#}"
    );
    // The legacy name is the same switch.
    write_state(
        &dir,
        &snapshot(),
        "[execution]\ndownload_submission = true\n",
        Some(addr),
        false,
    );
    assert!(arr::config::load(&dir.join("config.toml")).is_err());

    let app = start_app(&snapshot(), "", Some(addr), false).await;
    let (_, caps, _) = body(app.get("/api/v1/capabilities").send().await.unwrap()).await;
    assert_eq!(caps["execution_enabled"], false);
    let (_, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "movie:tmdb:603" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["plan"]["state"], "shadow_planned");
    assert_eq!(v["plan"]["execution"]["submitted"], false);
    assert_eq!(v["plan"]["execution"]["download_submission"], "disabled");
    let id = v["plan"]["id"].as_str().unwrap().to_string();
    // Execution requests are refused with an actionable code; nothing is submitted.
    let (s, v, _) = body(
        app.post(&format!("/api/v1/plans/{id}/execute"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("execution_disabled"))
    );
    let (s, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "movie:tmdb:603", "execute": true }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("execution_disabled"))
    );
    let (s, v, _) = body(
        app.post("/api/v1/commands")
            .json(&json!({ "name": "rss_sync" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("execution_disabled"))
    );
    // Only GET /api/v1/search was ever requested upstream.
    assert!(
        mock.lock()
            .unwrap()
            .log
            .iter()
            .all(|q| q.contains("type=search"))
    );
}

#[tokio::test]
async fn tvmaze_backed_seasons_and_discovery() {
    let (addr, mock) = start_mock().await;
    {
        let mut m = mock.lock().unwrap();
        m.releases.insert(
            "WAR S01".into(),
            vec![
                release("WAR.2026.S01.1080p.WEB-DL.DDP5.1.H.264-GRP", 20.0, 30),
                release("War.2019.S01.1080p.WEB-DL.H.264", 20.0, 30),
                release("Star.Wars.Andor.S01.1080p.WEB-DL", 20.0, 30),
                release("WAR.S01.1080p.WEB-DL.H.264-NOYEAR", 20.0, 30),
            ],
        );
        m.releases.insert(
            "Fargo S05".into(),
            vec![
                release("Fargo.S05.1080p.WEB-DL.DDP5.1.H.264-GRP", 25.0, 60),
                release("Fargo.S05E01.1080p.WEB-DL.H.264", 2.5, 60),
            ],
        );
    }
    let app = start_app(&snapshot(), "", Some(addr), true).await;

    let (_, v, _) = body(
        app.get("/api/v1/discover?kind=series&q=war")
            .send()
            .await
            .unwrap(),
    )
    .await;
    let ids: Vec<&str> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["media_id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"series:tvdb:999001") && ids.contains(&"series:tvmaze:9004"),
        "{ids:?}"
    );
    let (_, v, _) = body(
        app.get("/api/v1/discover?kind=series&q=war&year=2026")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["results"].as_array().unwrap().len(), 1);

    // WAR (2026): not in the catalog; TVmaze lookup (301 same-origin) supplies the episode list.
    let (s, v, text) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "season:tvdb:999001:1" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 201, "{text}");
    let plan = &v["plan"];
    assert_eq!(plan["state"], "shadow_planned", "{text}");
    assert_eq!(plan["selection"], "season_pack");
    assert_eq!(
        plan["profile"], "sonarr:6",
        "sole series profile used for a non-catalog series"
    );
    assert_eq!(
        plan["candidates"][0]["title"],
        "WAR.2026.S01.1080p.WEB-DL.DDP5.1.H.264-GRP"
    );
    let (_, r, _) = body(
        app.get("/api/v1/media/season:tvdb:999001:1/releases")
            .send()
            .await
            .unwrap(),
    )
    .await;
    let get = |t: &str| {
        r["releases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["title"] == t)
            .unwrap()["decision"]
            .clone()
    };
    assert_eq!(
        get("War.2019.S01.1080p.WEB-DL.H.264")["rejections"][0]["code"],
        "year_mismatch"
    );
    assert_eq!(
        get("Star.Wars.Andor.S01.1080p.WEB-DL")["rejections"][0]["code"],
        "title_mismatch"
    );
    // No year in the release: accepted by title, with an explicit note.
    let noyear = get("WAR.S01.1080p.WEB-DL.H.264-NOYEAR");
    assert_eq!(noyear["approved"], true);
    assert!(
        noyear["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("matched by title only"))
    );

    // Fargo S05: catalog says 0 files; TVmaze supplies the 10 episodes; a full pack is planned.
    let (_, v, text) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "season:tvdb:269613:5" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["plan"]["state"], "shadow_planned", "{text}");
    assert_eq!(v["plan"]["selection"], "season_pack");
    assert_eq!(v["plan"]["units"].as_array().unwrap().len(), 10);

    // A tvmaze-namespaced request resolves to the canonical TVDB identity.
    let (_, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "season:tvmaze:9003:1" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        v["created"], false,
        "same canonical target as season:tvdb:999001:1"
    );
    assert_eq!(v["plan"]["target"], "season:tvdb:999001:1");
}

#[tokio::test]
async fn partial_episode_query_failure_is_reported_per_unit() {
    let (addr, mock) = start_mock().await;
    slow_horses_releases(&mock);
    mock.lock()
        .unwrap()
        .failing_queries
        .push("Slow Horses S01E06".into());
    let app = start_app(&snapshot(), "", Some(addr), false).await;
    let (s, v, text) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "season:tvdb:372264:1" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 201, "{text}");
    let plan = &v["plan"];
    assert_eq!(plan["state"], "shadow_partial");
    assert_eq!(plan["search"]["completeness"]["retrieval"], "incomplete");
    assert_eq!(plan["search"]["completeness"]["provider"], "unverified");
    let issues = plan["search"]["completeness"]["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["kind"], "query_failed");
    assert_eq!(issues[0]["query"], "Slow Horses S01E06");
    let unit = |label: &str| {
        plan["units"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["label"] == label)
            .unwrap()["status"]
            .clone()
    };
    assert_eq!(unit("S01E06"), "search_failed");
    assert_eq!(unit("S01E05"), "no_approved_release");
    assert_eq!(unit("S01E04"), "planned");
    let codes: Vec<&str> = plan["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["search_failed", "no_approved_release"]);

    // Everything failing (season query included) is an upstream error and nothing is stored.
    mock.lock().unwrap().mode = Mode::Status(503);
    let (s, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "movie:tmdb:603" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (502, Some("upstream_unavailable"))
    );
    let (_, list, _) = body(
        app.get("/api/v1/plans?target=movie:tmdb:603")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list["total"], 0);
}

#[tokio::test]
async fn episode_search_budget_records_a_partial_plan() {
    let (addr, mock) = start_mock().await;
    slow_horses_releases(&mock);
    {
        let mut m = mock.lock().unwrap();
        for q in [
            "Slow Horses S01E04",
            "Slow Horses S01E05",
            "Slow Horses S01E06",
        ] {
            m.slow_queries
                .push((q.into(), std::time::Duration::from_millis(1900)));
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    write_state(&dir, &snapshot(), "", Some(addr), false);
    // request deadline 4 s -> episode budget 3 s; three 1.9 s queries at concurrency 2 exceed it.
    let cfg = std::fs::read_to_string(dir.join("config.toml"))
        .unwrap()
        .replace("request_timeout_secs = 30", "request_timeout_secs = 4");
    std::fs::remove_file(dir.join("config.toml")).unwrap();
    arr::fsutil::write_new_private_file(&dir.join("config.toml"), cfg.as_bytes()).unwrap();
    let base = serve_dir(&dir).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/plans"))
        .bearer_auth(TOKEN)
        .json(&json!({ "media_id": "season:tvdb:372264:1" }))
        .send()
        .await
        .unwrap();
    let (s, v, text) = body(resp).await;
    assert_eq!(s, 201, "plan recorded within the request deadline: {text}");
    let plan = &v["plan"];
    assert_eq!(
        plan["state"], "shadow_partial",
        "E02 from the season query is still planned"
    );
    let statuses: Vec<&str> = plan["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        [
            "owned",
            "planned",
            "owned",
            "search_failed",
            "search_failed",
            "search_failed"
        ]
    );
    assert!(
        plan["blockers"][0]["message"]
            .as_str()
            .unwrap()
            .contains("time budget")
    );
}

/// WAR (2026), TVDB 467092 / TVmaze 86587 (public identities): 8 episodes, 4 aired, E01 owned.
fn war_snapshot() -> Value {
    let now = time::OffsetDateTime::now_utc();
    let at = |days: i64| arr::import::rfc3339(now + time::Duration::days(days));
    let mut snap = snapshot();
    snap["sonarr"]["series"].as_array_mut().unwrap().push(json!({
        "id": 40, "title": "WAR (2026)", "alternateTitles": [], "year": 2026, "tvdbId": 467092, "tvMazeId": 86587,
        "seriesType": "standard", "runtime": 60, "originalLanguage": { "id": 1, "name": "English" }, "status": "continuing",
        "monitored": true, "qualityProfileId": 6, "path": "/tvshows/WAR (2026)",
        "seasons": [{ "seasonNumber": 1, "monitored": true, "statistics": { "episodeFileCount": 1, "episodeCount": 4, "totalEpisodeCount": 8 } }],
    }));
    let episodes: Vec<Value> = (1..=8i64)
        .map(|n| {
            let aired = if n <= 4 { at(-7 * (5 - n)) } else { at(7 * (n - 4)) };
            json!({ "seasonNumber": 1, "episodeNumber": n, "title": format!("Episode {n}"), "airDateUtc": aired,
                    "runtime": 60, "hasFile": n == 1, "monitored": true })
        })
        .collect();
    snap["sonarr"]["episode_samples"]["40"] = json!(episodes);
    snap
}

#[tokio::test]
async fn native_year_disambiguator_scopes_queries_and_empty_results_stay_honest() {
    let (addr, mock) = start_mock().await;
    let app = start_app(&war_snapshot(), "", Some(addr), false).await;

    let (s, v, text) = body(
        app.get("/api/v1/media/season:tvdb:467092:1/releases")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(v["queries"][0]["query"], "WAR 2026 S01");
    assert_eq!(v["outcome"], "observed_empty");
    assert_eq!(v["completeness"]["retrieval"], "complete");
    assert_eq!(v["completeness"]["provider"], "unverified");

    let (s, v, text) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "season:tvdb:467092:1" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 201, "{text}");
    let plan = &v["plan"];
    assert_eq!(plan["state"], "blocked");
    let statuses: Vec<&str> = plan["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        [
            "owned",
            "no_release_observed",
            "no_release_observed",
            "no_release_observed",
            "not_aired",
            "not_aired",
            "not_aired",
            "not_aired"
        ]
    );
    assert!(plan["candidates"].as_array().unwrap().is_empty());
    assert_eq!(plan["blockers"][0]["code"], "no_release_observed");
    assert!(
        plan["notes"].as_array().unwrap().iter().any(|n| n
            .as_str()
            .unwrap()
            .contains("re-download owned episodes [1]")),
        "{text}"
    );
    assert_eq!(plan["search"]["completeness"]["provider"], "unverified");

    // Every query kept the native disambiguator; only aired, missing episodes were searched.
    let log = mock.lock().unwrap().log.clone();
    assert!(
        log.iter().all(|q| q.contains("query=WAR+2026+S01")),
        "{log:?}"
    );
    for e in ["E02", "E03", "E04"] {
        assert!(
            log.iter()
                .any(|q| q.contains(&format!("query=WAR+2026+S01{e}&"))),
            "{e}: {log:?}"
        );
    }
    assert!(
        !log.iter()
            .any(|q| q.contains("S01E05") || q.contains("S01E01&"))
    );
}

async fn post_plan(base: &str, key: Option<&str>, body_json: Value) -> (u16, Value, String) {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/api/v1/plans"))
        .bearer_auth(TOKEN)
        .json(&body_json);
    if let Some(k) = key {
        req = req.header("idempotency-key", k);
    }
    body(req.send().await.unwrap()).await
}

#[tokio::test]
async fn idempotency_keys_bind_to_existing_active_plans() {
    let (addr, mock) = start_mock().await;
    slow_horses_releases(&mock);
    mock.lock().unwrap().releases.insert(
        "The Matrix 1999".into(),
        vec![release(
            "The.Matrix.1999.1080p.WEB-DL.DDP5.1.H.264-GRP",
            8.0,
            40,
        )],
    );
    let app = start_app(&snapshot(), "", Some(addr), false).await;
    let slow = json!({ "media_id": "season:tvdb:372264:1" });
    let matrix = json!({ "media_id": "movie:tmdb:603" });
    let (_, a, _) = post_plan(&app.base, None, slow.clone()).await;
    let (_, b, _) = post_plan(&app.base, None, matrix.clone()).await;
    let (slow_id, matrix_id) = (a["plan"]["id"].clone(), b["plan"]["id"].clone());

    // A fresh key returning an existing active plan must bind to it.
    let (s, v, _) = post_plan(&app.base, Some("parent-active-key-alias"), slow.clone()).await;
    assert_eq!(
        (s, &v["plan"]["id"], v["created"].as_bool()),
        (200, &slow_id, Some(false))
    );
    let (s, v, _) = post_plan(&app.base, Some("parent-active-key-alias"), matrix.clone()).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("idempotency_key_reused"))
    );
    let (s, v, _) = post_plan(&app.base, Some("parent-active-key-alias"), slow.clone()).await;
    assert_eq!((s, &v["plan"]["id"]), (200, &slow_id));
    // Plain repeats still return the active plan; one active plan per target.
    let (s, v, _) = post_plan(&app.base, None, matrix.clone()).await;
    assert_eq!((s, &v["plan"]["id"]), (200, &matrix_id));

    // Bindings survive restart, superseding and cancellation.
    let base = serve_dir(&app.dir).await;
    let (s, _, _) = post_plan(&base, Some("parent-active-key-alias"), matrix.clone()).await;
    assert_eq!(s, 409, "binding survives restart");
    let replan = json!({ "media_id": "season:tvdb:372264:1", "replan": true });
    let (s, r, _) = post_plan(&base, Some("replan-key"), replan.clone()).await;
    assert_eq!(s, 201);
    let (s, v, _) = post_plan(&base, Some("parent-active-key-alias"), slow.clone()).await;
    assert_eq!(
        (s, &v["plan"]["id"], v["plan"]["state"].as_str()),
        (200, &slow_id, Some("superseded"))
    );
    let new_id = r["plan"]["id"].as_str().unwrap();
    let (s, _, _) = body(
        reqwest::Client::new()
            .post(format!("{base}/api/v1/plans/{new_id}/cancel"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200);
    let (s, v, _) = post_plan(&base, Some("replan-key"), replan.clone()).await;
    assert_eq!((s, v["plan"]["state"].as_str()), (200, Some("cancelled")));
    let (s, _, _) = post_plan(&base, Some("replan-key"), matrix.clone()).await;
    assert_eq!(s, 409);
}

#[tokio::test]
async fn racing_idempotency_keys_bind_exactly_once() {
    let (addr, mock) = start_mock().await;
    slow_horses_releases(&mock);
    {
        let mut m = mock.lock().unwrap();
        m.releases.insert(
            "The Matrix 1999".into(),
            vec![release(
                "The.Matrix.1999.1080p.WEB-DL.DDP5.1.H.264-GRP",
                8.0,
                40,
            )],
        );
        m.releases.insert(
            "Amélie 2001".into(),
            vec![release("Amelie.2001.FRENCH.1080p.WEB-DL", 6.0, 10)],
        );
    }
    let app = start_app(&snapshot(), "", Some(addr), false).await;
    let slow = json!({ "media_id": "season:tvdb:372264:1" });
    let matrix = json!({ "media_id": "movie:tmdb:603" });
    post_plan(&app.base, None, slow.clone()).await;
    post_plan(&app.base, None, matrix.clone()).await;

    // One fresh key raced across two targets that both have active plans: exactly one
    // fingerprint wins the binding; every request for the other target gets 409.
    let mut tasks = vec![];
    for i in 0..8 {
        let base = app.base.clone();
        let req = if i % 2 == 0 {
            slow.clone()
        } else {
            matrix.clone()
        };
        tasks.push(tokio::spawn(async move {
            (req.clone(), post_plan(&base, Some("race-key"), req).await)
        }));
    }
    let mut ok_targets = std::collections::BTreeSet::new();
    let mut conflict_targets = std::collections::BTreeSet::new();
    for t in tasks {
        let (req, (s, v, text)) = t.await.unwrap();
        match s {
            200 => {
                ok_targets.insert(v["plan"]["target"].as_str().unwrap().to_string());
            }
            409 => {
                conflict_targets.insert(req["media_id"].as_str().unwrap().to_string());
            }
            _ => panic!("unexpected {s}: {text}"),
        }
    }
    assert_eq!(ok_targets.len(), 1, "{ok_targets:?}");
    assert_eq!(conflict_targets.len(), 1, "{conflict_targets:?}");
    assert_ne!(ok_targets, conflict_targets);

    // Store::create's racing-existing branch: two fresh keys for one new target, slow upstream
    // so both requests reach create(); both keys end up bound to the single plan.
    mock.lock()
        .unwrap()
        .slow_queries
        .push(("Amélie 2001".into(), std::time::Duration::from_millis(400)));
    let amelie = json!({ "media_id": "movie:tmdb:194" });
    let (a, b) = tokio::join!(
        post_plan(&app.base, Some("k-a"), amelie.clone()),
        post_plan(&app.base, Some("k-b"), amelie.clone())
    );
    let created: Vec<bool> = [&a, &b].iter().map(|r| r.1["created"] == true).collect();
    assert_eq!(created.iter().filter(|c| **c).count(), 1, "{created:?}");
    assert_eq!(a.1["plan"]["id"], b.1["plan"]["id"]);
    // Reuse for a different request first: a replay would itself bind an unbound key.
    for k in ["k-a", "k-b"] {
        let (s, _, _) = post_plan(&app.base, Some(k), slow.clone()).await;
        assert_eq!(s, 409, "{k} is bound to the Amélie request");
        let (s, v, _) = post_plan(&app.base, Some(k), amelie.clone()).await;
        assert_eq!(
            (s, &v["plan"]["id"]),
            (200, &a.1["plan"]["id"]),
            "{k} replays"
        );
    }
}

/// A schema-2 state database written by the previous release, holding a plan bound to an
/// Idempotency-Key with the fingerprint that release computed for the request
/// `{"media_id":"movie:tmdb:603"}`.
fn legacy_state(dir: &std::path::Path, fingerprint: &str) {
    let db = dir.join("state.sqlite");
    arr::fsutil::write_new_private_file(&db, b"").unwrap();
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch(
        r#"
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE plans (id TEXT PRIMARY KEY, target TEXT NOT NULL, state TEXT NOT NULL, active INTEGER NOT NULL CHECK (active IN (0, 1)),
            idempotency_key TEXT UNIQUE, request_fingerprint TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, plan_json TEXT NOT NULL);
        CREATE UNIQUE INDEX plans_one_active_per_target ON plans(target) WHERE active = 1;
        CREATE INDEX plans_created ON plans(created_at, id);
        CREATE TABLE idempotency_keys (key TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, plan_id TEXT NOT NULL REFERENCES plans(id), bound_at TEXT NOT NULL);
        INSERT INTO meta VALUES ('schema_version', '2');
        "#,
    )
    .unwrap();
    let plan = json!({
        "id": "pln_0123456789abcdef01234567", "target": "movie:tmdb:603", "requested_target": "movie:tmdb:603", "title": "The Matrix (1999)",
        "state": "shadow_planned",
        "execution": { "mode": "shadow", "download_submission": "not_implemented", "submitted": false, "note": "shadow plan only" },
        "created_at": "2026-10-20T00:00:00Z", "updated_at": "2026-10-20T00:00:00Z", "import_id": null, "catalog_observed_at": null,
        "profile": "radarr:6", "destination_root": "/movies", "selection": "movie", "units": [], "candidates": [], "blockers": [],
        "search": null, "notes": [], "superseded_by": null,
    });
    c.execute(
        "INSERT INTO plans VALUES ('pln_0123456789abcdef01234567', 'movie:tmdb:603', 'shadow_planned', 1, NULL, NULL, '2026-10-20T00:00:00Z', '2026-10-20T00:00:00Z', ?1)",
        [plan.to_string()],
    )
    .unwrap();
    c.execute(
        "INSERT INTO idempotency_keys VALUES ('legacy-key-1', ?1, 'pln_0123456789abcdef01234567', '2026-10-20T00:00:00Z')",
        [fingerprint],
    )
    .unwrap();
}

#[tokio::test]
async fn legacy_idempotency_keys_replay_after_the_upgrade() {
    let (addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    write_state(&dir, &snapshot(), "", Some(addr), false);
    // The previous release fingerprinted the canonical body {media_id, quality_profile,
    // replan}, serialised in this order.
    let fp = hex_digest(br#"{"media_id":"movie:tmdb:603","quality_profile":null,"replan":false}"#);
    legacy_state(&dir, &fp);
    let base = serve_dir(&dir).await;
    let client = reqwest::Client::new();
    for body in [
        json!({ "media_id": "movie:tmdb:603" }),
        json!({ "media_id": "movie:tmdb:603", "replan": false, "execute": false, "revisions": false }),
    ] {
        let r = client
            .post(format!("{base}/api/v1/plans"))
            .bearer_auth(TOKEN)
            .header("Idempotency-Key", "legacy-key-1")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200, "legacy key replays the old plan");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["plan"]["id"], "pln_0123456789abcdef01234567");
        assert_eq!(
            v["plan"]["execution"]["download_submission"],
            "not_implemented"
        );
    }
    // A different request with the old key still conflicts.
    let r = client
        .post(format!("{base}/api/v1/plans"))
        .bearer_auth(TOKEN)
        .header("Idempotency-Key", "legacy-key-1")
        .json(&json!({ "media_id": "movie:tmdb:603", "replan": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 409);
    assert!(
        mock.lock().unwrap().log.is_empty(),
        "replays never search or execute"
    );
}

fn hex_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}
