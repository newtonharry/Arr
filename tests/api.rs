//! HTTP-level tests against an in-process server and loopback mocks.
#![recursion_limit = "256"]

mod common;

use arr::{api, error, openapi};
use common::*;
use serde_json::{Value, json};
use std::time::Duration;

async fn body(resp: reqwest::Response) -> (u16, Value, String) {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    let v = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, v, text)
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("<none>")
}

#[tokio::test]
async fn auth_routing_and_schema() {
    let app = start_app(&snapshot(), "", None, false).await;
    let c = &app.client;

    let (s, v, _) = body(
        c.get(format!("{}/api/v1/health", app.base))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((s, v["status"].as_str()), (200, Some("ok")));

    let r = c
        .get(format!("{}/api/v1/readiness", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers().get("www-authenticate").unwrap(), "Bearer");
    assert_eq!(r.headers().get("cache-control").unwrap(), "no-store");
    let (s, v, _) = body(r).await;
    assert_eq!((s, code(&v)), (401, "unauthorized"));

    let (s, v, _) = body(
        c.get(format!("{}/api/v1/readiness", app.base))
            .bearer_auth("wrong-token-wrong-token-wrong")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((s, code(&v)), (401, "unauthorized"));
    let (s, v, _) = body(
        c.get(format!("{}/api/v1/readiness", app.base))
            .header("authorization", format!("Basic {TOKEN}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((s, code(&v)), (401, "unauthorized"));

    let (s, v, _) = body(app.get("/api/v1/readiness").send().await.unwrap()).await;
    assert_eq!(s, 200);
    assert_eq!(v["ready"], true);
    assert_eq!(v["mode"], "shadow");
    // Service readiness is not migration readiness: with execution off every stage is
    // blocked, each for stated configuration reasons.
    let stages = v["migration"]["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 3);
    for st in stages {
        assert_eq!(
            (st["status"].as_str(), st["ready"].as_bool()),
            (Some("blocked"), Some(false)),
            "{st}"
        );
        assert!(
            st["configuration_blockers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b.as_str().unwrap().contains("execution is disabled"))
        );
    }
    assert!(v.get("replacement").is_none());

    let (s, v, _) = body(app.get("/api/v1/nope").send().await.unwrap()).await;
    assert_eq!((s, code(&v)), (404, "not_found"));
    let (s, v, _) = body(
        app.client
            .delete(format!("{}/api/v1/plans", app.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((s, code(&v)), (405, "method_not_allowed"));

    // Schema is public, parses, and documents every route and every error code.
    let (s, doc, _) = body(
        c.get(format!("{}/api/v1/openapi.json", app.base))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(doc, openapi::document());
    for (method, path) in api::ROUTES {
        assert!(
            doc["paths"][path][method].is_object(),
            "{method} {path} undocumented"
        );
    }
    assert_eq!(
        doc["paths"].as_object().unwrap().len(),
        api::ROUTES
            .iter()
            .map(|r| r.1)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
    let codes: Vec<&str> = doc["components"]["schemas"]["ErrorCode"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for c in error::ALL_ERROR_CODES {
        assert!(codes.contains(&serde_json::to_value(c).unwrap().as_str().unwrap()));
    }
    let states = doc["components"]["schemas"]["PlanState"]["enum"]
        .as_array()
        .unwrap();
    assert!(states.contains(&json!("download_started")) && states.contains(&json!("imported")));
    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["ReleaseSearch"]["properties"]["outcome"]["enum"],
        json!(["results", "observed_empty"])
    );
    assert!(
        schemas["ReleaseSearch"]["properties"]
            .get("complete")
            .is_none()
    );
    assert_eq!(
        schemas["Completeness"]["properties"]["provider"]["enum"],
        json!(["unverified", "complete", "incomplete"]),
        "Prowlarr is always unverified; only native indexers report complete/incomplete"
    );
    for (schema, value) in [
        ("UnitStatus", "no_release_observed"),
        ("RejectionCode", "no_release_observed"),
    ] {
        assert!(
            schemas[schema]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!(value)),
            "{schema}"
        );
    }
}

#[tokio::test]
async fn body_and_query_errors_are_typed() {
    let app = start_app(&snapshot(), "", None, false).await;
    let cases: Vec<(reqwest::RequestBuilder, u16, &str)> = vec![
        (
            app.post("/api/v1/plans")
                .header("content-type", "application/json")
                .body("{not json"),
            400,
            "invalid_json",
        ),
        (
            app.post("/api/v1/plans")
                .header("content-type", "text/plain")
                .body("{}"),
            415,
            "unsupported_media_type",
        ),
        (
            app.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "x": 1 })),
            422,
            "invalid_body",
        ),
        (
            app.post("/api/v1/plans").json(&json!({ "media_id": 603 })),
            422,
            "invalid_body",
        ),
        (
            app.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:0603" })),
            400,
            "invalid_media_id",
        ),
        (
            app.post("/api/v1/plans")
                .json(&json!({ "media_id": "series:tvdb:372264" })),
            400,
            "invalid_media_id",
        ),
        (
            app.post("/api/v1/plans")
                .json(&json!({ "media_id": "x".repeat(5000) })),
            413,
            "payload_too_large",
        ),
        (
            app.post("/api/v1/plans")
                .header("idempotency-key", "bad key!")
                .json(&json!({ "media_id": "movie:tmdb:603" })),
            400,
            "invalid_request",
        ),
        (
            app.get("/api/v1/library/movies?limit=0"),
            400,
            "invalid_query",
        ),
        (
            app.get("/api/v1/library/movies?limit=abc"),
            400,
            "invalid_query",
        ),
        (
            app.get("/api/v1/library/movies?unknown=1"),
            400,
            "invalid_query",
        ),
        (app.get("/api/v1/discover?kind=movie"), 400, "invalid_query"),
        (
            app.get("/api/v1/discover?kind=book&q=x"),
            400,
            "invalid_query",
        ),
        (
            app.get(&format!(
                "/api/v1/discover?kind=movie&q={}",
                "a".repeat(101)
            )),
            400,
            "invalid_query",
        ),
        (
            app.get("/api/v1/media/movie:tmdb:-1"),
            400,
            "invalid_media_id",
        ),
        (
            app.get("/api/v1/media/season:tvdb:1:0"),
            400,
            "invalid_media_id",
        ),
        (app.get("/api/v1/plans/pln_notreal"), 404, "not_found"),
        (app.get("/api/v1/plans?state=bogus"), 400, "invalid_query"),
        (
            app.get("/api/v1/media/movie:tmdb:999"),
            404,
            "metadata_unavailable",
        ),
        // Prowlarr is not configured in this app.
        (
            app.get("/api/v1/media/movie:tmdb:603/releases"),
            503,
            "provider_not_configured",
        ),
    ];
    for (req, status, expected) in cases {
        let (s, v, text) = body(req.send().await.unwrap()).await;
        assert_eq!((s, code(&v)), (status, expected), "body: {text}");
        assert!(v["error"]["message"].is_string());
    }
}

#[tokio::test]
async fn catalog_lookup_discovery_and_capabilities() {
    let app = start_app(&snapshot(), "", None, false).await;
    let (_, v, _) = body(
        app.get("/api/v1/library/movies?q=matrix")
            .send()
            .await
            .unwrap(),
    )
    .await;
    let titles: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        ["The Matrix", "The Matrix Reloaded"],
        "exact key match first"
    );
    assert_eq!(v["observation"]["observed_at"], "2026-10-01T12:00:00Z");

    let (_, v, _) = body(
        app.get("/api/v1/library/movies?q=amelie")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        v["items"][0]["title"], "Amélie",
        "accent-insensitive lookup"
    );

    let (_, v, _) = body(app.get("/api/v1/library/series").send().await.unwrap()).await;
    assert_eq!(v["total"], 2);
    assert_eq!(v["items"][1]["episode_samples_present"], true);
    assert!(v["items"][1].get("episodes").is_none());

    let (s, v, _) = body(
        app.get("/api/v1/media/movie:tmdb:603")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(v["target"]["in_catalog"], true);
    assert_eq!(v["target"]["ownership"], "missing");

    let (s, v, _) = body(
        app.get("/api/v1/media/season:tvdb:372264:1")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200);
    let owned: Vec<u64> = v["target"]["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["ownership"] == "owned")
        .map(|e| e["number"].as_u64().unwrap())
        .collect();
    assert_eq!(owned, [1, 3]);
    // Fargo has no episode sample and TVmaze is disabled here: no episode list, explained.
    let (_, v, _) = body(
        app.get("/api/v1/media/season:tvdb:269613:5")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(v["target"]["episodes"].is_null());
    assert!(
        v["target"]["notes"][0]
            .as_str()
            .unwrap()
            .contains("TVmaze is disabled")
    );

    let (_, v, _) = body(
        app.get("/api/v1/discover?kind=movie&q=Leviticus")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["results"].as_array().unwrap().len(), 0);
    let tmdb = v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "plex")
        .unwrap();
    assert_eq!(tmdb["status"], "not_configured");
    // The assistant is told to have the user sign in to Plex, and not to ask for a TMDB key.
    let message = tmdb["message"].as_str().unwrap();
    assert!(
        message.contains("sign in to Plex") && message.contains("do not ask"),
        "{message}"
    );
    assert!(!message.contains("--tmdb-key"), "{message}");

    let (_, v, _) = body(app.get("/api/v1/capabilities").send().await.unwrap()).await;
    let state = |n: &str| {
        v["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == n)
            .unwrap()["state"]
            .clone()
    };
    assert_eq!(state("movie_discovery"), "not_configured");
    assert_eq!(state("release_search"), "not_configured");
    // Implemented but disabled by default (shadow mode); not verified beyond this build's tests.
    assert_eq!(state("download_submission"), "disabled");
    assert_eq!(state("rss_sync"), "disabled");
    assert_eq!(state("download_cleanup"), "disabled");
    assert_eq!(state("quality_upgrades"), "not_implemented");
    assert_eq!(state("rename"), "not_implemented");
    let cap = |n: &str| {
        v["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(cap("download_submission")["implemented"], true);
    assert_eq!(cap("download_submission")["configured"], false);
    assert_eq!(cap("download_submission")["verified"], "loopback_mocks");
    assert_eq!(v["execution_enabled"], false);

    let (_, settings, text) = body(app.get("/api/v1/settings").send().await.unwrap()).await;
    assert_eq!(
        settings["settings"]["profiles"].as_array().unwrap().len(),
        2
    );
    assert_no_leaks(&text);
    let (_, _, report) = body(app.get("/api/v1/migration-report").send().await.unwrap()).await;
    assert_no_leaks(&report);
}

fn matrix_releases() -> Vec<Value> {
    let mut v = vec![
        release("The.Matrix.1999.1080p.WEB-DL.DDP5.1.H.264-GRP", 8.0, 40),
        release("The.Matrix.1999.720p.BluRay.x264-GRP", 5.0, 90),
        release("The.Matrix.1999.REPACK.1080p.WEB-DL.H.264-GRP", 8.1, 3),
        release("The.Matrix.Reloaded.2003.1080p.WEB-DL.H.264", 8.0, 100),
        release("The.Matrix.Trilogy.1999-2003.1080p.WEB-DL", 25.0, 100),
        release("The.Matrix.1999.HDCAM.x264", 1.0, 100),
        release("The.Matrix.1999.2160p.WEB-DL.HEVC", 20.0, 100),
        release("The.Matrix.1999.1080p.BluRay.x264", 10.0, 100),
        release("The.Matrix.1999.FRENCH.1080p.WEB-DL", 8.0, 100),
        release("The.Matrix.1999.MULTi.1080p.WEB-DL", 8.0, 100),
        release("The.Matrix.1999.1080p.WEB-DL.x264-NOSEED", 8.0, 0),
        release("The.Matrix.1999.1080p.WEB-DL.Sample", 0.1, 100),
        release("The.Matrix.1999.KORSUB.1080p.WEBRip", 3.0, 100),
        release("The.Matrix.1999.1080p.WEBRip.x264-HUGE", 20.0, 100),
        release("The.Matrix.1080p.WEB-DL.x264", 8.0, 100),
        release("The.Matrix.2021.1080p.WEB-DL.x264", 8.0, 100),
        release("The.Matrix.1999.1080p.WEB-DL.x264.exe", 0.01, 100),
        release("The.Matrix.1999.S01E01.1080p.WEB-DL", 1.0, 100),
    ];
    let mut no_seed_info = release("The.Matrix.1999.1080p.WEBRip.x264-UNK", 6.0, 0);
    no_seed_info["seeders"] = Value::Null;
    v.push(no_seed_info);
    let mut no_link = release("The.Matrix.1999.1080p.WEBRip.x264-NOLINK", 6.0, 10);
    no_link["downloadUrl"] = Value::Null;
    no_link["magnetUrl"] = Value::Null;
    v.push(no_link);
    let mut foreign_indexer = release("The.Matrix.1999.1080p.WEBRip.x264-OTHER", 6.0, 10);
    foreign_indexer["indexerId"] = json!(7);
    v.push(foreign_indexer);
    let mut usenet = release("The.Matrix.1999.1080p.WEBRip.x264-NZB", 6.0, 10);
    usenet["protocol"] = json!("usenet");
    v.push(usenet);
    v
}

#[tokio::test]
async fn movie_release_decisions_are_explained_and_leak_free() {
    let (addr, mock) = start_mock().await;
    mock.lock()
        .unwrap()
        .releases
        .insert("The Matrix 1999".into(), matrix_releases());
    let app = start_app(&snapshot(), "", Some(addr), false).await;
    let (s, v, text) = body(
        app.get("/api/v1/media/movie:tmdb:603/releases")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200, "{text}");
    assert_no_leaks(&text);
    assert_eq!(v["outcome"], "results");
    // Everything was retrieved, but per-indexer coverage is not observable through Prowlarr.
    assert_eq!(v["completeness"]["retrieval"], "complete");
    assert_eq!(v["completeness"]["provider"], "unverified");
    assert_eq!(v["completeness"]["issues"], json!([]));
    assert_eq!(v["queries"][0]["status"], "complete");
    assert!(v.get("complete").is_none(), "the ambiguous boolean is gone");
    let rel = v["releases"].as_array().unwrap();
    let find = |t: &str| {
        rel.iter()
            .find(|r| r["title"] == t)
            .unwrap_or_else(|| panic!("missing {t}"))
    };
    let codes = |t: &str| -> Vec<String> {
        find(t)["decision"]["rejections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["code"].as_str().unwrap().to_string())
            .collect()
    };

    // The repack outranks the original in the same tier (prefer propers), and both outrank 720p.
    let approved: Vec<&str> = rel
        .iter()
        .filter(|r| r["decision"]["approved"] == true)
        .map(|r| r["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        approved,
        [
            "The.Matrix.1999.REPACK.1080p.WEB-DL.H.264-GRP",
            "The.Matrix.1999.1080p.WEB-DL.DDP5.1.H.264-GRP",
            "The.Matrix.1999.720p.BluRay.x264-GRP",
        ]
    );
    assert_eq!(rel[0]["rank"], 1);
    assert!(rel[0]["release_ref"].as_str().unwrap().starts_with("rel_"));

    for (title, expected) in [
        (
            "The.Matrix.Reloaded.2003.1080p.WEB-DL.H.264",
            "title_mismatch",
        ),
        (
            "The.Matrix.Trilogy.1999-2003.1080p.WEB-DL",
            "multi_movie_bundle",
        ),
        ("The.Matrix.1999.HDCAM.x264", "quality_not_allowed"),
        ("The.Matrix.1999.2160p.WEB-DL.HEVC", "quality_not_allowed"),
        ("The.Matrix.1999.1080p.BluRay.x264", "quality_not_allowed"),
        ("The.Matrix.1999.FRENCH.1080p.WEB-DL", "language_mismatch"),
        (
            "The.Matrix.1999.MULTi.1080p.WEB-DL",
            "language_ambiguous_multi",
        ),
        (
            "The.Matrix.1999.1080p.WEB-DL.x264-NOSEED",
            "below_minimum_seeders",
        ),
        ("The.Matrix.1999.1080p.WEB-DL.Sample", "sample_or_extra"),
        ("The.Matrix.1999.KORSUB.1080p.WEBRip", "hardcoded_subs"),
        (
            "The.Matrix.1999.1080p.WEBRip.x264-HUGE",
            "size_above_maximum",
        ),
        ("The.Matrix.1080p.WEB-DL.x264", "year_missing"),
        ("The.Matrix.2021.1080p.WEB-DL.x264", "year_mismatch"),
        (
            "The.Matrix.1999.1080p.WEB-DL.x264.exe",
            "dangerous_file_type",
        ),
        ("The.Matrix.1999.S01E01.1080p.WEB-DL", "not_a_movie"),
        ("The.Matrix.1999.1080p.WEBRip.x264-UNK", "seeders_unknown"),
        (
            "The.Matrix.1999.1080p.WEBRip.x264-NOLINK",
            "no_download_link",
        ),
        (
            "The.Matrix.1999.1080p.WEBRip.x264-OTHER",
            "indexer_not_configured",
        ),
        (
            "The.Matrix.1999.1080p.WEBRip.x264-NZB",
            "protocol_unsupported",
        ),
    ] {
        assert!(
            codes(title).contains(&expected.to_string()),
            "{title}: expected {expected}, got {:?}",
            codes(title)
        );
        assert_eq!(find(title)["decision"]["approved"], false);
    }
    // Upstream contract: GET only, typed movie categories, configured indexer, header auth.
    let m = mock.lock().unwrap();
    let q = &m.log[0];
    assert!(
        q.contains("type=search")
            && q.contains("indexerIds=1")
            && q.contains("categories=2000")
            && q.contains("categories=2045"),
        "{q}"
    );
    assert!(
        !q.contains("5000"),
        "TV categories must not be used for movies"
    );
    assert!(
        !q.to_lowercase().contains("apikey"),
        "API key must not be in the URL"
    );
    assert_eq!(m.header_keys[0].as_deref(), Some(PROWLARR_KEY));
}

#[tokio::test]
async fn owned_movie_is_already_satisfied_and_unicode_title_matches() {
    let (addr, mock) = start_mock().await;
    mock.lock().unwrap().releases.insert(
        "Amélie 2001".into(),
        vec![
            release("Amelie.2001.1080p.WEB-DL.x264", 6.0, 10),
            release(
                "Le.Fabuleux.Destin.d.Amelie.Poulain.2001.FRENCH.1080p.WEB-DL",
                6.0,
                10,
            ),
        ],
    );
    let app = start_app(&snapshot(), "", Some(addr), false).await;
    let (s, v, text) = body(
        app.get("/api/v1/media/movie:tmdb:194/releases")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200, "{text}");
    let rel = v["releases"].as_array().unwrap();
    // Unmarked release: Radarr semantics assume the original language (French) -> mismatch.
    let unmarked = rel
        .iter()
        .find(|r| r["title"] == "Amelie.2001.1080p.WEB-DL.x264")
        .unwrap();
    assert_eq!(
        unmarked["decision"]["rejections"][0]["code"],
        "language_mismatch"
    );
    assert!(
        unmarked["decision"]["rejections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["code"] != "title_mismatch"),
        "accent-folded title must match"
    );
    let original = rel
        .iter()
        .find(|r| r["title"].as_str().unwrap().starts_with("Le.Fabuleux"))
        .unwrap();
    assert!(
        original["decision"]["rejections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["code"] != "title_mismatch"),
        "original-title alias must match"
    );
    // Both queries (unicode and ASCII-folded) were sent.
    let log = mock.lock().unwrap().log.clone();
    assert!(log.iter().any(|q| q.contains("query=Am%C3%A9lie+2001")));
    assert!(log.iter().any(|q| q.contains("query=Amelie+2001")));

    let (s, v, _) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "movie:tmdb:604" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 201);
    assert_eq!(v["plan"]["state"], "already_satisfied");
    assert_eq!(v["plan"]["units"][0]["status"], "owned");
}

#[tokio::test]
async fn provider_failures_are_typed_and_redacted() {
    let (addr, mock) = start_mock().await;
    mock.lock()
        .unwrap()
        .releases
        .insert("The Matrix 1999".into(), matrix_releases());
    let app = start_app(&snapshot(), "", Some(addr), false).await;
    for (mode, status, expected) in [
        (Mode::Status(401), 502, "upstream_auth"),
        (Mode::Status(403), 502, "upstream_auth"),
        (Mode::Status(429), 503, "upstream_rate_limited"),
        (Mode::Status(500), 502, "upstream_unavailable"),
        (Mode::Status(503), 502, "upstream_unavailable"),
        (Mode::Status(302), 502, "upstream_bad_response"),
        (Mode::Status(400), 502, "upstream_bad_response"),
        (Mode::Malformed, 502, "upstream_bad_response"),
        (Mode::Huge, 502, "upstream_bad_response"),
        (Mode::Delay(Duration::from_secs(4)), 504, "upstream_timeout"),
    ] {
        mock.lock().unwrap().mode = mode.clone();
        let (s, v, text) = body(
            app.get("/api/v1/media/movie:tmdb:603/releases?refresh=true")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!((s, code(&v)), (status, expected), "{mode:?}: {text}");
        assert_eq!(v["error"]["details"]["provider"], "prowlarr");
        assert_no_leaks(&text);
        assert!(!text.contains("127.0.0.1"), "upstream URL leaked: {text}");
    }

    // No results is a success with an explicit outcome, not an error.
    mock.lock().unwrap().mode = Mode::Normal;
    mock.lock().unwrap().releases.clear();
    let (s, v, _) = body(
        app.get("/api/v1/media/movie:tmdb:603/releases?refresh=true")
            .send()
            .await
            .unwrap(),
    )
    .await;
    // HTTP 200 [] is an observation, not proof of absence: Prowlarr can answer this way after
    // an internal indexer failure.
    assert_eq!((s, v["outcome"].as_str()), (200, Some("observed_empty")));
    assert_eq!(v["completeness"]["retrieval"], "complete");
    assert_eq!(v["completeness"]["provider"], "unverified");
    assert!(
        v["uncertainty"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u.as_str().unwrap().contains("not proof"))
    );
    // The same facts carry into the plan: nothing observed is distinct from nothing approved.
    let (s, v, text) = body(
        app.post("/api/v1/plans")
            .json(&json!({ "media_id": "movie:tmdb:603" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 201, "{text}");
    let plan = &v["plan"];
    assert_eq!(plan["state"], "blocked");
    assert_eq!(plan["blockers"][0]["code"], "no_release_observed");
    assert_eq!(plan["units"][0]["status"], "no_release_observed");
    assert!(
        plan["units"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("not proof of absence")
    );
    assert_eq!(plan["search"]["completeness"]["retrieval"], "complete");
    assert_eq!(plan["search"]["completeness"]["provider"], "unverified");
}

#[tokio::test]
async fn outage_and_partial_pagination() {
    // Prowlarr pointing at a closed port: connection failure.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let app = start_app(&snapshot(), "", Some(closed), false).await;
    let (s, v, text) = body(
        app.get("/api/v1/media/movie:tmdb:603/releases")
            .send()
            .await
            .unwrap(),
    )
    .await;
    // Windows retries a refused connection for about two seconds, so it can time out first.
    let refused = (s, code(&v)) == (502, "upstream_unavailable");
    let timed_out = cfg!(windows) && (s, code(&v)) == (504, "upstream_timeout");
    assert!(refused || timed_out, "{text}");
    assert!(
        !text.contains(&closed.port().to_string()),
        "URL leaked: {text}"
    );

    // page_size 2: page 1 succeeds (full), page 2 fails -> partial results, not an error.
    let (addr, mock) = start_mock().await;
    mock.lock()
        .unwrap()
        .releases
        .insert("The Matrix 1999".into(), matrix_releases());
    mock.lock().unwrap().mode = Mode::FailAfterFirstPage(500);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    write_state(&dir, &snapshot(), "", Some(addr), false);
    let cfg = std::fs::read_to_string(dir.join("config.toml"))
        .unwrap()
        .replace("page_size = 100", "page_size = 2");
    std::fs::remove_file(dir.join("config.toml")).unwrap();
    arr::fsutil::write_new_private_file(&dir.join("config.toml"), cfg.as_bytes()).unwrap();
    let base = serve_dir(&dir).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/v1/media/movie:tmdb:603/releases"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    let (s, v, _) = body(resp).await;
    assert_eq!(s, 200);
    assert_eq!(v["outcome"], "results", "observed releases are kept");
    assert_eq!(v["completeness"]["retrieval"], "incomplete");
    assert_eq!(v["completeness"]["issues"][0]["kind"], "page_failed");
    assert_eq!(v["queries"][0]["status"], "partial");
    assert_eq!(v["queries"][0]["error"]["code"], "upstream_unavailable");
    assert_eq!(v["counts"]["unique"], 2);

    // Full pagination: max_pages bounds the number of requests per query.
    mock.lock().unwrap().mode = Mode::Normal;
    mock.lock().unwrap().log.clear();
    let resp = reqwest::Client::new()
        .get(format!(
            "{base}/api/v1/media/movie:tmdb:603/releases?refresh=true"
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    let (_, v, _) = body(resp).await;
    assert_eq!(mock.lock().unwrap().log.len(), 2, "max_pages = 2");
    assert!(mock.lock().unwrap().log[1].contains("offset=2"));
    // A full last page at the page bound means more may exist: retrieval is incomplete, but
    // the valid candidates that were retrieved stay visible.
    assert_eq!(v["queries"][0]["truncated_by_page_limit"], true);
    assert_eq!(v["queries"][0]["status"], "truncated");
    assert_eq!(v["outcome"], "results");
    assert_eq!(v["completeness"]["retrieval"], "incomplete");
    assert_eq!(v["completeness"]["issues"][0]["kind"], "page_limit");
    assert_eq!(v["counts"]["unique"], 4);
    assert!(v["counts"]["approved"].as_u64().unwrap() >= 1);

    // Plans keep the approved candidate and carry the same retrieval facts.
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/plans"))
        .bearer_auth(TOKEN)
        .json(&json!({ "media_id": "movie:tmdb:603" }))
        .send()
        .await
        .unwrap();
    let (s, v, text) = body(resp).await;
    assert_eq!(s, 201, "{text}");
    let plan = &v["plan"];
    assert_eq!(plan["state"], "shadow_planned");
    assert_eq!(plan["search"]["completeness"]["retrieval"], "incomplete");
    assert_eq!(
        plan["search"]["completeness"]["issues"][0]["kind"],
        "page_limit"
    );
    assert!(
        plan["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("retrieval incomplete")),
        "{text}"
    );
}

#[tokio::test]
async fn search_results_are_cached_and_deterministic() {
    let (addr, mock) = start_mock().await;
    let mut rels = matrix_releases();
    rels.push(rels[0].clone()); // duplicate (same info hash) collapses
    mock.lock()
        .unwrap()
        .releases
        .insert("The Matrix 1999".into(), rels);
    let app = start_app(&snapshot(), "", Some(addr), false).await;
    let (_, a, _) = body(
        app.get("/api/v1/media/movie:tmdb:603/releases")
            .send()
            .await
            .unwrap(),
    )
    .await;
    let (_, b, _) = body(
        app.get("/api/v1/media/movie:tmdb:603/releases")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        mock.lock().unwrap().log.len(),
        1,
        "second search served from cache"
    );
    assert_eq!(b["queries"][0]["from_cache"], true);
    assert_eq!(a["releases"], b["releases"], "ordering is deterministic");
    assert_eq!(
        a["counts"]["unique"].as_u64().unwrap() as usize,
        matrix_releases().len()
    );
}

#[tokio::test]
async fn movies_are_off_only_without_plex_and_tmdb() {
    let app = start_app(&snapshot(), "", None, false).await;
    let (s, v, text) = body(
        app.post("/api/v1/quick-add")
            .json(&json!({ "media_id": "movie:tmdb:12345" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(v["outcome"], "movies_off");
    assert_eq!(
        v["message"],
        "Movie search isn't set up yet. The person who runs Arr needs to sign in to Plex again."
    );
    let (_, f, _) = body(app.get("/api/v1/feed").send().await.unwrap()).await;
    assert_eq!(
        (&f["movies_enabled"], &f["tmdb_backup"]),
        (&json!(false), &json!(false))
    );
    let (_, v, _) = body(app.get("/api/v1/readiness").send().await.unwrap()).await;
    assert!(
        v.to_string().contains("Movie search isn't set up"),
        "readiness says movies are off: {v}"
    );
}

#[tokio::test]
async fn tmdb_movie_discovery_and_non_catalog_plan() {
    let (addr, mock) = start_mock().await;
    mock.lock().unwrap().releases.insert(
        "Leviticus 2025".into(),
        vec![
            release("Leviticus.2025.1080p.WEB-DL.DDP5.1.H.264-GRP", 4.0, 25),
            release("Leviticus.2024.1080p.WEB-DL.H.264", 4.0, 25),
        ],
    );
    for (cred, label) in [(TMDB_V3_KEY, "v3 key"), (TMDB_V4_TOKEN, "v4 token")] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let extra = format!(
            "[tmdb]\nenabled = true\nbase_url = \"http://{addr}/tmdb\"\ncredential_file = \"secrets/tmdb_credential\"\n"
        );
        write_state(&dir, &snapshot(), &extra, Some(addr), false);
        arr::fsutil::write_new_private_file(&dir.join("secrets/tmdb_credential"), cred.as_bytes())
            .unwrap();
        let base = serve_dir(&dir).await;
        let c = reqwest::Client::new();
        let get = |p: &str| c.get(format!("{base}{p}")).bearer_auth(TOKEN);

        let (s, v, text) = body(
            get("/api/v1/discover?kind=movie&q=Leviticus")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(s, 200, "{label}: {text}");
        assert_no_leaks(&text);
        assert!(!text.contains(cred), "{label} leaked");
        assert_eq!(v["results"][0]["media_id"], "movie:tmdb:900001");
        assert_eq!(v["results"][0]["in_catalog"], false);
        assert_eq!(v["providers"][1]["status"], "ok");

        let (_, caps, _) = body(get("/api/v1/capabilities").send().await.unwrap()).await;
        let md = caps["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "movie_discovery")
            .unwrap();
        assert_eq!(md["state"], "available");

        let (s, v, text) = body(get("/api/v1/media/movie:tmdb:900001").send().await.unwrap()).await;
        assert_eq!(s, 200, "{text}");
        assert_eq!(v["target"]["metadata_source"], "tmdb");
        assert_eq!(v["target"]["runtime_minutes"], 86);
        let (s, v, _) = body(get("/api/v1/media/movie:tmdb:900002").send().await.unwrap()).await;
        assert_eq!(
            (s, v["error"]["code"].as_str()),
            (404, Some("metadata_unavailable"))
        );

        let (s, v, text) = body(
            c.post(format!("{base}/api/v1/plans"))
                .bearer_auth(TOKEN)
                .json(&json!({ "media_id": "movie:tmdb:900001" }))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(s, 201, "{text}");
        assert!(!text.contains(cred));
        assert_eq!(v["plan"]["state"], "shadow_planned");
        assert_eq!(v["plan"]["profile"], "radarr:6");
        assert_eq!(
            v["plan"]["candidates"][0]["title"],
            "Leviticus.2025.1080p.WEB-DL.DDP5.1.H.264-GRP"
        );
    }

    // Wrong credential: typed upstream auth failure, credential not echoed.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    write_state(
        &dir,
        &snapshot(),
        &format!(
            "[tmdb]\nenabled = true\nbase_url = \"http://{addr}/tmdb\"\ncredential_file = \"secrets/tmdb_credential\"\n"
        ),
        Some(addr),
        false,
    );
    arr::fsutil::write_new_private_file(
        &dir.join("secrets/tmdb_credential"),
        b"WRONG-TMDB-KEY-0001",
    )
    .unwrap();
    let base = serve_dir(&dir).await;
    let c = reqwest::Client::new();
    let (s, v, text) = body(
        c.get(format!("{base}/api/v1/media/movie:tmdb:900001"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (502, Some("upstream_auth")),
        "{text}"
    );
    assert!(!text.contains("WRONG-TMDB-KEY"));
    let (s, v, _) = body(
        c.get(format!("{base}/api/v1/discover?kind=movie&q=Leviticus"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200, "discovery degrades to per-provider failure");
    assert_eq!(v["providers"][1]["status"], "failed");
    assert_eq!(v["providers"][1]["error"]["code"], "upstream_auth");
}

fn rewrite_config(dir: &std::path::Path, from: &str, to: &str) {
    let cfg = std::fs::read_to_string(dir.join("config.toml"))
        .unwrap()
        .replace(from, to);
    std::fs::remove_file(dir.join("config.toml")).unwrap();
    arr::fsutil::write_new_private_file(&dir.join("config.toml"), cfg.as_bytes()).unwrap();
}

#[tokio::test]
async fn no_import_is_reported_not_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    write_state(&dir, &snapshot(), "", None, false);
    std::fs::remove_file(dir.join("active-import.json")).unwrap();
    let base = serve_dir(&dir).await;
    let c = reqwest::Client::new();
    let (s, v, _) = body(
        c.get(format!("{base}/api/v1/readiness"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((s, v["ready"].as_bool()), (503, Some(false)));
    assert_eq!(v["checks"][1]["status"], "missing");
    for path in [
        "/api/v1/library/movies",
        "/api/v1/settings",
        "/api/v1/migration-report",
        "/api/v1/media/movie:tmdb:603/releases",
    ] {
        let (s, v, _) = body(
            c.get(format!("{base}{path}"))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            (s, v["error"]["code"].as_str()),
            (503, Some("catalog_not_imported")),
            "{path}"
        );
    }
    let (s, v, _) = body(
        c.post(format!("{base}/api/v1/plans"))
            .bearer_auth(TOKEN)
            .json(&json!({ "media_id": "movie:tmdb:603" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (503, Some("catalog_not_imported"))
    );
}

#[tokio::test]
async fn concurrency_limit_and_request_deadline() {
    let (addr, mock) = start_mock().await;
    mock.lock()
        .unwrap()
        .releases
        .insert("The Matrix 1999".into(), matrix_releases());
    mock.lock()
        .unwrap()
        .slow_queries
        .push(("The Matrix 1999".into(), Duration::from_millis(1500)));

    // One slot: a second request while the first is in flight is shed immediately.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    write_state(&dir, &snapshot(), "", Some(addr), false);
    rewrite_config(
        &dir,
        "max_body_bytes = 4096",
        "max_body_bytes = 4096\nmax_concurrent_requests = 1",
    );
    let base = serve_dir(&dir).await;
    let slow = {
        let url = format!("{base}/api/v1/media/movie:tmdb:603/releases");
        tokio::spawn(async move {
            reqwest::Client::new()
                .get(url)
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (s, v, _) = body(
        reqwest::Client::new()
            .get(format!("{base}/api/v1/capabilities"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((s, v["error"]["code"].as_str()), (503, Some("overloaded")));
    let (s, _, _) = body(
        reqwest::Client::new()
            .get(format!("{base}/api/v1/health"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(s, 200, "liveness is never shed");
    let (s, _, _) = body(slow.await.unwrap()).await;
    assert_eq!(s, 200, "the in-flight request completes normally");

    // Server deadline shorter than the upstream answer: typed request_timeout.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    write_state(&dir, &snapshot(), "", Some(addr), false);
    rewrite_config(
        &dir,
        "request_timeout_secs = 30",
        "request_timeout_secs = 1",
    );
    let base = serve_dir(&dir).await;
    let (s, v, _) = body(
        reqwest::Client::new()
            .get(format!("{base}/api/v1/media/movie:tmdb:603/releases"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (503, Some("request_timeout"))
    );
}
