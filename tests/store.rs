//! Store-level idempotency bindings and the schema 1 -> 3 and 2 -> 3 migrations.

use arr::plan::Plan;
use arr::search::Retrieval;
use arr::store::{CreateOutcome, Store};
use serde_json::{Value, json};
use std::path::Path;

fn plan_json(id: &str, target: &str, state: &str) -> Value {
    json!({
        "id": id, "target": target, "requested_target": target, "title": "t", "state": state,
        "execution": { "mode": "shadow", "download_submission": "not_implemented", "submitted": false, "note": "n" },
        "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z",
        "import_id": null, "catalog_observed_at": null, "profile": "radarr:6", "destination_root": null,
        "selection": null, "units": [], "candidates": [], "blockers": [], "search": null, "notes": [],
        "superseded_by": null,
    })
}

fn plan(id: &str, target: &str) -> Plan {
    serde_json::from_value(plan_json(id, target, "blocked")).unwrap()
}

fn key(k: &str, fp: &str) -> Option<(String, String)> {
    Some((k.into(), fp.into()))
}

fn existing_id(o: CreateOutcome) -> String {
    match o {
        CreateOutcome::Existing(p) => p.id,
        CreateOutcome::Created(_) => panic!("expected an existing plan, got a new one"),
        CreateOutcome::IdempotencyConflict => panic!("expected an existing plan, got a conflict"),
    }
}

#[tokio::test]
async fn keys_bind_to_whichever_plan_is_returned_and_persist() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.sqlite");
    let store = Store::open(&db).unwrap();
    let t = "movie:tmdb:603";
    assert!(matches!(
        store
            .create(plan("pln_a", t), key("k1", "fp1"), false)
            .await
            .unwrap(),
        CreateOutcome::Created(_)
    ));
    // Racing-existing branch: a second plan for the same target returns the first and binds k2.
    let out = store
        .create(plan("pln_b", t), key("k2", "fp2"), false)
        .await
        .unwrap();
    assert_eq!(existing_id(out), "pln_a");
    let (p, fp) = store
        .by_idempotency_key("k2".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((p.id.as_str(), fp.as_str()), ("pln_a", "fp2"));
    assert!(matches!(
        store
            .create(plan("pln_c", "movie:tmdb:604"), key("k2", "other"), false)
            .await
            .unwrap(),
        CreateOutcome::IdempotencyConflict
    ));
    // Existing-active fast path binds; with nothing active nothing is bound.
    let out = store
        .existing_active(t.into(), key("k3", "fp3"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(existing_id(out), "pln_a");
    assert!(
        store
            .existing_active("movie:tmdb:999".into(), key("k4", "fp4"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .by_idempotency_key("k4".into())
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store
            .existing_active(t.into(), key("k3", "different"))
            .await
            .unwrap(),
        Some(CreateOutcome::IdempotencyConflict)
    ));

    // Reopen: bindings and the one-active-per-target rule persist; cancelled plans stay bound.
    drop(store);
    let store = Store::open(&db).unwrap();
    for (k, fp) in [("k1", "fp1"), ("k2", "fp2"), ("k3", "fp3")] {
        let (p, got) = store.by_idempotency_key(k.into()).await.unwrap().unwrap();
        assert_eq!((p.id.as_str(), got.as_str()), ("pln_a", fp));
    }
    store
        .cancel("pln_a".into(), "2026-10-02T00:00:00Z".into())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let (p, _) = store
        .by_idempotency_key("k2".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.state.as_str(), "cancelled");
    assert!(matches!(
        store
            .create(plan("pln_d", t), key("k5", "fp5"), false)
            .await
            .unwrap(),
        CreateOutcome::Created(_)
    ));
}

const SCHEMA_V1: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE plans (
    id TEXT PRIMARY KEY, target TEXT NOT NULL, state TEXT NOT NULL,
    active INTEGER NOT NULL CHECK (active IN (0, 1)), idempotency_key TEXT UNIQUE,
    request_fingerprint TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, plan_json TEXT NOT NULL
);
CREATE UNIQUE INDEX plans_one_active_per_target ON plans(target) WHERE active = 1;
CREATE INDEX plans_created ON plans(created_at, id);
INSERT INTO meta (key, value) VALUES ('schema_version', '1');
"#;

fn write_v1(db: &Path) {
    arr::fsutil::write_new_private_file(db, b"").unwrap();
    let c = rusqlite::Connection::open(db).unwrap();
    c.execute_batch(SCHEMA_V1).unwrap();
    // A plan as an earlier build stored it: `search.complete` and query status "ok".
    let mut old = plan_json("pln_legacy", "season:tvdb:372264:1", "shadow_partial");
    old["search"] = json!({
        "queries": [{ "query": "Slow Horses S01", "categories": [5000], "indexer_ids": [1], "status": "ok", "error": null,
                      "pages_fetched": 1, "raw_results": 16, "unique_results": 16, "truncated_by_page_limit": false,
                      "from_cache": false, "fetched_at": "2026-10-01T00:00:00Z" }],
        "complete": true, "releases_considered": 16, "approved": 0, "rejection_counts": { "covers_owned": 14 },
    });
    c.execute(
        "INSERT INTO plans VALUES ('pln_legacy', 'season:tvdb:372264:1', 'shadow_partial', 1, 'legacy-key', 'legacy-fp', '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z', ?1)",
        [old.to_string()],
    )
    .unwrap();
    c.execute(
        "INSERT INTO plans VALUES ('pln_plain', 'movie:tmdb:603', 'blocked', 1, NULL, NULL, '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z', ?1)",
        [plan_json("pln_plain", "movie:tmdb:603", "blocked").to_string()],
    )
    .unwrap();
}

#[tokio::test]
async fn schema_1_database_migrates_without_losing_plans_or_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.sqlite");
    write_v1(&db);
    let store = Store::open(&db).unwrap();

    let (p, fp) = store
        .by_idempotency_key("legacy-key".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((p.id.as_str(), fp.as_str()), ("pln_legacy", "legacy-fp"));
    let search = p.search.unwrap();
    assert_eq!(
        search.completeness.retrieval,
        Retrieval::Unknown,
        "old `complete: true` is not reinterpreted"
    );
    assert_eq!(search.queries[0].status, "ok");
    assert!(store.get("pln_plain".into()).await.unwrap().is_some());
    let (all, total) = store.list(None, None, 100, 0).await.unwrap();
    assert_eq!((all.len(), total), (2, 2));

    // Old key keeps its fingerprint; one active plan per target still holds.
    assert!(matches!(
        store
            .create(
                plan("pln_x", "movie:tmdb:604"),
                key("legacy-key", "other"),
                false
            )
            .await
            .unwrap(),
        CreateOutcome::IdempotencyConflict
    ));
    let out = store
        .create(plan("pln_y", "movie:tmdb:603"), key("new-key", "fp"), false)
        .await
        .unwrap();
    assert_eq!(existing_id(out), "pln_plain");

    drop(store);
    let c = rusqlite::Connection::open(&db).unwrap();
    let version: String = c
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, "3");
    for table in [
        "media",
        "grabs",
        "import_journal",
        "blocklist",
        "rss_seen",
        "requests",
        "commands",
    ] {
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "{table} created by the migration");
    }
    // Re-opening an already migrated database is a no-op.
    let store = Store::open(&db).unwrap();
    assert!(
        store
            .by_idempotency_key("new-key".into())
            .await
            .unwrap()
            .is_some()
    );
}

/// A schema 2 database as the previous release wrote it (plans + key bindings only).
fn write_v2(db: &Path) {
    arr::fsutil::write_new_private_file(db, b"").unwrap();
    let c = rusqlite::Connection::open(db).unwrap();
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
    c.execute(
        "INSERT INTO plans VALUES ('pln_v2', 'movie:tmdb:603', 'shadow_planned', 1, NULL, NULL, '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z', ?1)",
        [plan_json("pln_v2", "movie:tmdb:603", "shadow_planned").to_string()],
    )
    .unwrap();
    c.execute(
        "INSERT INTO idempotency_keys VALUES ('v2-key', 'v2-fp', 'pln_v2', '2026-10-01T00:00:00Z')",
        [],
    )
    .unwrap();
}

#[tokio::test]
async fn schema_2_database_migrates_to_3_keeping_plans_and_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.sqlite");
    write_v2(&db);
    let store = Store::open(&db).unwrap();
    let (p, fp) = store
        .by_idempotency_key("v2-key".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((p.id.as_str(), fp.as_str()), ("pln_v2", "v2-fp"));
    // Plans recorded by earlier builds keep their execution record ("not_implemented").
    assert_eq!(p.execution.download_submission, "not_implemented");
    assert!(p.execution.grabs.is_empty());
    let out = store
        .create(plan("pln_z", "movie:tmdb:603"), key("k", "f"), false)
        .await
        .unwrap();
    assert_eq!(existing_id(out), "pln_v2");
    drop(store);
    let c = rusqlite::Connection::open(&db).unwrap();
    let version: String = c
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, "3");
    drop(c);
    // Idempotent re-open.
    let store = Store::open(&db).unwrap();
    assert!(store.get("pln_v2".into()).await.unwrap().is_some());
}

#[tokio::test]
async fn schema_3_columns_added_later_are_migrated_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.sqlite");
    drop(Store::open(&db).unwrap());
    // A schema-3 database from the first execution build lacks `media.monitor_new`.
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch("ALTER TABLE media DROP COLUMN monitor_new")
        .unwrap();
    drop(c);
    let store = Store::open(&db).unwrap();
    let has: bool = store
        .blocking(|c| {
            Ok(
                c.prepare("SELECT 1 FROM pragma_table_info('media') WHERE name = 'monitor_new'")?
                    .exists([])?,
            )
        })
        .unwrap();
    assert!(has);
    drop(store);
    // Idempotent.
    Store::open(&db).unwrap();
}
