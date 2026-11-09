//! Importer safety: atomic, repeatable, private, tamper-evident, explicit about gaps.
#![recursion_limit = "256"]

mod common;

use arr::import::{self, ImportOptions};
use arr::{catalog::CatalogIndex, fsutil};
use common::*;
use serde_json::{Value, json};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn opts(state: &Path, snap: &Path) -> ImportOptions {
    ImportOptions {
        state_dir: state.to_path_buf(),
        snapshot: snap.to_path_buf(),
        observed_at: Some(time::macros::datetime!(2026-10-01 12:00 UTC)),
        dry_run: false,
    }
}

fn write_snapshot(dir: &Path, name: &str, v: &Value) -> std::path::PathBuf {
    let p = dir.join(name);
    let _ = fs::remove_file(&p);
    fsutil::write_new_private_file(&p, &serde_json::to_vec(v).unwrap()).unwrap();
    p
}

/// Asserts a Unix mode; Windows has none (privacy comes from the folder's access list).
macro_rules! assert_mode {
    ($p:expr, $want:expr $(, $($msg:tt)*)?) => {
        #[cfg(unix)]
        assert_eq!(fs::metadata($p).unwrap().permissions().mode() & 0o777, $want $(, $($msg)*)?);
    };
}

fn normalized(v: &Value) -> import::Normalized {
    import::normalize(
        v,
        "imp-test",
        "2026-10-01T12:00:00Z",
        "sha",
        0,
        "2026-10-01T12:00:00Z",
    )
    .unwrap()
}

#[test]
fn import_is_atomic_repeatable_private_and_secret_free() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let snap = write_snapshot(tmp.path(), "native.json", &snapshot());
    let a = import::run_import(&opts(&state, &snap)).unwrap();
    assert_eq!(a.status, "created");
    let dir = a.import_dir.clone().unwrap();
    assert_mode!(&state, 0o700);
    assert_mode!(&state.join("imports"), 0o700);
    assert_mode!(&dir, 0o700);
    for f in fs::read_dir(&dir).unwrap() {
        let f = f.unwrap().path();
        assert_mode!(&f, 0o600, "{}", f.display());
        if f.file_name().unwrap() != "source-native.json" {
            assert_no_leaks(&fs::read_to_string(&f).unwrap());
        }
    }
    assert_eq!(
        fs::read(dir.join("source-native.json")).unwrap(),
        fs::read(&snap).unwrap(),
        "original preserved byte-for-byte"
    );
    assert_mode!(&state.join("active-import.json"), 0o600);

    // Same bytes again: no new directory, same id.
    let b = import::run_import(&opts(&state, &snap)).unwrap();
    assert_eq!(
        (b.status, b.import_id.as_str()),
        ("unchanged", a.import_id.as_str())
    );
    assert_eq!(fs::read_dir(state.join("imports")).unwrap().count(), 1);

    // Changed snapshot: new content-addressed import becomes active; the old one is kept.
    let mut v = snapshot();
    v["radarr"]["movie"].as_array_mut().unwrap().pop();
    let snap2 = write_snapshot(tmp.path(), "native2.json", &v);
    let c = import::run_import(&opts(&state, &snap2)).unwrap();
    assert_eq!(c.status, "created");
    assert_ne!(c.import_id, a.import_id);
    assert_eq!(fs::read_dir(state.join("imports")).unwrap().count(), 2);
    let loaded = import::load_active(&state).unwrap().unwrap();
    assert_eq!(loaded.manifest.import_id, c.import_id);
    assert_eq!(loaded.catalog.movies.len(), 2);

    // Same bytes observed later: a new import records the newer observation time.
    let later = import::run_import(&ImportOptions {
        observed_at: Some(time::macros::datetime!(2026-10-02 12:00 UTC)),
        ..opts(&state, &snap)
    })
    .unwrap();
    assert_eq!(later.status, "created");
    assert_eq!(later.observed_at, "2026-10-02T12:00:00Z");
    assert_eq!(later.snapshot_sha256, a.snapshot_sha256);

    // Re-activating the first snapshot is a pointer swap.
    let d = import::run_import(&opts(&state, &snap)).unwrap();
    assert_eq!(
        (d.status, d.import_id.as_str()),
        ("unchanged", a.import_id.as_str())
    );
    assert_eq!(
        import::load_active(&state)
            .unwrap()
            .unwrap()
            .manifest
            .import_id,
        a.import_id
    );
}

#[test]
fn failed_or_dry_run_import_leaves_previous_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let good = write_snapshot(tmp.path(), "good.json", &snapshot());
    let first = import::run_import(&opts(&state, &good)).unwrap();
    let pointer = fs::read(state.join("active-import.json")).unwrap();

    let mut bad = snapshot();
    bad["sonarr"]
        .as_object_mut()
        .unwrap()
        .remove("qualityprofile");
    let bad = write_snapshot(tmp.path(), "bad.json", &bad);
    let err = format!("{:#}", import::run_import(&opts(&state, &bad)).unwrap_err());
    assert!(err.contains("sonarr.qualityprofile is required"), "{err}");

    let mut malformed = snapshot();
    malformed["radarr"]["qualityprofile"] = json!({ "not": "an array" });
    let malformed = write_snapshot(tmp.path(), "malformed.json", &malformed);
    assert!(import::run_import(&opts(&state, &malformed)).is_err());
    let not_json = tmp.path().join("not.json");
    fsutil::write_new_private_file(&not_json, b"{ truncated").unwrap();
    assert!(
        format!(
            "{:#}",
            import::run_import(&opts(&state, &not_json)).unwrap_err()
        )
        .contains("not valid JSON")
    );

    let mut v = snapshot();
    v["radarr"]["movie"] = json!([]);
    let other = write_snapshot(tmp.path(), "other.json", &v);
    let dry = import::run_import(&ImportOptions {
        dry_run: true,
        ..opts(&state, &other)
    })
    .unwrap();
    assert_eq!(dry.status, "dry_run");
    assert_eq!(dry.summary.movies, 0);

    assert_eq!(
        fs::read(state.join("active-import.json")).unwrap(),
        pointer,
        "active import unchanged"
    );
    let names: Vec<String> = fs::read_dir(state.join("imports"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        names,
        [first.import_id],
        "no partial or temp directories left behind"
    );
}

#[cfg(unix)] // Unix permissions or symlinks
#[test]
fn unsafe_paths_and_tampering_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let snap = write_snapshot(tmp.path(), "native.json", &snapshot());

    let open = tmp.path().join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        format!("{:#}", import::run_import(&opts(&open, &snap)).unwrap_err()).contains("chmod 700")
    );

    let real = tmp.path().join("real");
    fsutil::ensure_private_dir(&real).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(
        format!("{:#}", import::run_import(&opts(&link, &snap)).unwrap_err()).contains("symlink")
    );
    assert!(
        import::run_import(&opts(&real, tmp.path())).is_err(),
        "snapshot must be a regular file"
    );

    // World-readable snapshot: imported, with a warning (it contains credentials).
    let loose = tmp.path().join("loose.json");
    fs::write(&loose, serde_json::to_vec(&snapshot()).unwrap()).unwrap();
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).unwrap();
    let s = import::run_import(&opts(&real, &loose)).unwrap();
    assert!(s.warnings[0].contains("chmod 600"));

    // Tampering with any imported file is detected at load.
    let dir = s.import_dir.unwrap();
    let settings = dir.join("settings.json");
    let mut text = fs::read_to_string(&settings).unwrap();
    text = text.replace("\"minimum_seeders\": 1", "\"minimum_seeders\": 0");
    fs::write(&settings, text).unwrap();
    let err = format!("{:#}", import::load_active(&real).err().unwrap());
    assert!(err.contains("does not match its manifest digest"), "{err}");
}

#[test]
fn catalog_collisions_skips_and_numbering_are_reported() {
    let mut v = snapshot();
    let movies = v["radarr"]["movie"].as_array_mut().unwrap();
    let mut dup = movies[0].clone();
    dup["id"] = json!(99);
    movies.push(dup);
    movies.push(json!({ "id": 100, "title": "No Ids", "tmdbId": 0 }));
    movies.push(json!({ "id": 101, "title": 5, "tmdbId": 77 }));
    let series = v["sonarr"]["series"].as_array_mut().unwrap();
    let mut scene = series[1].clone();
    scene["id"] = json!(22);
    scene["tvdbId"] = json!(555);
    scene["tvMazeId"] = json!(9002);
    scene["useSceneNumbering"] = json!(true);
    scene["alternateTitles"] = json!([{ "title": "Fargo Scene", "seasonNumber": 1, "sceneSeasonNumber": 2 }, { "title": "Fargo Alias", "seasonNumber": -1 }]);
    series.push(scene);
    v["sonarr"]["episode_samples"]["404"] = json!([]);
    let n = normalized(&v);
    let r = &n.report;
    assert_eq!(
        n.catalog
            .movies
            .iter()
            .filter(|m| m.identity_conflict)
            .count(),
        2,
        "both colliding entries are marked"
    );
    assert!(
        r.catalog_conflicts
            .iter()
            .any(|c| c.contains("movie:tmdb:603 appears more than once"))
    );
    assert!(
        r.catalog_conflicts
            .iter()
            .any(|c| c.contains("tvmaze id 9002 is shared"))
    );
    assert!(
        r.catalog_skipped
            .iter()
            .any(|c| c.contains("missing tmdbId"))
    );
    assert!(
        r.catalog_skipped
            .iter()
            .any(|c| c.contains("unexpected shape"))
    );
    assert!(
        r.catalog_notes
            .iter()
            .any(|c| c.contains("episode_samples[404] does not match"))
    );
    assert!(
        r.catalog_notes
            .iter()
            .any(|c| c.contains("scene-season aliases dropped"))
    );
    let s = n.catalog.series.iter().find(|s| s.tvdb_id == 555).unwrap();
    assert!(s.scene_numbering);
    assert_eq!(s.alternate_titles.len(), 1);
    assert_eq!(s.alternate_titles[0].season, None);
    assert_eq!(
        s.root_folder.as_deref(),
        Some("/tvshows"),
        "root derived from path, full path not stored"
    );
    let idx = CatalogIndex::new(n.catalog.clone());
    assert!(idx.movie(603).unwrap().identity_conflict);
    assert!(r.credentials.iter().all(|c| {
        c.fields
            .values()
            .all(|v| v == "present" || v == "masked_or_empty")
    }));
    assert_no_leaks(&serde_json::to_string(&n.report).unwrap());
    assert_no_leaks(&serde_json::to_string(&n.settings).unwrap());
    assert_no_leaks(&serde_json::to_string(&n.catalog).unwrap());
}

#[test]
fn unsupported_semantics_block_or_are_reported() {
    let mut v = snapshot();
    v["radarr"]["qualityprofile"][0]["items"]
        .as_array_mut()
        .unwrap()
        .push(
            json!({ "quality": { "id": 99, "name": "Bluray-8K" }, "items": [], "allowed": true }),
        );
    v["sonarr"]["qualityprofile"][0]["items"][6]["maxSize"] = json!(50);
    v["sonarr"]["delayprofile"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "id": 2, "enableTorrent": true, "torrentDelay": 60, "tags": [7] }));
    assert_eq!(
        v["radarr"]["indexer"][0]["fields"][7]["name"],
        "requiredFlags"
    );
    v["radarr"]["indexer"][0]["fields"][7]["value"] = json!([1]);
    let mut jackett = v["radarr"]["indexer"][0].clone();
    jackett["id"] = json!(2);
    jackett["name"] = json!("Jackett");
    jackett["fields"][0]["value"] =
        json!("http://user:pw@jackett:9117/api/v2.0/indexers/x/results/torznab/?apikey=secret");
    v["radarr"]["indexer"].as_array_mut().unwrap().push(jackett);
    v["radarr"]["indexer"][0]["fields"][3]["value"] = json!([2000, 5000, 100201]);
    v["radarr"]["config/indexer"]["futureApiKey"] = json!(FAKE_APIKEY);
    v["radarr"]["releaseprofile"] = json!([{ "id": 1, "enabled": false, "required": ["x"] }]);
    v["sonarr"]["releaseprofile"] =
        json!([{ "id": 1, "enabled": true, "required": [], "ignored": ["x265"] }]);
    let n = normalized(&v);
    let codes: Vec<&str> = n
        .settings
        .blocking_issues
        .iter()
        .map(|b| b.code.as_str())
        .collect();
    assert!(codes.contains(&"unknown_allowed_quality"), "{codes:?}");
    let rp: Vec<_> = n
        .settings
        .blocking_issues
        .iter()
        .filter(|b| b.code == "release_profile_terms")
        .collect();
    assert_eq!(rp.len(), 1, "only the enabled Sonarr profile blocks");
    assert_eq!(
        rp[0].scope,
        arr::settings::IssueScope::Media {
            media: arr::settings::MediaKind::Series
        }
    );
    // Absent section: reported as a gap, not silently treated as "none".
    let base = normalized(&snapshot());
    assert!(
        base.report
            .items
            .iter()
            .any(|i| i.area == "releaseprofile" && i.effect.contains("NOT enforced"))
    );
    assert!(
        base.report.sources[0]
            .sections_missing
            .contains(&"releaseprofile".to_string())
    );
    assert!(codes.contains(&"profile_size_overrides"));
    assert!(codes.contains(&"tag_routing"));
    let ix = n
        .settings
        .indexers
        .iter()
        .find(|i| i.native.app == "radarr")
        .unwrap();
    assert_eq!(
        ix.unsupported,
        ["requiredFlags"],
        "flags cannot be evaluated: this indexer's releases are rejected"
    );
    assert_eq!(
        ix.categories,
        [2000],
        "categories outside the movie range are not searched"
    );
    assert_eq!(
        n.settings
            .indexers
            .iter()
            .filter(|i| i.native.app == "radarr")
            .count(),
        1,
        "non-Prowlarr indexer unusable"
    );
    let jackett = n
        .report
        .items
        .iter()
        .find(|i| i.area.contains("Jackett") && i.setting == "mapping")
        .unwrap();
    assert_eq!(
        jackett.value["baseUrl"], "http://jackett:9117/api/v2.0/indexers/x/results/torznab/",
        "userinfo and query stripped"
    );
    assert!(!serde_json::to_string(&n.report).unwrap().contains("secret"));
    assert_no_leaks(&serde_json::to_string(&n.report).unwrap());
    let md = import::render_markdown(&n.report);
    assert!(md.contains("unknown_allowed_quality") && md.contains("Cutover blockers"));
}

#[test]
fn cli_init_import_check() {
    let bin = env!("CARGO_BIN_EXE_arr");
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let run = |args: &[&str]| {
        let out = std::process::Command::new(bin).args(args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    let (ok, out, err) = run(&[
        "init",
        "--state-dir",
        state.to_str().unwrap(),
        "--prowlarr-url",
        "http://127.0.0.1:9696",
    ]);
    assert!(ok, "{err}");
    assert!(out.contains("api_token"));
    let token = fs::read_to_string(state.join("secrets/api_token")).unwrap();
    assert_eq!(token.len(), 64);
    assert_mode!(&state.join("secrets/api_token"), 0o600);
    assert_mode!(&state.join("config.toml"), 0o600);
    let (ok, _, _) = run(&["init", "--state-dir", state.to_str().unwrap()]);
    assert!(ok);
    assert_eq!(
        fs::read_to_string(state.join("secrets/api_token")).unwrap(),
        token,
        "init never overwrites"
    );

    let snap = write_snapshot(tmp.path(), "native.json", &snapshot());
    let (ok, out, err) = run(&[
        "import",
        "--state-dir",
        state.to_str().unwrap(),
        "--snapshot",
        snap.to_str().unwrap(),
        "--observed-at",
        "2026-10-01T12:00:00Z",
    ]);
    assert!(ok, "{err}");
    let summary: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(summary["status"], "created");
    assert_eq!(summary["summary"]["movies"], 3);
    assert_no_leaks(&out);
    assert_no_leaks(&err);

    // check fails without the Prowlarr key file, then passes.
    let cfg = state.join("config.toml");
    let (ok, _, err) = run(&["check", "--config", cfg.to_str().unwrap()]);
    assert!(!ok && err.contains("prowlarr.api_key_file"), "{err}");
    fsutil::write_new_private_file(
        &state.join("secrets/prowlarr_api_key"),
        PROWLARR_KEY.as_bytes(),
    )
    .unwrap();
    let (ok, out, err) = run(&["check", "--config", cfg.to_str().unwrap()]);
    assert!(ok, "{err}");
    let check: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(check["execution_enabled"], false);
    assert_eq!(check["qbittorrent_configured"], false);
    assert_eq!(check["active_import"]["movies"], 3);
    assert!(!out.contains(PROWLARR_KEY) && !err.contains(PROWLARR_KEY) && !out.contains(&token));
}

#[test]
fn sonarr_v4_language_stub_is_an_observation_not_policy() {
    use arr::import::ItemStatus;
    use arr::settings::{LanguageRequirement, UnmarkedLanguage};
    // The fixture carries Sonarr v4's retired "Deprecated" stub (English allowed) and no
    // custom formats: effective v4 policy is "no language filter".
    let n = normalized(&snapshot());
    let sonarr = n.settings.profile("sonarr:6").unwrap();
    assert_eq!(sonarr.language.requirement, LanguageRequirement::Any);
    assert!(
        sonarr.language.source.contains("retired"),
        "{}",
        sonarr.language.source
    );
    // Radarr's real profile language stays active.
    let radarr = n.settings.profile("radarr:6").unwrap();
    assert_eq!(
        radarr.language.requirement,
        LanguageRequirement::OneOf {
            languages: vec!["english".into()]
        }
    );
    assert_eq!(radarr.language.unmarked, UnmarkedLanguage::AssumeOriginal);
    let stub = n
        .report
        .items
        .iter()
        .find(|i| i.app == "sonarr" && i.area == "languageprofile")
        .unwrap();
    assert_eq!(stub.status, ItemStatus::ObservedOnly);
    assert!(stub.effect.contains("not enforced"));
    assert!(n.settings.blocking_issues.is_empty());

    // Pre-v4 language profiles are not modelled: fail closed for series only.
    let mut v = snapshot();
    v["sonarr"]["languageprofile"] = json!([
        { "id": 1, "name": "English", "languages": [{ "language": { "id": 1, "name": "English" }, "allowed": true }] },
        { "id": 2, "name": "Any", "languages": [] }
    ]);
    let n = normalized(&v);
    let b: Vec<_> = n
        .settings
        .blocking_issues
        .iter()
        .map(|b| b.code.as_str())
        .collect();
    assert_eq!(b, ["language_profile_unsupported"]);

    // Language rules expressed as scored custom formats remain fail-closed.
    let mut v = snapshot();
    v["sonarr"]["customformat"] = json!([{ "id": 1, "name": "Not English", "specifications": [] }]);
    v["sonarr"]["qualityprofile"][0]["formatItems"] =
        json!([{ "format": 1, "name": "Not English", "score": -10000 }]);
    let n = normalized(&v);
    assert!(
        n.settings
            .blocking_issues
            .iter()
            .any(|b| b.code == "custom_format_scoring"
                && b.scope
                    == arr::settings::IssueScope::Profile {
                        profile: "sonarr:6".into()
                    })
    );
}

#[test]
fn changed_normalization_creates_a_new_import_and_keeps_the_old_one() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let snap = write_snapshot(tmp.path(), "native.json", &snapshot());
    let current = import::run_import(&opts(&state, &snap)).unwrap();
    let current_dir = current.import_dir.clone().unwrap();

    // Re-create what an earlier build wrote for the same bytes and observed_at: an English
    // Sonarr rule derived from the retired stub, under a different content-addressed id.
    let old_id = "imp-0123456789abcdef-s1";
    let old_dir = state.join("imports").join(old_id);
    fs::rename(&current_dir, &old_dir).unwrap();
    let settings_path = old_dir.join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    for p in settings["profiles"].as_array_mut().unwrap() {
        if p["id"] == "sonarr:6" {
            p["language"] = json!({ "requirement": { "mode": "one_of", "languages": ["english"] },
                                    "unmarked": "accept", "source": "sonarr deprecated language profile 'Deprecated'" });
        }
    }
    let old_settings = serde_json::to_vec_pretty(&settings).unwrap();
    fs::remove_file(&settings_path).unwrap();
    fsutil::write_new_private_file(&settings_path, &old_settings).unwrap();
    let manifest_path = old_dir.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["import_id"] = json!(old_id);
    manifest["files"]["settings.json"] = json!(import::sha256_hex(&old_settings));
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    fs::remove_file(&manifest_path).unwrap();
    fsutil::write_new_private_file(&manifest_path, &manifest_bytes).unwrap();
    fsutil::atomic_replace_private(
        &state.join("active-import.json"),
        &serde_json::to_vec(
            &json!({ "import_id": old_id, "manifest_sha256": import::sha256_hex(&manifest_bytes) }),
        )
        .unwrap(),
    )
    .unwrap();
    let loaded = import::load_active(&state).unwrap().unwrap();
    assert_eq!(
        loaded.manifest.import_id, old_id,
        "the older import is valid and active"
    );
    let before: Vec<(String, Vec<u8>)> = {
        let mut v: Vec<_> = fs::read_dir(&old_dir)
            .unwrap()
            .map(|e| {
                let p = e.unwrap().path();
                (
                    p.file_name().unwrap().to_string_lossy().to_string(),
                    fs::read(&p).unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    };

    // Same snapshot bytes and observed_at: the current normalisation differs, so a new import
    // is created and activated; the old directory is left byte-for-byte untouched.
    let again = import::run_import(&opts(&state, &snap)).unwrap();
    assert_eq!(again.status, "created");
    assert_eq!(again.import_id, current.import_id);
    assert_ne!(again.import_id, old_id);
    let mut after: Vec<_> = fs::read_dir(&old_dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (
                p.file_name().unwrap().to_string_lossy().to_string(),
                fs::read(&p).unwrap(),
            )
        })
        .collect();
    after.sort();
    assert_eq!(before, after, "old import is not rewritten in place");
    let loaded = import::load_active(&state).unwrap().unwrap();
    assert_eq!(loaded.manifest.import_id, current.import_id);
    assert_eq!(
        loaded
            .settings
            .profile("sonarr:6")
            .unwrap()
            .language
            .requirement,
        arr::settings::LanguageRequirement::Any
    );
}

#[test]
fn imports_written_by_earlier_builds_still_load() {
    use arr::catalog::Catalog;
    use arr::settings::Settings;
    let n = normalized(&snapshot());
    // Settings as an earlier build wrote them: no media management, no revision/redownload/RSS
    // policy fields.
    let mut settings = serde_json::to_value(&n.settings).unwrap();
    settings.as_object_mut().unwrap().remove("media_management");
    for p in settings["release_policies"].as_array_mut().unwrap() {
        for k in [
            "revision_upgrades",
            "redownload_failed",
            "redownload_failed_interactive",
            "rss_sync_interval_minutes",
        ] {
            p.as_object_mut().unwrap().remove(k);
        }
    }
    let old: Settings = serde_json::from_value(settings).unwrap();
    assert!(old.media_management.is_empty());
    let policy = old.policy(arr::settings::MediaKind::Series).unwrap();
    assert!(
        !policy.revision_upgrades,
        "absent = no revision upgrades (fail closed)"
    );
    assert_eq!(
        policy.rss_sync_interval_minutes, None,
        "absent = native default interval"
    );
    assert_eq!(
        arr::rss::interval(&old, None),
        Some(std::time::Duration::from_secs(15 * 60))
    );
    // Catalog entries without folders, file evidence or managed-state flags.
    let mut catalog = serde_json::to_value(&n.catalog).unwrap();
    for m in catalog["movies"].as_array_mut().unwrap() {
        for k in ["folder", "file", "ownership_unknown", "queued"] {
            m.as_object_mut().unwrap().remove(k);
        }
    }
    for s in catalog["series"].as_array_mut().unwrap() {
        s.as_object_mut().unwrap().remove("folder");
        for e in s["episodes"].as_array_mut().into_iter().flatten() {
            for k in ["file", "ownership_unknown", "queued"] {
                e.as_object_mut().unwrap().remove(k);
            }
        }
    }
    let old: Catalog = serde_json::from_value(catalog).unwrap();
    assert!(
        old.movies
            .iter()
            .all(|m| m.file.is_none() && m.folder.is_none())
    );
    // A legacy numeric revision in stored file evidence still reads.
    let ev: arr::catalog::FileEvidence = serde_json::from_value(json!({
        "path": "x/y.mkv", "size": 1, "quality": "webdl_1080p", "revision": 2, "source": "snapshot", "observed_at": "2026-01-01T00:00:00Z"
    }))
    .unwrap();
    assert_eq!(ev.revision.unwrap().version, 2);
}

#[test]
fn native_file_evidence_and_revisions_are_imported_never_guessed() {
    let mut v = snapshot();
    v["radarr"]["movie"][1]["movieFile"] = json!({ "relativePath": "The.Matrix.Reloaded.2003.1080p.WEB-DL.mkv", "size": 123,
        "quality": { "quality": { "id": 3, "name": "WEBDL-1080p" }, "revision": { "version": 2, "real": 1, "isRepack": true } } });
    v["sonarr"]["episode_samples"]["20"][0]["episodeFile"] = json!({ "relativePath": "Season 1/Slow.Horses.S01E01.mkv", "size": 5,
        "quality": { "quality": { "id": 3, "name": "WEBDL-1080p" } } });
    let n = normalized(&v);
    let m = n.catalog.movies.iter().find(|m| m.tmdb_id == 604).unwrap();
    assert_eq!(m.folder.as_deref(), Some("The Matrix Reloaded (2003)"));
    let f = m.file.as_ref().unwrap();
    assert_eq!(
        f.path.as_deref(),
        Some("The Matrix Reloaded (2003)/The.Matrix.Reloaded.2003.1080p.WEB-DL.mkv")
    );
    assert_eq!(
        f.revision,
        Some(arr::release::Revision {
            version: 2,
            real: 1
        })
    );
    let s = n
        .catalog
        .series
        .iter()
        .find(|s| s.tvdb_id == 372264)
        .unwrap();
    let e1 = s
        .episodes
        .as_ref()
        .unwrap()
        .iter()
        .find(|e| e.number == 1)
        .unwrap();
    let f = e1.file.as_ref().unwrap();
    assert_eq!(
        f.path.as_deref(),
        Some("Slow Horses/Season 1/Slow.Horses.S01E01.mkv")
    );
    assert_eq!(f.revision, None, "no exported revision: unknown, not 1");
    // Unsafe relative paths are dropped, not trusted.
    v["sonarr"]["episode_samples"]["20"][0]["episodeFile"]["relativePath"] =
        json!("../../etc/passwd");
    let n = normalized(&v);
    let s = n
        .catalog
        .series
        .iter()
        .find(|s| s.tvdb_id == 372264)
        .unwrap();
    assert_eq!(
        s.episodes.as_ref().unwrap()[0].file.as_ref().unwrap().path,
        None
    );
}

#[test]
fn rss_seed_criteria_and_client_report_their_implemented_effects() {
    use import::ItemStatus;
    let find = |n: &import::Normalized, app: &str, setting: &str| {
        n.report
            .items
            .iter()
            .find(|i| i.app == app && i.area.starts_with("indexer[") && i.setting == setting)
            .unwrap_or_else(|| panic!("{app} {setting}"))
            .clone()
    };
    let v = snapshot();
    let n = normalized(&v);
    for app in ["radarr", "sonarr"] {
        let rss = find(&n, app, "enableRss");
        assert_eq!(
            (rss.status, &rss.value),
            (ItemStatus::Enforced, &json!(true))
        );
        assert!(
            rss.effect.contains("reads this indexer's Torznab feed")
                && rss.effect.contains("execution enabled"),
            "{}",
            rss.effect
        );
        let seed = find(&n, app, "seed criteria");
        assert_eq!(seed.status, ItemStatus::Enforced);
        assert_eq!(seed.value["seedRatio"], json!(2.0));
        assert!(
            seed.effect.contains("ratio/seeding-time limits")
                && seed.effect.contains("verified import"),
            "{}",
            seed.effect
        );
        let client = n
            .report
            .items
            .iter()
            .find(|i| i.app == app && i.setting == "client")
            .unwrap();
        assert_eq!(client.status, ItemStatus::Enforced);
        assert_eq!(client.value["execution"], "server_config");
    }
    // No stale shadow-only claim survives in the items or the rendered report, and none of
    // these settings counts towards the not-implemented summary.
    let md = import::render_markdown(&n.report);
    for stale in [
        "RSS sync is not implemented",
        "(not implemented)",
        "submission is impossible",
        "no filesystem access",
        "future integration",
    ] {
        assert!(!md.contains(stale), "{stale}");
        assert!(
            n.report.items.iter().all(|i| !i.effect.contains(stale)),
            "{stale}"
        );
    }
    let not_implemented: Vec<&str> = n
        .report
        .items
        .iter()
        .filter(|i| i.status == ItemStatus::NotImplemented)
        .map(|i| i.setting.as_str())
        .collect();
    assert!(
        not_implemented
            .iter()
            .all(|s| !matches!(*s, "enableRss" | "seed criteria" | "client")),
        "{not_implemented:?}"
    );
    assert_eq!(
        n.report.summary.not_implemented_items,
        not_implemented.len()
    );

    assert!(
        arr::rss::feeds(&n.settings)
            .values()
            .flatten()
            .any(|c| *c == 2000)
    );

    // The actual settings decide the effect: RSS off, no seed goal, disabled client.
    let mut off = snapshot();
    off["radarr"]["indexer"][0]["enableRss"] = json!(false);
    off["radarr"]["indexer"][0]["fields"]
        .as_array_mut()
        .unwrap()
        .retain(|f| f["name"] != "seedCriteria.seedRatio");
    off["radarr"]["downloadclient"][0]["enable"] = json!(false);
    let o = normalized(&off);
    let rss = find(&o, "radarr", "enableRss");
    assert_eq!(
        (rss.status, &rss.value),
        (ItemStatus::Enforced, &json!(false))
    );
    assert!(rss.effect.contains("does not read"), "{}", rss.effect);
    assert!(
        arr::rss::feeds(&o.settings)
            .values()
            .flatten()
            .all(|c| !(2000..3000).contains(c)),
        "the movie feed is no longer read"
    );
    assert!(
        find(&o, "radarr", "seed criteria")
            .effect
            .starts_with("no seed goal")
    );
    let client = o
        .report
        .items
        .iter()
        .find(|i| i.app == "radarr" && i.setting == "client")
        .unwrap();
    assert_eq!(client.status, ItemStatus::ObservedOnly);

    // Deterministic: the same snapshot normalises to the same report.
    assert_eq!(
        serde_json::to_string(&normalized(&v).report).unwrap(),
        serde_json::to_string(&n.report).unwrap()
    );
}

/// Re-importing a newer snapshot: new native items appear, native metadata refreshes, and the
/// user's choices (monitoring, removal, added items) are kept.
#[tokio::test]
async fn a_reimport_keeps_the_users_choices_on_snapshot_items() {
    let (addr, _mock) = start_mock().await;
    let mut app = start_app(&snapshot(), "", Some(addr), true).await;
    let call = |rb: reqwest::RequestBuilder| async move {
        let r = rb.send().await.unwrap();
        let t = r.text().await.unwrap();
        serde_json::from_str::<Value>(&t).unwrap_or(Value::Null)
    };
    let movie = |v: &Value, tmdb: u64| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["tmdb_id"] == tmdb)
            .cloned()
    };
    let added = call(app.post("/api/v1/library").json(&json!({ "media_id": "movie:tmdb:900001", "metadata": { "title": "Leviticus", "year": 2025, "runtime_minutes": 86 } }))).await;
    assert_eq!(added["created"], true);
    let patch = |app: &App, path: &str| {
        app.client
            .patch(format!("{}{path}", app.base))
            .bearer_auth(TOKEN)
    };
    call(patch(&app, "/api/v1/library/movie:tmdb:603").json(&json!({ "monitored": false }))).await;
    call(
        app.client
            .delete(format!("{}/api/v1/library/movie:tmdb:194", app.base))
            .bearer_auth(TOKEN),
    )
    .await;
    app.base = serve_dir(&app.dir).await;

    let mut snap = snapshot();
    snap["radarr"]["movie"][0]["title"] = json!("The Matrix (Remastered)");
    snap["radarr"]["movie"][0]["monitored"] = json!(true);
    snap["radarr"]["movie"].as_array_mut().unwrap().push(json!({ "id": 13, "title": "Dune", "originalLanguage": { "id": 1, "name": "English" }, "year": 2021, "runtime": 155, "tmdbId": 438631, "hasFile": false, "monitored": true, "qualityProfileId": 6, "path": "/movies/Dune (2021)", "isAvailable": true }));
    write_state(&app.dir, &snap, "", Some(addr), true);
    app.base = serve_dir(&app.dir).await;
    let v = call(app.get("/api/v1/library/movies")).await;
    assert!(movie(&v, 438631).is_some(), "new snapshot item seeded");
    let matrix = movie(&v, 603).unwrap();
    assert_eq!(matrix["title"], "The Matrix (Remastered)");
    assert_eq!(
        matrix["monitored"], false,
        "user's unmonitor survives the re-import"
    );
    assert!(movie(&v, 194).is_none(), "removal survives the re-import");
    assert!(
        movie(&v, 900001).is_some(),
        "added item survives the re-import"
    );
}
