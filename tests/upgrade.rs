//! Upgrade and rollback safety: what a user who installed an earlier release keeps on disk
//! must keep working with the code in this tree.
//!
//! Two things are protected here.
//!
//! 1. Configs. `tests/fixtures/configs/` holds the config.toml exactly as each released
//!    installer or setup page wrote it (install.sh and install.ps1 v0.3.0 .. v0.10.0, `arr
//!    wizard` v0.4.0 .. v0.10.0, and the remote-access block they add), deduplicated to the
//!    distinct layouts. The only edits are placeholders: `@ROOT@` for folders on the machine.
//!    Every one must still load. The config sections are `deny_unknown_fields`, so a later change
//!    that removes or renames a key (or rejects a value) a release wrote fails here, and
//!    so does one that makes a new key mandatory.
//! 2. State. `tests/fixtures/state-v0.9.0/` is the state of a managed setup (Plex-bootstrapped
//!    movie and series, one grab in flight) as the released v0.9.0 binary wrote it: the active
//!    import with its digests, and the SQLite store. The current code must open it, serve it,
//!    and still know its titles, its plans (decoded into the current Plan type) and its grab. A later change that renames a stored field, a
//!    column or a settings key, or that cannot read an old value, fails here.
//!
//! The installers update by swapping the binary and rolling back to the previous one if the new
//! one does not report healthy, so the reverse also matters: the previous release must start on
//! the state and config the new one has run. That needs the previous release's binary; it is
//! checked in CI by `tests/upgrade/rollback.sh`, which installs with the current
//! install.sh and the current setup page's config so that a new key either writes is caught.
//!
//! When a fixture has to change because a release really changed what it writes, add the new
//! layout next to the old ones; never edit an old one to make a test pass.

use arr::{api, config, fsutil};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const TOKEN: &str = "upgrade-test-api-token-0123456789abcdef";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A folder path as TOML and the stored JSON can both hold it, on every platform.
fn slashed(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Creates every `secrets/<file>` a config mentions, as the installers did.
fn write_secrets(dir: &Path, config_text: &str) {
    for part in config_text.split("\"secrets/").skip(1) {
        let name = part.split('"').next().unwrap();
        let path = dir.join("secrets").join(name);
        if !path.exists() {
            let value = if name == "api_token" {
                TOKEN.to_string()
            } else {
                format!("{name}-fake-secret-value-0123456789")
            };
            fsutil::write_new_private_file(&path, value.as_bytes()).unwrap();
        }
    }
}

fn state_dir(root: &Path) -> PathBuf {
    let dir = root.join("state");
    fsutil::ensure_private_dir(&dir).unwrap();
    fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
    dir
}

#[test]
fn every_released_config_layout_still_loads() {
    let mut names: Vec<_> = std::fs::read_dir(fixtures().join("configs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".toml"))
        .collect();
    names.sort();
    assert!(names.len() >= 9, "fixtures went missing: {names:?}");
    for name in &names {
        let root = tempfile::tempdir().unwrap();
        let dir = state_dir(root.path());
        let text = std::fs::read_to_string(fixtures().join("configs").join(name))
            .unwrap()
            .replace("@ROOT@", &slashed(root.path()));
        assert!(!text.contains("@ROOT@"));
        write_secrets(&dir, &text);
        let cfg_path = dir.join("config.toml");
        fsutil::atomic_replace_private(&cfg_path, text.as_bytes()).unwrap();
        let cfg = config::load(&cfg_path).unwrap_or_else(|e| panic!("{name}: {e:#}"));

        // What each file says, read back: nothing was silently dropped.
        assert_eq!(cfg.search_backend(), Some("native"), "{name}");
        assert_eq!(cfg.download_backend(), Some("embedded"), "{name}");
        assert!(
            cfg.qbittorrent.is_none() && cfg.prowlarr.is_none(),
            "{name}"
        );
        assert!(cfg.mcp.is_some() && cfg.plex.is_some(), "{name}");
        assert_eq!(cfg.migration, config::MigrationMode::Coexistence, "{name}");
        assert_eq!(cfg.reporting, text.contains("[reporting]"), "{name}");
        assert_eq!(
            cfg.execution_enabled,
            text.contains("[execution]\nenabled = true"),
            "{name}"
        );
        assert_eq!(cfg.tmdb.is_some(), text.contains("[tmdb]\nenabled = true"));
        assert_eq!(
            cfg.mcp.as_ref().unwrap().oauth.is_some(),
            text.contains("[mcp.oauth]"),
            "{name}"
        );
        let upload = cfg.embedded.as_ref().unwrap().upload_bps;
        assert_eq!(
            upload.is_some(),
            text.contains("upload_limit_bytes_per_sec"),
            "{name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_written_by_v0_9_0_opens_and_serves() {
    let root = tempfile::tempdir().unwrap();
    let dir = state_dir(root.path());

    // The fixture's state, copied as it is, but for the folder the machine it came from had.
    let src = fixtures().join("state-v0.9.0");
    copy_private(
        &src.join("active-import.json"),
        &dir.join("active-import.json"),
    );
    copy_tree(&src.join("imports"), &dir.join("imports"));
    let db = dir.join("state.sqlite");
    copy_private(&src.join("state.sqlite"), &db);
    rewrite_root(&db, &slashed(root.path()));

    // The config the newest installer layout writes, with the sockets kept on this machine.
    let text = std::fs::read_to_string(fixtures().join("configs/wizard-v0.10.toml"))
        .unwrap()
        .replace("@ROOT@", &slashed(root.path()))
        .replace("0.0.0.0:51413", "127.0.0.1:0")
        .replace(
            "dht = true",
            "dht = false\ninitial_peers = [\"127.0.0.1:9\"]",
        );
    write_secrets(&dir, &text);
    let cfg_path = dir.join("config.toml");
    fsutil::atomic_replace_private(&cfg_path, text.as_bytes()).unwrap();
    let cfg = config::load(&cfg_path).unwrap();

    let state = Arc::new(api::AppState::new(cfg).expect("the old state opens"));
    let (app, _stop) = api::app(state.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let get = |path: &str| {
        let req = client.get(format!("{base}{path}")).bearer_auth(TOKEN);
        async move {
            let r = req.send().await.unwrap();
            let status = r.status().as_u16();
            (status, r.text().await.unwrap())
        }
    };

    let (s, body) = get("/api/v1/health").await;
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("\"status\":\"ok\""), "{body}");

    // The managed setup survived: its settings, and its origin.
    let (s, body) = get("/api/v1/setup").await;
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("managed_setup"), "{body}");
    let (s, body) = get("/api/v1/settings").await;
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("setup:movies-hd"), "{body}");

    // The catalog from the old store: owned titles (Plex-bootstrapped) and a requested one.
    let (s, body) = get("/api/v1/library/movies").await;
    assert_eq!(s, 200, "{body}");
    assert!(
        body.contains("The Menu") && body.contains("The Matrix"),
        "{body}"
    );
    let (s, body) = get("/api/v1/library/series").await;
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("Slow Horses"), "{body}");
    let index = state.index();
    assert!(index.movie(593643).unwrap().has_file, "The Menu is owned");
    assert!(
        index.movie(603).unwrap().monitored,
        "The Matrix is requested"
    );
    let horses = index.series(arr::ids::SeriesKey::Tvdb(372264)).unwrap();
    assert_eq!(
        horses
            .episodes
            .as_ref()
            .unwrap()
            .iter()
            .filter(|e| e.has_file)
            .count(),
        2
    );

    // The grab that was in flight is still there, with its plan.
    let (s, body) = get("/api/v1/queue?all=true").await;
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("grb_aad47e6df55688785fa2908f"), "{body}");
    // Its plans are stored as JSON and decoded into the current Plan type: the listing and the
    // single read both fail if a required field was renamed or removed.
    let (s, body) = get("/api/v1/plans").await;
    assert_eq!(s, 200, "{body}");
    let plans: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(plans["total"], 2, "{body}");
    let by_state = |state: &str| {
        plans["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["state"] == state)
            .unwrap_or_else(|| panic!("no {state} plan: {body}"))
            .clone()
    };
    assert_eq!(by_state("superseded")["id"], "pln_b8a51380258b6172e7363f95");
    let live = by_state("download_queued");
    assert_eq!(live["id"], "pln_9c8b32cb1d220af18190a600");
    assert_eq!(live["target"], "movie:tmdb:603");
    let (s, body) = get("/api/v1/plans/pln_9c8b32cb1d220af18190a600").await;
    assert_eq!(s, 200, "{body}");
    let plan: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(plan["plan"]["state"], "download_queued", "{body}");
    assert_eq!(plan["plan"]["target"], "movie:tmdb:603", "{body}");
    assert_eq!(plan["plan"]["execution"]["outcome"], "queued", "{body}");
    assert_eq!(
        plan["plan"]["execution"]["grabs"][0], "grb_aad47e6df55688785fa2908f",
        "the plan names its grab: {body}"
    );
    assert_eq!(plan["plan"]["profile"], "setup:movies-hd", "{body}");
    let stored = state.store.get("pln_9c8b32cb1d220af18190a600".into()).await;
    assert!(
        stored.unwrap().is_some(),
        "decoded from the store as a Plan"
    );
    let (s, body) = get("/api/v1/readiness").await;
    assert_eq!(s, 200, "{body}");

    server.abort();
    if let Some(e) = state.client.as_deref().and_then(|c| c.embedded()) {
        e.shutdown().await;
    }
}

fn copy_private(from: &Path, to: &Path) {
    let bytes = std::fs::read(from).unwrap();
    fsutil::write_new_private_file(to, &bytes).unwrap();
}

fn copy_tree(from: &Path, to: &Path) {
    fsutil::ensure_private_dir(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            copy_private(&entry.path(), &target);
        }
    }
}

/// The fixture was made with its folders under `@ROOT@`; point them at this test's folder.
fn rewrite_root(db: &Path, root: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for table in tables {
        let columns: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |r| r.get(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for col in columns {
            conn.execute(
                &format!(
                    "UPDATE {table} SET {col} = replace({col}, '@ROOT@', ?1) \
                     WHERE typeof({col}) = 'text' AND instr({col}, '@ROOT@') > 0"
                ),
                [root],
            )
            .unwrap();
        }
    }
}
