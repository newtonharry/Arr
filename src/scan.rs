//! Library reconciliation: scans each configured root and updates file evidence for managed
//! items (rename off: `Root/Movie Title (Year)/`, `Root/Series Title/Season N/`).
//!
//! A root that is missing, unreadable, incompletely walked or suspiciously empty (no entries
//! while owned items are recorded there, like native "root folder is empty") changes nothing.
//! A scheduled rescan that would retire more than half of the recorded files of a root (at
//! least 10) is also held back as `suspect_mass_missing`; an explicit `rescan` command applies it. On a healthy root, files found are owned
//! immediately (quality/revision only from explicit file-name evidence), recorded files no
//! longer present (or changed) are retired, while a recorded file the walk could not
//! attribute by name but that is still there, unchanged and symlink-free (for example an
//! episode Plex identified exactly) keeps its evidence, and the items are marked scanned so re-imports never override
//! live evidence. New or changed files are probed lazily (bounded per scan); a failed probe is
//! reported but never drops ownership.

use crate::api::AppState;
use crate::catalog::FileEvidence;
use crate::ids::MediaId;
use crate::import::rfc3339;
use crate::ingest::{self, VIDEO_EXTENSIONS};
use crate::library;
use crate::release::{self, Numbering, QualityBasis};
use crate::settings::MediaKind;
use anyhow::Result;
use rusqlite::{TransactionBehavior, params};
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use time::OffsetDateTime;

const MAX_ENTRIES: usize = 500_000;
const MAX_DEPTH: usize = 4;

#[derive(Debug, Clone, Serialize)]
pub struct RootReport {
    pub name: &'static str,
    pub local: String,
    /// `ok`, `unavailable`, `incomplete`, `empty_suspect` or `not_configured`.
    pub status: &'static str,
    pub detail: Option<String>,
    pub items_scanned: usize,
    pub files: usize,
    pub added: usize,
    pub retired: usize,
    pub unmatched: Vec<String>,
    pub probed: usize,
    pub probe_failures: usize,
}

#[derive(Debug, Clone)]
struct Found {
    rel: String,
    size: u64,
    mtime: i64,
    targets: Vec<MediaId>,
}

struct Item {
    media_id: MediaId,
    folder: String,
    /// Known episodes (series only): (season, episode).
    episodes: BTreeSet<(u16, u16)>,
    /// A file whose own name contradicts this identity is not the item's, whatever folder
    /// it sits in.
    identity: crate::identity::Identity,
}

struct Walk {
    status: &'static str,
    detail: Option<String>,
    found: BTreeMap<MediaId, Vec<Found>>,
    scanned: Vec<MediaId>,
    unmatched: Vec<String>,
    files: usize,
}

fn video(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, e)| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

fn skip_dir(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    name.starts_with('.')
        || [
            "extras",
            "featurettes",
            "behind the scenes",
            "deleted scenes",
            "interviews",
            "scenes",
            "samples",
            "sample",
            "shorts",
            "trailers",
            "other",
            "@eadir",
            "extrafanart",
            "plex versions",
        ]
        .contains(&l.as_str())
}

fn list_videos(
    dir: &Path,
    root: &Path,
    depth: usize,
    out: &mut Vec<(String, u64, i64)>,
    budget: &mut usize,
    errors: &mut Vec<String>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            errors.push(format!(
                "{}: {}",
                dir.strip_prefix(root).unwrap_or(dir).display(),
                e.kind()
            ));
            return;
        }
    };
    for entry in rd {
        if *budget == 0 {
            errors.push("entry limit reached".into());
            return;
        }
        *budget -= 1;
        let Ok(entry) = entry else {
            errors.push(format!("{}: unreadable entry", dir.display()));
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue; // never followed
        }
        if meta.is_dir() {
            if !skip_dir(&name) {
                list_videos(&entry.path(), root, depth + 1, out, budget, errors);
            }
        } else if meta.is_file()
            && video(&name)
            && !name.starts_with("._")
            && !name.starts_with(".um-")
        {
            let lower = name.to_ascii_lowercase();
            if lower
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w == "sample")
            {
                continue;
            }
            if let Ok(rel) = entry.path().strip_prefix(root) {
                out.push((
                    rel.to_string_lossy().to_string(),
                    meta.len(),
                    crate::fsutil::mtime(&meta),
                ));
            }
        }
    }
}

fn walk(root: &Path, items: &[Item], recorded_files: usize) -> Walk {
    let mut w = Walk {
        status: "ok",
        detail: None,
        found: BTreeMap::new(),
        scanned: vec![],
        unmatched: vec![],
        files: 0,
    };
    let top = match std::fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() => std::fs::read_dir(root),
        Ok(_) => {
            w.status = "unavailable";
            w.detail = Some("the root is not a directory".into());
            return w;
        }
        Err(e) => {
            w.status = "unavailable";
            w.detail = Some(format!("the root is not accessible ({})", e.kind()));
            return w;
        }
    };
    let names: BTreeSet<String> = match top {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(e) => {
            w.status = "unavailable";
            w.detail = Some(format!("the root cannot be listed ({})", e.kind()));
            return w;
        }
    };
    if names.is_empty() && recorded_files > 0 {
        w.status = "empty_suspect";
        w.detail = Some(format!(
            "the root is empty but {recorded_files} file(s) are recorded there (unmounted?); nothing changed"
        ));
        return w;
    }
    let mut budget = MAX_ENTRIES;
    let mut errors = vec![];
    let claimed: BTreeSet<&str> = items
        .iter()
        .map(|i| i.folder.split('/').next().unwrap_or(""))
        .collect();
    for item in items {
        let dir = root.join(&item.folder);
        if crate::paths::check_no_symlinks(root, &dir).is_err() {
            errors.push(format!("{}: symbolic link", item.folder));
            continue;
        }
        let mut vids = vec![];
        if dir.is_dir() {
            list_videos(&dir, root, 1, &mut vids, &mut budget, &mut errors);
        }
        let mut found = vec![];
        for (rel, size, mtime) in vids {
            let inner = rel
                .strip_prefix(&format!("{}/", item.folder))
                .unwrap_or(&rel);
            if item.identity.contradiction(inner.split('/')).is_some() {
                if w.unmatched.len() < 20 {
                    w.unmatched.push(format!("{rel} (names another title)"));
                }
                continue;
            }
            let targets = match item.media_id {
                MediaId::Movie { .. } => vec![item.media_id],
                _ => {
                    let base = rel.rsplit('/').next().unwrap_or(&rel);
                    match release::parse(base).numbering {
                        Numbering::Episodes { season, episodes }
                            if episodes
                                .iter()
                                .all(|e| item.episodes.contains(&(season, *e))) =>
                        {
                            episodes
                                .iter()
                                .map(|e| library::episode_id(item.media_id, season, *e))
                                .collect()
                        }
                        _ => {
                            if w.unmatched.len() < 20 {
                                w.unmatched.push(rel.clone());
                            }
                            continue;
                        }
                    }
                }
            };
            found.push(Found {
                rel,
                size,
                mtime,
                targets,
            });
        }
        w.files += found.len();
        w.found.insert(item.media_id, found);
        w.scanned.push(item.media_id);
    }
    for n in &names {
        if !claimed.contains(n.as_str()) && !n.starts_with('.') && w.unmatched.len() < 20 {
            w.unmatched.push(n.clone());
        }
    }
    if !errors.is_empty() {
        w.status = "incomplete";
        w.detail = Some(format!(
            "{} problem(s) while walking, e.g. {}; nothing changed",
            errors.len(),
            errors[0]
        ));
    }
    w
}

/// File evidence from a path relative to its root (quality and revision only when the
/// file name states them explicitly).
pub fn file_evidence(rel: &str, size: u64, now: &str) -> FileEvidence {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let p = release::parse(base);
    let explicit = p.quality_basis == QualityBasis::Parsed;
    FileEvidence {
        path: Some(rel.to_string()),
        size: Some(size),
        quality: explicit.then_some(p.quality),
        revision: explicit.then_some(p.revision),
        source: "scan".into(),
        observed_at: now.to_string(),
        probe: None,
    }
}

/// Scans every configured root. Returns one report per root.
pub async fn scan(st: &AppState, now: OffsetDateTime, explicit: bool) -> Result<Vec<RootReport>> {
    let mut reports = vec![];
    let Some(lib) = st.cfg.library.clone() else {
        return Ok(reports);
    };
    let index = st.index();
    for (name, media) in [("movies", MediaKind::Movie), ("tv", MediaKind::Series)] {
        let Some(root) = ingest::root_for(&lib, media).map(Path::to_path_buf) else {
            reports.push(RootReport {
                name,
                local: String::new(),
                status: "not_configured",
                detail: None,
                items_scanned: 0,
                files: 0,
                added: 0,
                retired: 0,
                unmatched: vec![],
                probed: 0,
                probe_failures: 0,
            });
            continue;
        };
        let replace_illegal = st.current().is_none_or(|l| {
            l.settings
                .media_management(media)
                .is_none_or(|m| m.replace_illegal_characters)
        });
        let mut items = vec![];
        let mut owned_items = 0usize;
        match media {
            MediaKind::Movie => {
                for m in index
                    .catalog
                    .movies
                    .iter()
                    .filter(|m| !m.provenance.source_app.ends_with("(unmanaged)"))
                {
                    if ingest::root_matches(st, media, m.root_folder.as_deref()) {
                        owned_items += usize::from(m.has_file);
                        let folder = m.folder.clone().unwrap_or_else(|| {
                            library::folder_name(&m.title, m.year, replace_illegal)
                        });
                        if !library::valid_folder(&folder) {
                            continue;
                        }
                        items.push(Item {
                            media_id: m.id,
                            folder,
                            episodes: BTreeSet::new(),
                            identity: crate::identity::Identity::movie(m),
                        });
                    }
                }
            }
            MediaKind::Series => {
                for s in index
                    .catalog
                    .series
                    .iter()
                    .filter(|s| !s.provenance.source_app.ends_with("(unmanaged)"))
                {
                    // Without an episode list, files cannot be mapped; the series is skipped.
                    if ingest::root_matches(st, media, s.root_folder.as_deref()) {
                        owned_items += s.episodes.iter().flatten().filter(|e| e.has_file).count()
                            + s.seasons
                                .iter()
                                .map(|x| x.episode_file_count.unwrap_or(0) as usize)
                                .sum::<usize>()
                                .min(1);
                    }
                    if ingest::root_matches(st, media, s.root_folder.as_deref())
                        && s.episodes.is_some()
                    {
                        let folder = s
                            .folder
                            .clone()
                            .unwrap_or_else(|| library::clean_name(&s.title, replace_illegal));
                        if !library::valid_folder(&folder) {
                            continue;
                        }
                        let episodes = s
                            .episodes
                            .iter()
                            .flatten()
                            .map(|e| (e.season, e.number))
                            .collect();
                        items.push(Item {
                            media_id: s.id,
                            folder,
                            episodes,
                            identity: crate::identity::Identity::series(s),
                        });
                    }
                }
            }
        }
        let kind = if media == MediaKind::Movie {
            "movie:%"
        } else {
            "series:%"
        };
        let recorded: i64 = st
            .store
            .with(move |c| Ok(c.query_row("SELECT COUNT(*) FROM files WHERE present = 1 AND path IS NOT NULL AND media_id LIKE ?1", [kind], |r| r.get(0))?))
            .await?;
        let root2 = root.clone();
        let recorded = (recorded as usize).max(owned_items);
        let w = tokio::task::spawn_blocking(move || walk(&root2, &items, recorded)).await?;
        let mut report = RootReport {
            name,
            local: root.display().to_string(),
            status: w.status,
            detail: w.detail.clone(),
            items_scanned: 0,
            files: w.files,
            added: 0,
            retired: 0,
            unmatched: w.unmatched.clone(),
            probed: 0,
            probe_failures: 0,
        };
        if w.status == "ok" {
            let now_s = rfc3339(now);
            let (found, scanned) = (w.found, w.scanned);
            report.items_scanned = scanned.len();
            let root3 = root.clone();
            let outcome = st
                .store
                .with(move |c| apply(c, &root3, &scanned, &found, &now_s, !explicit))
                .await?;
            let (added, retired) = match outcome {
                Ok(x) => x,
                Err((retired, before)) => {
                    report.status = "suspect_mass_missing";
                    report.detail = Some(format!(
                        "the scan would retire {retired} of {before} recorded file(s); nothing changed (an explicit rescan command applies it)"
                    ));
                    (0, 0)
                }
            };
            report.added = added;
            report.retired = retired;
        }
        if report.status == "ok" {
            // Lazy, bounded probes of new or changed files.
            let kind = if media == MediaKind::Movie {
                "movie:%"
            } else {
                "series:%"
            };
            let (probed, failures) =
                probe_pending(st, &root, &lib, kind.to_string(), lib.probes_per_scan).await?;
            report.probed = probed;
            report.probe_failures = failures;
        }
        let r2 = report.clone();
        let now_s = rfc3339(now);
        st.store
            .with(move |c| {
                c.execute(
                    "INSERT INTO roots (name, local, status, scanned_at, files, unmatched, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (name) DO UPDATE SET local = excluded.local, status = excluded.status, scanned_at = excluded.scanned_at, files = excluded.files, unmatched = excluded.unmatched, detail = excluded.detail",
                    params![r2.name, r2.local, r2.status, now_s, r2.files as i64, json!(r2.unmatched).to_string(), r2.detail],
                )?;
                library::event(c, &now_s, "rescan", None, None, None, &format!("{} root {}: {} file(s), {} added, {} retired", r2.name, r2.status, r2.files, r2.added, r2.retired), Some(&json!({ "probed": r2.probed, "probe_failures": r2.probe_failures })))?;
                Ok(())
            })
            .await?;
        reports.push(report);
    }
    st.reload_index().await?;
    Ok(reports)
}

/// Applies a healthy walk: found files are owned (unchanged rows kept, changed ones replaced),
/// recorded files no longer present are retired, the items are marked scanned. With `guard`,
/// retiring more than half of the recorded files (at least 10) is refused:
/// `Err((retired, before))` and nothing is written.
type Applied = std::result::Result<(usize, usize), (usize, usize)>;

/// Whether a recorded file is still present at `root/rel`, unchanged (size and mtime when
/// recorded) and reached without a symbolic link.
fn still_there(root: &Path, rel: Option<&str>, size: Option<i64>, mtime: Option<i64>) -> bool {
    let Some(rel) = rel.and_then(crate::paths::safe_rel) else {
        return false;
    };
    let path = root.join(rel);
    if crate::paths::check_no_symlinks(root, &path).is_err() {
        return false;
    }
    std::fs::symlink_metadata(&path).is_ok_and(|m| {
        m.file_type().is_file()
            && size.is_none_or(|s| s as u64 == m.len())
            && mtime.is_none_or(|t| t == crate::fsutil::mtime(&m))
    })
}

fn apply(
    c: &mut rusqlite::Connection,
    root: &Path,
    scanned: &[MediaId],
    found: &BTreeMap<MediaId, Vec<Found>>,
    now_s: &str,
    guard: bool,
) -> Result<Applied> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (mut added, mut retired) = (0usize, 0usize);
    let mut before = 0usize;
    {
        let mut count =
            tx.prepare("SELECT COUNT(*) FROM files WHERE media_id = ?1 AND present = 1")?;
        for m in scanned {
            before += count.query_row([m.to_string()], |r| r.get::<_, i64>(0))? as usize;
        }
    }
    for media_id in scanned {
        let mid = media_id.to_string();
        let files = found.get(media_id).cloned().unwrap_or_default();
        let mut stmt = tx.prepare(
            "SELECT id, path, size, mtime FROM files WHERE media_id = ?1 AND present = 1",
        )?;
        type Row = (i64, Option<String>, Option<i64>, Option<i64>);
        let rows: Vec<Row> = stmt
            .query_map([&mid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        let mut kept = BTreeSet::new();
        for f in &files {
            match rows.iter().find(|r| r.1.as_deref() == Some(f.rel.as_str())) {
                Some((id, _, size, mtime))
                    if size.is_none_or(|s| s as u64 == f.size)
                        && mtime.is_none_or(|m| m == f.mtime) =>
                {
                    tx.execute(
                        "UPDATE files SET size = ?2, mtime = ?3 WHERE id = ?1",
                        params![id, f.size as i64, f.mtime],
                    )?;
                    kept.insert(*id);
                }
                other => {
                    if let Some((id, ..)) = other {
                        tx.execute("UPDATE files SET present = 0 WHERE id = ?1", [id])?;
                        retired += 1;
                    }
                    let ev = file_evidence(&f.rel, f.size, now_s);
                    let id = library::insert_file(&tx, &mid, &ev, &f.targets, None, Some(f.mtime))?;
                    kept.insert(id);
                    added += 1;
                }
            }
        }
        for (id, path, size, mtime) in rows.iter().filter(|r| !kept.contains(&r.0)) {
            // Not attributed by this walk (its name did not parse, say), but still there and
            // unchanged: the recorded evidence stands.
            if files
                .iter()
                .all(|f| Some(f.rel.as_str()) != path.as_deref())
                && still_there(root, path.as_deref(), *size, *mtime)
            {
                continue;
            }
            tx.execute("UPDATE files SET present = 0 WHERE id = ?1", [id])?;
            retired += 1;
        }
        tx.execute(
            "UPDATE media SET scanned_at = ?2 WHERE id = ?1",
            params![mid, now_s],
        )?;
    }
    if guard && retired >= 10 && retired * 2 > before {
        return Ok(Err((retired, before)));
    }
    tx.commit()?;
    Ok(Ok((added, retired)))
}

fn item_for(
    index: &crate::catalog::CatalogIndex,
    media_id: MediaId,
    replace_illegal: bool,
) -> Option<Item> {
    match media_id {
        MediaId::Movie { tmdb } => {
            let m = index.movie(tmdb)?;
            let folder = m
                .folder
                .clone()
                .unwrap_or_else(|| library::folder_name(&m.title, m.year, replace_illegal));
            library::valid_folder(&folder).then(|| Item {
                media_id: m.id,
                folder,
                episodes: BTreeSet::new(),
                identity: crate::identity::Identity::movie(m),
            })
        }
        other => {
            let s = index.series(other.series_key()?)?;
            let folder = s
                .folder
                .clone()
                .unwrap_or_else(|| library::clean_name(&s.title, replace_illegal));
            let episodes = s
                .episodes
                .iter()
                .flatten()
                .map(|e| (e.season, e.number))
                .collect();
            library::valid_folder(&folder).then(|| Item {
                media_id: s.id,
                folder,
                episodes,
                identity: crate::identity::Identity::series(s),
            })
        }
    }
}

/// Scans one managed item's folder (used before an add-and-search, so files already in the
/// library are owned before anything is planned). Only that folder is walked and probed. An
/// unavailable root, a missing folder or an incomplete walk changes nothing; the status says
/// which.
pub async fn scan_item(
    st: &AppState,
    media_id: MediaId,
    now: OffsetDateTime,
) -> Result<serde_json::Value> {
    let Some(lib) = st.cfg.library.clone() else {
        return Ok(
            json!({ "status": "not_configured", "detail": "no [library] roots are configured" }),
        );
    };
    let media = crate::downloads::media_kind(media_id);
    let Some(root) = ingest::root_for(&lib, media).map(Path::to_path_buf) else {
        return Ok(
            json!({ "status": "not_configured", "detail": format!("no [library] root for {}", media.native_app()) }),
        );
    };
    let index = st.index();
    let root_folder = match media_id {
        MediaId::Movie { tmdb } => index.movie(tmdb).and_then(|m| m.root_folder.clone()),
        other => other
            .series_key()
            .and_then(|k| index.series(k))
            .and_then(|s| s.root_folder.clone()),
    };
    if !ingest::root_matches(st, media, root_folder.as_deref()) {
        return Ok(
            json!({ "status": "root_unmapped", "detail": format!("the item's root folder {root_folder:?} is not the mapped [library] root") }),
        );
    }
    let replace_illegal = st.current().is_none_or(|l| {
        l.settings
            .media_management(media)
            .is_none_or(|m| m.replace_illegal_characters)
    });
    let Some(item) = item_for(&index, media_id, replace_illegal) else {
        return Ok(
            json!({ "status": "not_scannable", "detail": "the item is not managed or has no usable folder" }),
        );
    };
    let folder = item.folder.clone();
    match std::fs::symlink_metadata(&root) {
        Ok(m) if m.is_dir() => {}
        _ => {
            return Ok(
                json!({ "status": "unavailable", "folder": folder, "detail": "the library root is not accessible; nothing changed" }),
            );
        }
    }
    if crate::paths::check_no_symlinks(&root, &root.join(&folder)).is_err() {
        return Ok(
            json!({ "status": "incomplete", "folder": folder, "detail": "the item folder path contains a symbolic link; nothing changed" }),
        );
    }
    if !root.join(&folder).is_dir() {
        return Ok(
            json!({ "status": "no_folder", "folder": folder, "files": 0, "detail": "nothing on disk for this item yet" }),
        );
    }
    let root2 = root.clone();
    let w = tokio::task::spawn_blocking(move || walk(&root2, &[item], 0)).await?;
    if w.status != "ok" {
        return Ok(json!({ "status": w.status, "folder": folder, "detail": w.detail }));
    }
    let now_s = rfc3339(now);
    let (found, scanned, files) = (w.found, w.scanned, w.files);
    let root3 = root.clone();
    let unmatched: Vec<String> = w
        .unmatched
        .into_iter()
        .filter(|u| u.starts_with(&format!("{folder}/")))
        .collect();
    let applied = st
        .store
        .with(move |c| apply(c, &root3, &scanned, &found, &now_s, false))
        .await?;
    let (added, retired) = applied.unwrap_or((0, 0));
    let (probed, failures) =
        probe_pending(st, &root, &lib, media_id.to_string(), lib.probes_per_scan).await?;
    st.reload_index().await?;
    Ok(json!({
        "status": "ok", "folder": folder, "files": files, "added": added, "retired": retired,
        "unmatched": unmatched, "probed": probed, "probe_failures": failures,
    }))
}

/// Probes up to `limit` present, unprobed files whose media id matches `pattern` (SQL LIKE).
/// The measured resolution corrects the recorded quality (media info outranks name tags);
/// a failed probe is recorded but never drops ownership.
pub(crate) async fn probe_pending(
    st: &AppState,
    root: &Path,
    lib: &crate::config::LibraryConfig,
    pattern: String,
    limit: u32,
) -> Result<(usize, usize)> {
    let todo: Vec<(i64, String, Option<String>)> = st
        .store
        .with(move |c| {
            let mut stmt = c.prepare("SELECT id, path, quality FROM files WHERE present = 1 AND probe IS NULL AND path IS NOT NULL AND media_id LIKE ?1 ORDER BY id LIMIT ?2")?;
            let rows = stmt
                .query_map(params![pattern, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await?;
    let (mut probed, mut failures) = (0, 0);
    for (id, rel, quality) in todo {
        let abs = root.join(&rel);
        let result = match crate::probe::validate(&abs, &[root]) {
            Ok(p) => crate::probe::run(&lib.ffprobe, &p, lib.probe_timeout).await,
            Err(e) => Err(e),
        };
        probed += 1;
        let recorded: Option<crate::quality::Quality> =
            quality.and_then(|q| serde_json::from_value(serde_json::Value::String(q)).ok());
        let (text, quality) = match result {
            Ok(pr) => {
                let q = match recorded {
                    Some(q) => crate::quality::effective(Some(q), pr.width, pr.height).ok(),
                    None => None,
                };
                (pr.evidence(), q.map(|q| q.as_str().to_string()))
            }
            Err(e) => {
                failures += 1;
                (
                    format!("failed: {e}"),
                    recorded.map(|q| q.as_str().to_string()),
                )
            }
        };
        st.store
            .with(move |c| {
                Ok(c.execute(
                    "UPDATE files SET probe = ?2, quality = ?3 WHERE id = ?1",
                    params![id, text, quality],
                )
                .map(|_| ())?)
            })
            .await?;
    }
    Ok((probed, failures))
}

/// Root status rows (`GET /library/roots`).
pub fn roots(c: &rusqlite::Connection) -> Result<Vec<serde_json::Value>> {
    let mut stmt = c.prepare(
        "SELECT name, local, status, scanned_at, files, unmatched, detail FROM roots ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "name": r.get::<_, String>(0)?,
                "local": r.get::<_, String>(1)?,
                "status": r.get::<_, String>(2)?,
                "scanned_at": r.get::<_, Option<String>>(3)?,
                "files": r.get::<_, Option<i64>>(4)?,
                "unmatched": serde_json::from_str::<serde_json::Value>(&r.get::<_, Option<String>>(5)?.unwrap_or_else(|| "[]".into())).unwrap_or(json!([])),
                "detail": r.get::<_, Option<String>>(6)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
