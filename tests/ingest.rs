//! Completed-download import against real generated media: ffprobe validation, path safety,
//! no-overwrite, free space, copy fallback, sample durations, exact multi-episode ownership,
//! native revision replacement, and journal crash recovery.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::json;
use std::path::Path;

const OLD: &str = "Parity.Show.S01E01.1080p.WEB-DL.x264-OLD.mkv";

/// A managed library with "Parity Show" (tvdb 999002): three aired episodes, E01 owned by a
/// file on disk (WEBDL-1080p, revision 1) when `owned`.
async fn parity(o: ExecOptions, owned: bool) -> Exec {
    let e = start_exec(o).await;
    if owned {
        place_old_file(&e);
    }
    e.add("series:tvdb:999002").await;
    e
}

fn place_old_file(e: &Exec) -> std::path::PathBuf {
    let p = e.tv.join("Parity Show/Season 1").join(OLD);
    if !p.exists() {
        put(&e.tv, &format!("Parity Show/Season 1/{OLD}"), &mkv(1));
    }
    p
}

/// The native "propers and repacks: prefer and upgrade" setting, as a managed policy.
fn revisions() -> ExecOptions {
    ExecOptions {
        tv_extra: "revision_upgrades = true\n".into(),
        ..Default::default()
    }
}

async fn grab_episode(
    e: &Exec,
    ep: u16,
    title: &str,
    files: Vec<(String, Vec<u8>)>,
) -> (String, String) {
    let (hash, r) = e.publish(title, title, files, 1.5, false);
    e.search_results(&format!("Parity Show S01E{ep:02}"), vec![r]);
    let (s, v, text) = e.json(e.post("/api/v1/plans").json(&json!({ "media_id": format!("episode:tvdb:999002:1:{ep}"), "execute": true, "replan": true }))).await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("not executed: {text}"))
        .to_string();
    e.tick().await;
    (id, hash)
}

fn episode_owned(e: &Exec, n: u16) -> bool {
    let index = e.state.index();
    let s = index.series(arr::ids::SeriesKey::Tvdb(999002)).unwrap();
    s.episodes
        .iter()
        .flatten()
        .find(|x| x.season == 1 && x.number == n)
        .unwrap()
        .has_file
}

#[tokio::test]
async fn unplayable_media_fails_is_blocklisted_and_never_owned() {
    let e = parity(
        ExecOptions {
            tv_extra: "redownload_failed = false\n".into(),
            ..Default::default()
        },
        true,
    )
    .await;
    let full = mkv(3);
    let cases: Vec<(u16, &str, Vec<u8>)> = vec![
        (
            2,
            "Parity.Show.S01E02.1080p.WEB-DL.x264-TRUNC",
            full[..full.len() / 2].to_vec(),
        ),
        (
            3,
            "Parity.Show.S01E03.1080p.WEB-DL.x264-AUDIO",
            audio_only(),
        ),
    ];
    for (ep, title, bytes) in cases {
        let (id, hash) = grab_episode(&e, ep, title, vec![(format!("{title}.mkv"), bytes)]).await;
        e.complete(&hash);
        e.tick().await;
        let g = e.grab_row(&id);
        assert_eq!(
            (g.state.as_str(), g.failure.as_deref()),
            ("failed", Some("invalid_media")),
            "{title}: {:?}",
            g.last_error
        );
        assert!(
            g.last_error
                .as_deref()
                .unwrap()
                .contains("no playable video")
        );
        assert!(!episode_owned(&e, ep), "{title} must not be owned");
        assert!(
            !e.tv
                .join(format!("Parity Show/Season 1/{title}.mkv"))
                .exists()
        );
        let (_, v, text) = e
            .json(e.get("/api/v1/blocklist?media_id=series:tvdb:999002"))
            .await;
        assert!(!text.contains(&hash), "blocklist carries no raw hash");
        assert!(
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["grab_id"] == id.as_str())
        );
        // Removal of failed downloads applies (complete torrent: only once its share limit is met).
        assert_eq!(g.cleanup, "seeding");
        // autoRedownloadFailed is off in the fixture (Sonarr default omitted): no re-search queued.
        assert_eq!(g.retry.as_deref(), Some("none"));
    }
    // Junk with a video extension.
    let (id, hash) = grab_episode(
        &e,
        2,
        "Parity.Show.S01E02.1080p.WEB-DL.x264-JUNK",
        vec![(
            "Parity.Show.S01E02.1080p.WEB-DL.x264-JUNK.mkv".into(),
            vec![0x1a, 0x45, 0xdf, 0xa3, 1, 2, 3, 4],
        )],
    )
    .await;
    e.complete(&hash);
    e.tick().await;
    assert_eq!(e.grab_row(&id).failure.as_deref(), Some("invalid_media"));
}

#[cfg(unix)] // Unix permissions or symlinks
#[tokio::test]
async fn symlinks_existing_destinations_and_free_space_block_without_writing() {
    let e = parity(ExecOptions::default(), true).await;
    // 1. The completed source was replaced by a symlink pointing elsewhere.
    let title = "Parity.Show.S01E02.1080p.WEB-DL.x264-LINK";
    let (id, hash) = grab_episode(&e, 2, title, vec![(format!("{title}.mkv"), mkv(2))]).await;
    e.complete(&hash);
    let src = e.downloads.join(format!("{title}.mkv"));
    let elsewhere = e.downloads.parent().unwrap().join("outside.mkv");
    std::fs::write(&elsewhere, mkv(2)).unwrap();
    std::fs::remove_file(&src).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &src).unwrap();
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "import_blocked");
    assert!(
        g.last_error.as_deref().unwrap().contains("symbolic link")
            || g.last_error
                .as_deref()
                .unwrap()
                .contains("not a regular file"),
        "{:?}",
        g.last_error
    );
    assert!(
        !e.tv
            .join(format!("Parity Show/Season 1/{title}.mkv"))
            .exists()
    );
    let (_, v, _) = e.json(e.get(&format!("/api/v1/queue/{id}"))).await;
    assert_eq!(v["grab"]["status"], "import_blocked");

    // 2. A file with the destination name already exists: never overwritten.
    let title = "Parity.Show.S01E03.1080p.WEB-DL.x264-DUP";
    let existing = e.tv.join(format!("Parity Show/Season 1/{title}.mkv"));
    std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
    std::fs::write(&existing, b"user's own file").unwrap();
    let (id, hash) = grab_episode(&e, 3, title, vec![(format!("{title}.mkv"), mkv(2))]).await;
    e.complete(&hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "import_blocked");
    // The destination is reconciled before importing: the user's file is recognised as the
    // episode (or, if it appeared later, still never overwritten).
    let why = g.last_error.as_deref().unwrap();
    assert!(
        why.contains("already owned") || why.contains("nothing is overwritten"),
        "{why}"
    );
    assert_eq!(std::fs::read(&existing).unwrap(), b"user's own file");
    // Retry after the user moved their file away: imported.
    std::fs::remove_file(&existing).unwrap();
    let (s, v, _) = e.json(e.post(&format!("/api/v1/queue/{id}/retry"))).await;
    assert_eq!((s, v["action"].as_str()), (200, Some("reimport")));
    e.tick().await;
    assert_eq!(e.grab_row(&id).state, "imported");
    assert!(existing.is_file() && episode_owned(&e, 3));

    // 3. Minimum free space far above what the disk has: blocked, nothing written.
    let e = parity(
        ExecOptions {
            tv_extra: "minimum_free_space_mb = 10000000\n".into(),
            ..Default::default()
        },
        true,
    )
    .await;
    let title = "Parity.Show.S01E02.1080p.WEB-DL.x264-SPACE";
    let (id, hash) = grab_episode(&e, 2, title, vec![(format!("{title}.mkv"), mkv(2))]).await;
    e.complete(&hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "import_blocked");
    assert!(
        g.last_error
            .as_deref()
            .unwrap()
            .contains("not enough free space"),
        "{:?}",
        g.last_error
    );
    let leftovers: Vec<_> = walk(&e.tv);
    assert!(
        leftovers
            .iter()
            .all(|p| !p.contains(title) && !p.contains(".um-")),
        "{leftovers:?}"
    );
}

fn walk(dir: &Path) -> Vec<String> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p.display().to_string());
            }
        }
    }
    out
}

#[tokio::test]
async fn copy_fallback_and_native_sample_durations() {
    let e = parity(
        ExecOptions {
            tv_extra: "copy_using_hardlinks = false\n".into(),
            sample_detection: true,
            ..Default::default()
        },
        true,
    )
    .await;
    // A 3-second file for a 50-minute runtime is a sample (native minimum 600 s).
    let title = "Parity.Show.S01E02.1080p.WEB-DL.x264-SHORT";
    let (id, hash) = grab_episode(&e, 2, title, vec![(format!("{title}.mkv"), mkv(3))]).await;
    e.complete(&hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.failure.as_deref(), Some("invalid_media"));
    let err = g.last_error.unwrap();
    assert!(
        err.contains("sample") && err.contains("600 s") && err.contains("sample_detection"),
        "{err}"
    );

    let e = parity(
        ExecOptions {
            tv_extra: "copy_using_hardlinks = false\n".into(),
            ..Default::default()
        },
        true,
    )
    .await;
    let title = "Parity.Show.S01E02.1080p.WEB-DL.x264-COPY";
    let (id, hash) = grab_episode(&e, 2, title, vec![(format!("{title}.mkv"), mkv(2))]).await;
    e.complete(&hash);
    e.tick().await;
    assert_eq!(e.grab_row(&id).state, "imported");
    let dest = e.tv.join(format!("Parity Show/Season 1/{title}.mkv"));
    let src = e.downloads.join(format!("{title}.mkv"));
    assert_ne!(ino(&dest), ino(&src), "copied, not linked");
    assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&src).unwrap());
    assert!(playable(&dest).await);
    let (_, v, _) = e.json(e.get(&format!("/api/v1/queue/{id}"))).await;
    assert_eq!(v["imports"][0]["method"], "copy");
}

#[tokio::test]
async fn season_pack_maps_exact_multi_episode_ownership() {
    // Nothing owned yet in this variant.
    let e = parity(ExecOptions::default(), false).await;
    let name = "Parity.Show.S01.1080p.WEB-DL.x264-UM";
    let files = vec![
        (
            "Parity.Show.S01E01.1080p.WEB-DL.x264-UM.mkv".to_string(),
            mkv(2),
        ),
        (
            "Parity.Show.S01E02E03.1080p.WEB-DL.x264-UM.mkv".to_string(),
            mkv(3),
        ),
        ("Extras/Behind.The.Scenes.mkv".to_string(), mkv(1)),
    ];
    let (hash, r) = e.publish(name, name, files, 4.0, false);
    e.search_results("Parity Show S01", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "season:tvdb:999002:1", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["plan"]["selection"], "season_pack", "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    let g = e.grab_row(&id);
    assert_eq!(g.targets.len(), 3);
    e.tick().await;
    e.complete(&hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "imported", "{:?}", g.last_error);
    assert_eq!(g.imported_files, 2);
    for n in 1..=3 {
        assert!(episode_owned(&e, n), "E{n:02}");
    }
    let season = e.tv.join("Parity Show/Season 1");
    assert!(
        season
            .join("Parity.Show.S01E01.1080p.WEB-DL.x264-UM.mkv")
            .is_file(),
        "season packs keep file names"
    );
    assert!(
        season
            .join("Parity.Show.S01E02E03.1080p.WEB-DL.x264-UM.mkv")
            .is_file()
    );
    assert!(!season.join("Behind.The.Scenes.mkv").exists());
    // One file owns both E02 and E03.
    let rows: Vec<(String, String)> = e
        .state
        .store
        .blocking(|c| {
            let mut stmt = c.prepare("SELECT f.path, t.target FROM files f JOIN file_targets t ON t.file_id = f.id WHERE f.present = 1 AND f.source = 'import' ORDER BY t.target")?;
            Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].0, rows[2].0);
    assert!(rows[1].0.ends_with("S01E02E03.1080p.WEB-DL.x264-UM.mkv"));
}

#[tokio::test]
async fn native_revision_upgrade_replaces_only_the_managed_file() {
    let e = parity(revisions(), true).await;
    let old = place_old_file(&e);
    let old_bytes = std::fs::read(&old).unwrap();
    // Ordinary search of an owned episode: nothing (owned, upgrades off).
    let proper = "Parity.Show.S01E01.PROPER.1080p.WEB-DL.x264-UM";
    let (hash, r) = e.publish(
        proper,
        proper,
        vec![(format!("{proper}.mkv"), mkv(3))],
        1.5,
        false,
    );
    let same = e
        .publish(
            "Parity.Show.S01E01.1080p.WEB-DL.x264-SAME",
            "same",
            vec![(
                "Parity.Show.S01E01.1080p.WEB-DL.x264-SAME.mkv".into(),
                mkv(2),
            )],
            1.5,
            false,
        )
        .1;
    let upgrade = e
        .publish(
            "Parity.Show.S01E01.PROPER.1080p.BluRay.x264-UP",
            "up",
            vec![(
                "Parity.Show.S01E01.PROPER.1080p.BluRay.x264-UP.mkv".into(),
                mkv(2),
            )],
            3.0,
            false,
        )
        .1;
    e.search_results("Parity Show S01E01", vec![r, same, upgrade]);
    let (_, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:999002:1:1" })),
        )
        .await;
    assert_eq!(v["plan"]["state"], "already_satisfied");
    // Explicit revision search: only the exact-quality PROPER is planned.
    let (s, v, text) = e.json(e.post("/api/v1/plans").json(&json!({ "media_id": "episode:tvdb:999002:1:1", "revisions": true, "replan": true, "execute": true }))).await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["plan"]["units"][0]["status"], "planned", "{text}");
    assert!(
        v["plan"]["units"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("revision upgrade")
    );
    assert_eq!(v["plan"]["candidates"].as_array().unwrap().len(), 1);
    assert!(
        v["plan"]["candidates"][0]["title"]
            .as_str()
            .unwrap()
            .contains("PROPER.1080p.WEB-DL")
    );
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    assert!(e.grab_row(&id).targets[0].replaces);
    e.tick().await;
    e.complete(&hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "imported", "{:?}", g.last_error);
    let new = e.tv.join(format!("Parity Show/Season 1/{proper}.mkv"));
    assert!(
        new.is_file() && !old.exists(),
        "the managed file was replaced"
    );
    assert!(
        walk(&e.tv).iter().all(|p| !p.contains(".um-")),
        "no backup or staging leftovers"
    );
    let index = e.state.index();
    let ep = index
        .series(arr::ids::SeriesKey::Tvdb(999002))
        .unwrap()
        .episodes
        .as_ref()
        .unwrap()[0]
        .clone();
    let ev = ep.file.unwrap();
    assert_eq!(
        ev.revision,
        Some(arr::release::Revision {
            version: 2,
            real: 0
        })
    );
    assert_eq!(ev.source, "import");
    let (_, v, _) = e.json(e.get("/api/v1/history?kind=file_replaced")).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    let _ = old_bytes;
}

#[tokio::test]
async fn unknown_revision_fails_closed_and_import_time_recheck_protects_the_original() {
    // Unknown revision in the native evidence: no revision upgrade is planned.
    let e = parity(revisions(), true).await;
    place_old_file(&e);
    e.state
        .store
        .blocking(|c| {
            Ok(c.execute(
                "UPDATE files SET revision_version = NULL, revision_real = NULL WHERE path LIKE '%-OLD.mkv'",
                [],
            )?)
        })
        .unwrap();
    e.state.reload_index().await.unwrap();
    let proper = "Parity.Show.S01E01.PROPER.1080p.WEB-DL.x264-UM";
    let (_, r) = e.publish(
        proper,
        proper,
        vec![(format!("{proper}.mkv"), mkv(3))],
        1.5,
        false,
    );
    e.search_results("Parity Show S01E01", vec![r]);
    let (_, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:999002:1:1", "revisions": true })),
        )
        .await;
    assert_eq!(v["plan"]["state"], "already_satisfied", "{text}");
    assert!(
        v["plan"]["units"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("covers_owned"),
        "{text}"
    );

    // Known revision at plan time, but the evidence changes before import (equal now): refused.
    let e = parity(revisions(), true).await;
    let old = place_old_file(&e);
    let (hash, r) = e.publish(
        proper,
        proper,
        vec![(format!("{proper}.mkv"), mkv(3))],
        1.5,
        false,
    );
    e.search_results("Parity Show S01E01", vec![r]);
    let (_, v, _) = e
        .json(e.post("/api/v1/plans").json(
            &json!({ "media_id": "episode:tvdb:999002:1:1", "revisions": true, "execute": true }),
        ))
        .await;
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    e.state
        .store
        .blocking(|c| {
            Ok(c.execute(
                "UPDATE files SET revision_version = 2 WHERE path LIKE '%-OLD.mkv'",
                [],
            )?)
        })
        .unwrap();
    e.state.reload_index().await.unwrap();
    e.complete(&hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "import_blocked");
    assert!(
        g.last_error.as_deref().unwrap().contains("not higher"),
        "{:?}",
        g.last_error
    );
    assert!(old.is_file(), "original untouched");
}

#[tokio::test]
async fn crash_recovery_restores_originals_and_rolls_back_uncommitted_imports() {
    let mut e = parity(ExecOptions::default(), true).await;
    let old = place_old_file(&e);
    let original = std::fs::read(&old).unwrap();
    let season = old.parent().unwrap().to_path_buf();
    // A replacement interrupted after the original was moved aside and the new file published,
    // but before the database commit.
    let staged_new = season.join("Parity.Show.S01E01.PROPER.1080p.WEB-DL.x264-UM.mkv");
    std::fs::write(&staged_new, mkv(2)).unwrap();
    let backup = season.join(".um-backup-900.orig");
    std::fs::rename(&old, &backup).unwrap();
    // A plain import interrupted while staged (and already linked into place).
    let staged = season.join(".um-import-901.partial");
    std::fs::write(&staged, mkv(2)).unwrap();
    let linked = season.join("Parity.Show.S01E02.1080p.WEB-DL.x264-UM.mkv");
    std::fs::hard_link(&staged, &linked).unwrap();
    // A committed replacement whose backup was not removed yet.
    let leftover = season.join(".um-backup-902.orig");
    std::fs::write(&leftover, b"old").unwrap();
    let tv = e.tv.display().to_string();
    let (n, s, b, l) = (
        ino(&staged_new) as i64,
        ino(&staged) as i64,
        ino(&backup) as i64,
        ino(&leftover) as i64,
    );
    let (staged_s, backup_s, leftover_s) = (
        staged.display().to_string(),
        backup.display().to_string(),
        leftover.display().to_string(),
    );
    e.state
        .store
        .blocking(move |c| {
            c.execute("INSERT INTO grabs (id, media_id, target, release_ref, info_hash, title, category, save_path, remove_completed, remove_failed, origin, payload_kind, payload, state, active, next_attempt_at, created_at, updated_at)
                       VALUES ('grb_000000000000000000000001', 'series:tvdb:999002', 'episode:tvdb:999002:1:1', 'rel_x', ?1, 't', 'tv-sonarr', '/downloads', 1, 1, 'interactive', 'magnet', x'00', 'importing', 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')", ["b".repeat(40)])?;
            let ins = "INSERT INTO import_journal (id, grab_id, source, root, dest, targets, size, method, state, staged, staged_ino, replaced_path, backup, backup_ino, updated_at) VALUES (?1, 'grb_000000000000000000000001', '/x', ?2, ?3, '[]', 1, 'hardlink', ?4, ?5, ?6, ?7, ?8, ?9, '2026-01-01T00:00:00Z')";
            c.execute(ins, rusqlite::params![900, tv, "Parity Show/Season 1/Parity.Show.S01E01.PROPER.1080p.WEB-DL.x264-UM.mkv", "published", Option::<String>::None, n, format!("Parity Show/Season 1/{OLD}"), backup_s, b])?;
            c.execute(ins, rusqlite::params![901, tv, "Parity Show/Season 1/Parity.Show.S01E02.1080p.WEB-DL.x264-UM.mkv", "staged", staged_s, s, Option::<String>::None, Option::<String>::None, Option::<i64>::None])?;
            c.execute(ins, rusqlite::params![902, tv, "Parity Show/Season 1/x.mkv", "committed", Option::<String>::None, Option::<i64>::None, Option::<String>::None, leftover_s, l])?;
            Ok(())
        })
        .unwrap();
    let recovery = e.restart().await;
    let notes: Vec<&str> = recovery["journal"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("journal 900: restored the original")),
        "{recovery}"
    );
    assert!(
        notes.iter().any(|n| n.contains("journal 901: rolled back")),
        "{recovery}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("journal 902: removed the leftover backup")),
        "{recovery}"
    );
    assert_eq!(
        std::fs::read(&old).unwrap(),
        original,
        "original restored byte for byte"
    );
    assert!(
        !staged_new.exists() && !backup.exists(),
        "uncommitted replacement removed"
    );
    assert!(
        !staged.exists() && !linked.exists(),
        "uncommitted import removed (our inode only)"
    );
    assert!(!leftover.exists(), "committed backup cleaned");
    assert_eq!(
        e.grab_row("grb_000000000000000000000001").state,
        "completed",
        "the import will be retried"
    );
    let states: Vec<String> = e
        .state
        .store
        .blocking(|c| {
            let mut stmt = c.prepare("SELECT state FROM import_journal ORDER BY id")?;
            Ok(stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap();
    assert_eq!(states, ["rolled_back", "rolled_back", "backup_removed"]);
    // A second recovery is a no-op.
    let again = e.restart().await;
    assert!(again["journal"].as_array().unwrap().is_empty());
}

#[cfg(unix)] // Unix permissions or symlinks
#[tokio::test]
async fn unreadable_sources_block_instead_of_failing_the_release() {
    use std::os::unix::fs::PermissionsExt;
    let e = parity(ExecOptions::default(), true).await;
    let title = "Parity.Show.S01E02.1080p.WEB-DL.x264-PERM";
    let (id, hash) = grab_episode(&e, 2, title, vec![(format!("{title}.mkv"), mkv(2))]).await;
    e.complete(&hash);
    let src = e.downloads.join(format!("{title}.mkv"));
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&src).is_ok() {
        eprintln!("running with privileges that ignore file modes; skipping");
        return;
    }
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(g.state, "import_blocked", "{:?}", g.last_error);
    assert!(g.last_error.as_deref().unwrap().contains("read access"));
    let (_, v, _) = e.json(e.get("/api/v1/blocklist")).await;
    assert!(v["items"].as_array().unwrap().is_empty(), "not blocklisted");
    // Once readable, a retry imports it.
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();
    e.json(e.post(&format!("/api/v1/queue/{id}/retry"))).await;
    e.tick().await;
    assert_eq!(e.grab_row(&id).state, "imported");
}

#[tokio::test]
async fn a_lower_resolution_proper_is_not_an_exact_quality_revision() {
    let e = parity(revisions(), true).await;
    let old = place_old_file(&e);
    let original = std::fs::read(&old).unwrap();
    // Advertised as the owned WEBDL-1080p with a higher revision; the file is 720p.
    let proper = "Parity.Show.S01E01.PROPER.1080p.WEB-DL.x264-LOW";
    let (hash, r) = e.publish(
        proper,
        proper,
        vec![(format!("{proper}.mkv"), mkv_sized(2, 1280, 720))],
        1.5,
        false,
    );
    e.search_results("Parity Show S01E01", vec![r]);
    let (_, v, text) = e
        .json(e.post("/api/v1/plans").json(
            &json!({ "media_id": "episode:tvdb:999002:1:1", "revisions": true, "execute": true }),
        ))
        .await;
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    e.tick().await;
    e.complete(&hash);
    e.tick().await;
    let g = e.grab_row(&id);
    assert_eq!(
        g.failure.as_deref(),
        Some("wrong_quality"),
        "{:?}",
        g.last_error
    );
    assert!(g.last_error.unwrap().contains("exact quality"));
    assert_eq!(
        std::fs::read(&old).unwrap(),
        original,
        "the owned file is untouched"
    );
    assert!(
        walk(&e.tv)
            .iter()
            .all(|p| !p.contains(".um-") && !p.contains("-LOW"))
    );
}
