//! Completed-download import (native completed download handling with rename off).
//!
//! Files come only from the grab's own torrent, mapped from the client save path into the
//! local download directory; names must be plain relative paths with no symlinked component.
//! Video files are selected (extras/sample folders and sample names skipped), mapped to the
//! grab's exact targets (episode numbering per file), probed with ffprobe (native
//! `enableMediaInfo`), checked against the native sample durations, then hardlinked (same
//! filesystem, `copyUsingHardlinks`) or copied into `Root/Folder[/Season N]/` under the
//! native scene name. Nothing is overwritten. Every file goes through `import_journal`:
//! `planned -> staged -> [backed_up ->] published -> committed [-> backup_removed]`, so a crash
//! at any point is rolled back (or a replacement restored) on restart. A native revision
//! upgrade moves only the old managed file to a sidecar backup until the new file is
//! published and committed. The download's source data is left in place (seeding).

use crate::api::AppState;
use crate::catalog::FileEvidence;
use crate::config::LibraryConfig;
use crate::downloads::{self, Grab, media_kind};
use crate::ids::MediaId;
use crate::import::rfc3339;
use crate::library;
use crate::paths;
use crate::probe::{self, Probe};
use crate::quality::Quality;
use crate::release::{self, Numbering, Revision};
use crate::settings::{MediaKind, MediaManagement};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::OffsetDateTime;

/// Containers the probe accepts (native video extensions that FFmpeg's whitelisted demuxers read).
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mk3d", "webm", "mp4", "m4v", "mov", "avi", "ts", "m2ts", "mpg", "mpeg", "wmv", "asf",
];
/// Other native video extensions: reported, not imported.
const OTHER_VIDEO: &[&str] = &[
    "iso", "img", "vob", "flv", "divx", "xvid", "ogm", "ogv", "rm", "rmvb", "3gp", "wtv", "dvr-ms",
    "strm",
];
/// Folder names skipped like native `ExcludedSubFoldersRegex`/extras folders.
const EXCLUDED_DIRS: &[&str] = &[
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
];
const LEASE: Duration = Duration::from_secs(30 * 60);

pub fn root_for(lib: &LibraryConfig, media: MediaKind) -> Option<&Path> {
    match media {
        MediaKind::Movie => lib.movies.as_deref(),
        MediaKind::Series => lib.tv.as_deref(),
    }
}

/// The native root folder path the local library root stands for.
pub fn native_root(st: &AppState, media: MediaKind) -> Option<String> {
    let lib = st.cfg.library.as_ref()?;
    let explicit = match media {
        MediaKind::Movie => lib.movies_native_root.clone(),
        MediaKind::Series => lib.tv_native_root.clone(),
    };
    if explicit.is_some() {
        return explicit;
    }
    let loaded = st.current()?;
    let roots: Vec<&str> = loaded
        .settings
        .root_folders
        .iter()
        .filter(|r| r.media == media)
        .map(|r| r.path.trim_end_matches('/'))
        .collect();
    match roots.as_slice() {
        [only] => Some(only.to_string()),
        _ => None,
    }
}

/// Whether an item's native root is the mapped one (items without a root use the mapped one).
pub fn root_matches(st: &AppState, media: MediaKind, item_root: Option<&str>) -> bool {
    match (
        item_root.map(|r| r.trim_end_matches('/')),
        native_root(st, media),
    ) {
        (None, Some(_)) => true,
        (Some(r), Some(n)) => r == n,
        (_, None) => false,
    }
}

fn ext_of(name: &str) -> Option<String> {
    let base = name.rsplit('/').next().unwrap_or(name);
    let (_, ext) = base.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

fn sample_name(name: &str) -> bool {
    let stem = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    stem.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| w == "sample")
}

fn excluded(rel: &str) -> bool {
    let mut parts: Vec<&str> = rel.split('/').collect();
    let file = parts.pop().unwrap_or("");
    file.starts_with("._")
        || file.ends_with(".partial~")
        || parts
            .iter()
            .any(|d| d.starts_with('.') || EXCLUDED_DIRS.contains(&d.to_ascii_lowercase().as_str()))
}

/// A torrent file selected for import.
#[derive(Debug, Clone)]
pub struct Planned {
    pub rel: String,
    pub source: PathBuf,
    pub size: u64,
    pub targets: Vec<MediaId>,
    pub dest_dir: PathBuf,
    pub dest_name: String,
    pub replace: Option<Replacement>,
    pub runtime: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Replacement {
    pub file_id: i64,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub file: String,
    pub reason: String,
}

/// File selection and target mapping (pure, testable).
pub struct Selection {
    pub planned: Vec<Planned>,
    pub skipped: Vec<Skipped>,
    /// Problems that block the whole import (ambiguity).
    pub blocker: Option<String>,
}

/// Season -> episode -> (runtime, evidence about the owned file).
pub type EpisodeMap = std::collections::BTreeMap<
    u16,
    std::collections::BTreeMap<u16, (Option<u32>, Option<FileEvidence>)>,
>;

pub struct ItemInfo {
    /// Folder relative to the root (`Title (Year)` or `Series Title`).
    pub folder: String,
    pub runtime: Option<u32>,
    /// Season number -> (episode number -> (runtime, existing evidence, file id)).
    pub episodes: EpisodeMap,
    pub existing: Option<FileEvidence>,
    pub series: Option<MediaId>,
    /// Title/alias/year identity the payload's names must not contradict.
    pub identity: Option<crate::identity::Identity>,
    /// The item's quality profile id.
    pub profile: Option<String>,
    /// The item's original language (for `original` language profiles).
    pub original_language: Option<String>,
}

pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub complete: bool,
    pub wanted: bool,
}

fn scene_name(client_name: &str, ext: &str) -> String {
    let base = client_name.trim();
    match base.rsplit_once('.') {
        Some((stem, e)) if e.eq_ignore_ascii_case(ext) => format!("{stem}.{ext}"),
        _ => format!("{base}.{ext}"),
    }
}

pub fn select(
    g: &Grab,
    files: &[FileEntry],
    local_save: &Path,
    item: &ItemInfo,
    replace_illegal: bool,
) -> Selection {
    let mut skipped = vec![];
    let mut videos: Vec<(&FileEntry, String)> = vec![];
    for f in files {
        let Some(rel) = paths::safe_rel(&f.name) else {
            skipped.push(Skipped {
                file: f.name.chars().take(120).collect(),
                reason: "unsafe file name".into(),
            });
            continue;
        };
        let rel = paths::rel_string(&rel).unwrap_or_else(|| rel.to_string_lossy().into_owned());
        let Some(ext) = ext_of(&rel) else { continue };
        if !VIDEO_EXTENSIONS.contains(&ext.as_str()) {
            if OTHER_VIDEO.contains(&ext.as_str()) {
                skipped.push(Skipped {
                    file: rel,
                    reason: format!(".{ext} is not an importable container"),
                });
            }
            continue;
        }
        if !f.wanted {
            skipped.push(Skipped {
                file: rel,
                reason: "not selected for download in the client".into(),
            });
            continue;
        }
        if excluded(&rel) || sample_name(&rel) {
            skipped.push(Skipped {
                file: rel,
                reason: "sample or extra".into(),
            });
            continue;
        }
        if !f.complete {
            skipped.push(Skipped {
                file: rel,
                reason: "incomplete in the client".into(),
            });
            continue;
        }
        videos.push((f, rel));
    }
    let mut planned: Vec<Planned> = vec![];
    let targets: Vec<MediaId> = g.targets.iter().map(|t| t.target).collect();
    let replaces = |t: &MediaId| g.targets.iter().any(|x| x.target == *t && x.replaces);
    let client_name = g.client_name.clone().unwrap_or_else(|| g.title.clone());
    let single = videos.len() == 1 && !g.full_season;
    let mut blocker = None;
    match g.media_id {
        MediaId::Movie { .. } => match videos.as_slice() {
            [] => {}
            [(_, rel)] if item.existing.is_some() && !replaces(&g.media_id) => {
                skipped.push(Skipped { file: rel.clone(), reason: "the movie already has a file and this download is not a revision replacement".into() });
            }
            [(_, rel)] if item.existing.as_ref().is_some_and(|ev| ev.path.is_none()) => {
                skipped.push(Skipped {
                    file: rel.clone(),
                    reason: "the owned file's path is unknown; revision replacement fails closed"
                        .into(),
                });
            }
            [(f, rel)] => {
                let ext = ext_of(rel).unwrap_or_default();
                let replace = item
                    .existing
                    .as_ref()
                    .and_then(|ev| ev.path.clone())
                    .map(|path| Replacement { file_id: 0, path });
                planned.push(Planned {
                    source: local_save.join(rel),
                    rel: rel.clone(),
                    size: f.size,
                    targets: vec![g.media_id],
                    dest_dir: PathBuf::from(&item.folder),
                    dest_name: library::clean_name(
                        &scene_name(&client_name, &ext),
                        replace_illegal,
                    ),
                    replace,
                    runtime: item.runtime,
                });
            }
            many => {
                blocker = Some(format!(
                    "{} video files in the download; one file per movie is imported (cancel, or remove the extras in the client and retry)",
                    many.len()
                ));
            }
        },
        _ => {
            let mut claimed: std::collections::BTreeMap<MediaId, String> = Default::default();
            for (f, rel) in &videos {
                let base = rel.rsplit('/').next().unwrap_or(rel);
                let parsed = release::parse(base);
                let eps: Vec<MediaId> = match (&parsed.numbering, g.target) {
                    (Numbering::Episodes { season, episodes }, _) => {
                        let Some(series) = item.series else { continue };
                        episodes
                            .iter()
                            .map(|e| library::episode_id(series, *season, *e))
                            .collect()
                    }
                    (Numbering::None, MediaId::Episode(..))
                        if videos.len() == 1 && targets.len() == 1 =>
                    {
                        targets.clone()
                    }
                    _ => {
                        skipped.push(Skipped {
                            file: rel.clone(),
                            reason: "no episode numbering in the file name".into(),
                        });
                        continue;
                    }
                };
                if eps.is_empty() || !eps.iter().all(|e| targets.contains(e)) {
                    skipped.push(Skipped {
                        file: rel.clone(),
                        reason: "covers episodes this download was not grabbed for".into(),
                    });
                    continue;
                }
                if let Some(e) = eps.iter().find(|e| claimed.contains_key(e)) {
                    blocker = Some(format!("two files claim {e} ({} and {rel})", claimed[e]));
                    break;
                }
                for e in &eps {
                    claimed.insert(*e, rel.clone());
                }
                let MediaId::Episode(_, season, number) = eps[0] else {
                    continue;
                };
                let info = item.episodes.get(&season).and_then(|m| m.get(&number));
                let existing: Vec<&FileEvidence> = eps
                    .iter()
                    .filter_map(|e| match e {
                        MediaId::Episode(_, s, n) => item
                            .episodes
                            .get(s)
                            .and_then(|m| m.get(n))
                            .and_then(|x| x.1.as_ref()),
                        _ => None,
                    })
                    .collect();
                let mut replace = None;
                if !existing.is_empty() {
                    let all_replace = eps.iter().all(replaces);
                    let paths: std::collections::BTreeSet<Option<&String>> =
                        existing.iter().map(|ev| ev.path.as_ref()).collect();
                    match (all_replace, paths.len(), existing.len() == eps.len()) {
                        (true, 1, true) => {
                            replace = existing[0]
                                .path
                                .clone()
                                .map(|path| Replacement { file_id: 0, path });
                        }
                        _ => {
                            skipped.push(Skipped {
                                file: rel.clone(),
                                reason: "an episode it covers is already owned and this is not a single-file revision replacement".into(),
                            });
                            continue;
                        }
                    }
                }
                let ext = ext_of(rel).unwrap_or_default();
                let name = if single {
                    scene_name(&client_name, &ext)
                } else {
                    base.to_string()
                };
                planned.push(Planned {
                    source: local_save.join(rel),
                    rel: rel.clone(),
                    size: f.size,
                    targets: eps,
                    dest_dir: PathBuf::from(&item.folder).join(format!("Season {season}")),
                    dest_name: library::clean_name(&name, replace_illegal),
                    replace,
                    runtime: info.and_then(|i| i.0).or(item.runtime),
                });
            }
        }
    }
    Selection {
        planned,
        skipped,
        blocker,
    }
}

/// Result of one import attempt.
#[derive(Debug)]
pub enum Outcome {
    Imported {
        files: usize,
        skipped: Vec<Skipped>,
        missing: Vec<MediaId>,
    },
    /// Not importable now; stays active (retried with backoff or on request).
    Blocked(String),
    /// The download's media is unusable: fail and blocklist.
    /// (failure code, reason): `invalid_media`, `wrong_payload`, `wrong_language` or
    /// `wrong_quality`.
    Failed(&'static str, String),
    /// The torrent stopped being ours.
    Released(String),
    /// The torrent is not complete after all.
    NotReady,
}

/// Imports every due completed grab (bounded per pass).
pub async fn import_due(st: &AppState, now: OffsetDateTime) -> Result<usize> {
    let now_s = rfc3339(now);
    let due = st
        .store
        .with(move |c| {
            downloads::select(
                c,
                "state = 'completed' AND next_attempt_at <= ?1 ORDER BY completed_at LIMIT 5",
                &[&now_s],
            )
        })
        .await?;
    let mut done = 0;
    for g in due {
        let _guard = st.worker.grabs.lock().await;
        let (id, owner, until) = (
            g.id.clone(),
            st.worker.instance.clone(),
            rfc3339(now + LEASE),
        );
        let leased = st
            .store
            .with(move |c| {
                Ok(c.execute(
                    "UPDATE grabs SET state = 'importing', lease_owner = ?2, lease_until = ?3 WHERE id = ?1 AND state = 'completed'",
                    params![id, owner, until],
                )?)
            })
            .await?;
        if leased == 0 {
            continue;
        }
        let outcome = match import_grab(st, &g, now).await {
            Ok(o) => o,
            Err(e) => Outcome::Blocked(format!("{e:#}")),
        };
        if apply(st, &g, outcome, now).await? {
            done += 1;
        }
    }
    Ok(done)
}

async fn apply(st: &AppState, g: &Grab, outcome: Outcome, now: OffsetDateTime) -> Result<bool> {
    let id = g.id.clone();
    let now_s = rfc3339(now);
    let attempts = g.attempts;
    match outcome {
        Outcome::Imported {
            files,
            skipped,
            missing,
        } => {
            let (cleanup, detail) = downloads::after_import_cleanup(g);
            let note = if missing.is_empty() {
                None
            } else {
                Some(format!(
                    "imported {files} file(s); not imported: {}",
                    missing
                        .iter()
                        .map(|m| m.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            };
            let (media_s, target_s, title) = (
                g.media_id.to_string(),
                g.target.to_string(),
                g.title.clone(),
            );
            let skipped_json = json!(skipped);
            let plex = st.plex.is_some();
            let media = g.media_id;
            st.store
                .with(move |c| {
                    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    tx.execute(
                        "UPDATE grabs SET state = 'imported', active = 0, imported_at = ?2, updated_at = ?2, lease_owner = NULL, lease_until = NULL, last_error = ?3, cleanup = ?4, cleanup_detail = ?5 WHERE id = ?1",
                        params![id, now_s, note, cleanup, detail],
                    )?;
                    library::event(&tx, &now_s, "import_completed", Some(&media_s), Some(&target_s), Some(&id), &format!("{title}: {files} file(s) imported"), Some(&json!({ "skipped": skipped_json, "missing": missing })))?;
                    if plex {
                        crate::worker::queue_command(&tx, "plex_refresh", Some(media), &now_s)?;
                    }
                    tx.commit()?;
                    Ok(())
                })
                .await?;
            if let (Some(qb), Some(l)) = (&st.client, st.current())
                && let Ok(p) = downloads::client_policy(st, &l.settings, media_kind(g.media_id))
                && let Some(cat) = p.post_import_category
            {
                let _ = qb.set_category(&g.info_hash, &cat).await;
            }
            st.reload_index().await?;
            st.worker.notify.notify_one();
            Ok(true)
        }
        Outcome::Blocked(reason) => {
            let next = rfc3339(
                now + Duration::from_secs((300u64 << attempts.clamp(0, 6) as u32).min(6 * 3600)),
            );
            let reason2 = reason.clone();
            st.store
                .with(move |c| {
                    c.execute(
                        "UPDATE grabs SET state = 'import_blocked', last_error = ?2, attempts = attempts + 1, next_attempt_at = ?3, updated_at = ?4, lease_owner = NULL, lease_until = NULL WHERE id = ?1",
                        params![id, reason2, next, now_s],
                    )?;
                    Ok(())
                })
                .await?;
            if g.last_error.as_deref() != Some(reason.as_str()) {
                let (media_s, target_s, gid) =
                    (g.media_id.to_string(), g.target.to_string(), g.id.clone());
                let at = rfc3339(now);
                st.store
                    .with(move |c| {
                        library::event(
                            c,
                            &at,
                            "import_blocked",
                            Some(&media_s),
                            Some(&target_s),
                            Some(&gid),
                            &reason,
                            None,
                        )
                    })
                    .await?;
            }
            Ok(false)
        }
        Outcome::Failed(code, reason) => {
            st.store
                .with(move |c| {
                    c.execute("UPDATE grabs SET state = 'downloading' WHERE id = ?1 AND state = 'importing'", [id])?;
                    Ok(())
                })
                .await?;
            let gid = g.id.clone();
            let fresh = st
                .store
                .with(move |c| downloads::get(c, &gid))
                .await?
                .context("grab vanished")?;
            downloads::fail(st, &fresh, code, &reason, true, now).await?;
            Ok(false)
        }
        Outcome::Released(reason) => {
            st.store
                .with(move |c| {
                    c.execute(
                        "UPDATE grabs SET state = 'released', active = 0, cleanup = 'kept', cleanup_detail = ?2, updated_at = ?3, lease_owner = NULL, lease_until = NULL WHERE id = ?1",
                        params![id, reason, now_s],
                    )?;
                    Ok(())
                })
                .await?;
            st.reload_index().await?;
            Ok(false)
        }
        Outcome::NotReady => {
            st.store
                .with(move |c| {
                    c.execute("UPDATE grabs SET state = 'downloading', lease_owner = NULL, lease_until = NULL, updated_at = ?2 WHERE id = ?1", params![id, now_s])?;
                    Ok(())
                })
                .await?;
            Ok(false)
        }
    }
}

fn item_info(st: &AppState, g: &Grab, mm: &MediaManagement) -> Result<ItemInfo, String> {
    let index = st.index();
    match g.media_id {
        MediaId::Movie { tmdb } => {
            let m = index
                .movie(tmdb)
                .ok_or("the movie is no longer in the managed catalog")?;
            if !root_matches(st, MediaKind::Movie, m.root_folder.as_deref()) {
                return Err(format!(
                    "the movie's root folder {:?} is not the mapped [library] movies root",
                    m.root_folder
                ));
            }
            let folder = m.folder.clone().unwrap_or_else(|| {
                library::folder_name(&m.title, m.year, mm.replace_illegal_characters)
            });
            if !library::valid_folder(&folder) {
                return Err(format!("the movie folder {folder:?} is not usable"));
            }
            Ok(ItemInfo {
                folder,
                runtime: m.runtime_minutes,
                episodes: Default::default(),
                existing: m.file.clone(),
                series: None,
                identity: Some(crate::identity::Identity::movie(m)),
                profile: m.quality_profile.clone(),
                original_language: m.original_language.clone(),
            })
        }
        MediaId::Series(key) => {
            let s = index
                .series(key)
                .ok_or("the series is no longer in the managed catalog")?;
            if !root_matches(st, MediaKind::Series, s.root_folder.as_deref()) {
                return Err(format!(
                    "the series' root folder {:?} is not the mapped [library] tv root",
                    s.root_folder
                ));
            }
            let folder = s
                .folder
                .clone()
                .unwrap_or_else(|| library::clean_name(&s.title, mm.replace_illegal_characters));
            if !library::valid_folder(&folder) {
                return Err(format!("the series folder {folder:?} is not usable"));
            }
            let mut episodes: EpisodeMap = Default::default();
            for e in s.episodes.iter().flatten() {
                let ev = if e.has_file {
                    e.file.clone().or(Some(FileEvidence {
                        path: None,
                        size: None,
                        quality: e.file_quality,
                        revision: None,
                        source: "snapshot".into(),
                        observed_at: String::new(),
                        probe: None,
                    }))
                } else {
                    None
                };
                episodes
                    .entry(e.season)
                    .or_default()
                    .insert(e.number, (e.runtime_minutes, ev));
            }
            Ok(ItemInfo {
                folder,
                runtime: s.runtime_minutes,
                episodes,
                existing: None,
                series: Some(s.id),
                identity: Some(crate::identity::Identity::series(s)),
                profile: s.quality_profile.clone(),
                original_language: s.original_language.clone(),
            })
        }
        _ => Err("grab media id is not a movie or series".into()),
    }
}

async fn import_grab(st: &AppState, g: &Grab, now: OffsetDateTime) -> Result<Outcome> {
    let Some(l) = st.current() else {
        return Ok(Outcome::Blocked("no configuration import is active".into()));
    };
    let media = media_kind(g.media_id);
    let blockers = l.settings.import_blockers(media);
    if !blockers.is_empty() {
        let codes: Vec<&str> = blockers.iter().map(|b| b.code.as_str()).collect();
        return Ok(Outcome::Blocked(format!(
            "import_unsupported: imported settings block file import ({})",
            codes.join(", ")
        )));
    }
    let Some(mm) = l.settings.media_management(media).cloned() else {
        return Ok(Outcome::Blocked(
            "the active import has no media-management settings; re-import the snapshot".into(),
        ));
    };
    let (Some(lib), Some((save_root, local_root)), Some(qb)) = (
        st.cfg.library.clone(),
        st.cfg.client_paths(),
        st.client.clone(),
    ) else {
        return Ok(Outcome::Blocked(
            "[library] or the download client is not configured".into(),
        ));
    };
    let local_root = local_root.to_path_buf();
    let cname = match qb.as_ref() {
        crate::client::Client::Embedded(_) => "the embedded engine",
        crate::client::Client::Qbit(_) => "qBittorrent",
    };
    let Some(root) = root_for(&lib, media).map(Path::to_path_buf) else {
        return Ok(Outcome::Blocked(format!(
            "no [library] root for {}",
            media.native_app()
        )));
    };
    let info = match qb.info(std::slice::from_ref(&g.info_hash)).await {
        Ok(list) => list
            .into_iter()
            .find(|t| t.hash.eq_ignore_ascii_case(&g.info_hash)),
        Err(e) => return Ok(Outcome::Blocked(format!("{} {}", qb.label(), e.describe()))),
    };
    let Some(t) = info else {
        return Ok(Outcome::Blocked(format!("the torrent is not in {cname}")));
    };
    let post = downloads::client_policy(st, &l.settings, media)
        .ok()
        .and_then(|p| p.post_import_category);
    if !(t.category == g.category || post.as_deref() == Some(t.category.as_str()))
        || !t.has_tag(downloads::TAG)
    {
        return Ok(Outcome::Released(format!(
            "the torrent's category or unified-media tag was changed in {cname}; it is no longer managed"
        )));
    }
    // Only data the client reports as verified and complete is imported: a torrent being
    // checked (for the embedded engine: its full piece check after completion or a restart)
    // or in error waits, even at 100 %.
    if t.progress < 1.0 || t.amount_left > 0 || !t.state().complete() {
        return Ok(Outcome::NotReady);
    }
    let Some(local_save) = paths::client_to_local(save_root, &local_root, &t.save_path) else {
        return Ok(Outcome::Blocked(format!(
            "the torrent's save path is outside the mapped {cname} download directory"
        )));
    };
    let files = match qb.files(&g.info_hash).await {
        Ok(Some(f)) => f,
        Ok(None) => return Ok(Outcome::Blocked(format!("the torrent is not in {cname}"))),
        Err(e) => return Ok(Outcome::Blocked(format!("{} {}", qb.label(), e.describe()))),
    };
    let entries: Vec<FileEntry> = files
        .iter()
        .map(|f| FileEntry {
            name: f.name.clone(),
            size: f.size.max(0) as u64,
            complete: f.progress >= 1.0,
            wanted: f.priority != 0,
        })
        .collect();
    // Reconcile the destination right before planning: a file another application (for
    // example the native app) put into the item's folder since the last scan is recognised
    // now, so this download never adds a second copy. Covered targets are then skipped, or
    // the grab waits as import_blocked; revision replacements still see the owned file.
    if let Err(e) = crate::scan::scan_item(st, g.media_id, now).await {
        return Ok(Outcome::Blocked(format!(
            "the destination folder could not be reconciled before importing: {e:#}"
        )));
    }
    let item = match item_info(st, g, &mm) {
        Ok(i) => i,
        Err(e) => return Ok(Outcome::Blocked(e)),
    };
    let mut sel = select(
        g,
        &entries,
        &local_save,
        &item,
        mm.replace_illegal_characters,
    );
    if let Some(b) = sel.blocker {
        return Ok(Outcome::Blocked(b));
    }
    // The payload's own names must not contradict the item (whatever the search result
    // claimed). Obfuscated names are judged by the validated grab context.
    if let Some(id) = &item.identity {
        let torrent_name = g.client_name.clone().unwrap_or_default();
        let bad = id.contradiction([torrent_name.as_str()]).or_else(|| {
            sel.planned
                .iter()
                .find_map(|p| id.contradiction(p.rel.split('/')))
        });
        if let Some(why) = bad {
            return Ok(Outcome::Failed(
                "wrong_payload",
                format!("{why}: the download is not the requested item"),
            ));
        }
    }
    // Explicit language markers in the payload's own names (file first, then the download
    // name) are judged by the profile like release titles. Unmarked names rely on the
    // approved release; stream languages are not guessed.
    if let Some(prof) = item
        .profile
        .as_deref()
        .and_then(|id| l.settings.profile(id))
        && let Some(required) =
            crate::decision::required_languages(prof, item.original_language.as_deref())
    {
        let allowed = match required {
            Ok(a) => a,
            Err(why) => return Ok(Outcome::Blocked(why)),
        };
        let torrent_name = g.client_name.clone().unwrap_or_default();
        for p in &sel.planned {
            let file = p.rel.rsplit('/').next().unwrap_or(&p.rel);
            let file = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
            for name in [file, torrent_name.as_str()] {
                match crate::decision::explicit_language(&allowed, &release::parse(name)) {
                    crate::decision::Explicit::Allowed => break,
                    crate::decision::Explicit::Rejected(_, why) => {
                        return Ok(Outcome::Failed(
                            "wrong_language",
                            format!("{}: the payload is {why}", p.rel),
                        ));
                    }
                    crate::decision::Explicit::Unmarked => {}
                }
            }
        }
    }
    // Replacement targets: the present managed file row and a fresh revision check.
    let policy_on = l
        .settings
        .policy(media)
        .is_some_and(|p| p.revision_upgrades);
    let grab_quality: Option<Quality> = g
        .quality
        .as_deref()
        .and_then(|q| serde_json::from_value(json!(q)).ok());
    let grab_revision = g.revision.unwrap_or_default();
    let media_s = g.media_id.to_string();
    for p in sel.planned.iter_mut() {
        let Some(r) = p.replace.as_mut() else {
            continue;
        };
        let ev = match p.targets[0] {
            MediaId::Movie { .. } => item.existing.clone(),
            MediaId::Episode(_, s, n) => item
                .episodes
                .get(&s)
                .and_then(|m| m.get(&n))
                .and_then(|x| x.1.clone()),
            _ => None,
        };
        let check = if !policy_on {
            Err("revision upgrades are not enabled by the imported policy".to_string())
        } else {
            grab_quality
                .ok_or_else(|| "the grab's quality is unknown".to_string())
                .and_then(|q| crate::decision::revision_rule(ev.as_ref(), q, grab_revision))
        };
        let (path, ms) = (r.path.clone(), media_s.clone());
        let row: Option<(i64, Vec<String>)> = st
            .store
            .with(move |c| {
                let id: Option<i64> = c
                    .query_row(
                        "SELECT id FROM files WHERE media_id = ?1 AND path = ?2 AND present = 1",
                        params![ms, path],
                        |r| r.get(0),
                    )
                    .optional()?;
                let Some(id) = id else { return Ok(None) };
                let mut stmt = c.prepare("SELECT target FROM file_targets WHERE file_id = ?1")?;
                let targets = stmt
                    .query_map([id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(Some((id, targets)))
            })
            .await?;
        let covered = row.as_ref().is_some_and(|(_, t)| {
            t.iter()
                .all(|x| p.targets.iter().any(|y| y.to_string() == *x))
        });
        match (check, row) {
            (Ok(()), Some((id, _))) if covered => r.file_id = id,
            (Err(e), _) => {
                return Ok(Outcome::Blocked(format!(
                    "revision replacement of {} refused at import: {e}",
                    p.rel
                )));
            }
            _ => {
                return Ok(Outcome::Blocked(format!(
                    "revision replacement of {} refused: the owned file is not a present managed file covering exactly these targets",
                    p.rel
                )));
            }
        }
    }
    if sel.planned.is_empty() {
        let reasons: Vec<String> = sel
            .skipped
            .iter()
            .map(|s| format!("{}: {}", s.file, s.reason))
            .collect();
        return Ok(Outcome::Blocked(format!(
            "no importable video file ({})",
            if reasons.is_empty() {
                "no video files".into()
            } else {
                reasons.join("; ")
            }
        )));
    }
    // Probe every source file before touching the library.
    let roots = [local_root.clone()];
    let mut probes: Vec<Probe> = vec![];
    let mut media_failures = vec![];
    let mut ok_planned = vec![];
    for p in sel.planned {
        let canonical = match probe::validate(&p.source, &[roots[0].as_path()]) {
            Ok(c) => c,
            Err(e) => return Ok(Outcome::Blocked(format!("{}: {e}", p.rel))),
        };
        // An unreadable file is a permissions problem, not bad media: never blocklist for it.
        if let Err(e) = std::fs::File::open(&canonical) {
            return Ok(Outcome::Blocked(format!(
                "{}: cannot read the downloaded file ({}); give arr read access to the download directory",
                p.rel,
                e.kind()
            )));
        }
        match probe::run(&lib.ffprobe, &canonical, lib.probe_timeout).await {
            Ok(pr) => {
                let min = p
                    .runtime
                    .filter(|_| lib.sample_detection)
                    .map(probe::minimum_duration_secs);
                if let Some(min) = min
                    && pr.duration_secs < f64::from(min)
                {
                    media_failures.push(format!("{}: sample ({:.0} s is below the native {min} s minimum for a {} min runtime; [library] sample_detection = false disables this)", p.rel, pr.duration_secs, p.runtime.unwrap_or(0)));
                    continue;
                }
                probes.push(pr);
                ok_planned.push(p);
            }
            Err(e) => media_failures.push(format!("{}: {e}", p.rel)),
        }
    }
    if ok_planned.is_empty() {
        return Ok(Outcome::Failed(
            "invalid_media",
            format!("no playable video: {}", media_failures.join("; ")),
        ));
    }
    for f in &media_failures {
        sel.skipped.push(Skipped {
            file: f.split(':').next().unwrap_or("").to_string(),
            reason: f.clone(),
        });
    }
    // Effective quality: the release's source with the resolution measured from the file
    // (media info outranks name tags, as natively). The profile and the exact-quality
    // revision rule apply to it, and it is what gets recorded.
    let profile = item
        .profile
        .as_deref()
        .and_then(|id| l.settings.profile(id));
    let mut qualities = vec![];
    let named_quality = |name: &str| {
        let base = name.rsplit('/').next().unwrap_or(name);
        let stem = base
            .rsplit_once('.')
            .filter(|(_, e)| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
            .map(|(s, _)| s)
            .unwrap_or(base);
        let parsed = release::parse(stem);
        (parsed.quality_basis == release::QualityBasis::Parsed).then_some(parsed.quality)
    };
    let client_name = g.client_name.clone().unwrap_or_default();
    for (p, pr) in ok_planned.iter().zip(&probes) {
        // The payload's own explicit source/quality markers (file, then download name) outrank
        // the indexer's claim; the measured resolution outranks every name.
        let named = named_quality(&p.rel).or_else(|| named_quality(&client_name));
        let eff = match crate::quality::effective(named.or(grab_quality), pr.width, pr.height) {
            Ok(q) => q,
            Err(e) => return Ok(Outcome::Failed("wrong_quality", format!("{}: {e}", p.rel))),
        };
        let advertised = grab_quality
            .map(|q| q.to_string())
            .unwrap_or_else(|| "unknown".into());
        if let Some(prof) = profile
            && prof.tier_of(eff).is_none()
        {
            return Ok(Outcome::Failed(
                "wrong_quality",
                format!(
                    "{}: the file is {eff} ({}x{}), which profile '{}' does not allow (advertised as {advertised})",
                    p.rel, pr.width, pr.height, prof.name
                ),
            ));
        }
        if p.replace.is_some() && Some(eff) != grab_quality {
            return Ok(Outcome::Failed(
                "wrong_quality",
                format!(
                    "{}: the file is {eff} ({}x{}), not the advertised {advertised}; a revision upgrade needs the owned file's exact quality",
                    p.rel, pr.width, pr.height
                ),
            ));
        }
        qualities.push(eff);
    }
    let mut imported = 0;
    let mut covered: Vec<MediaId> = vec![];
    for ((p, pr), eff) in ok_planned.iter().zip(&probes).zip(&qualities) {
        match import_file(
            st,
            g,
            &root,
            &lib,
            &mm,
            p,
            pr,
            Some(*eff),
            grab_revision,
            now,
        )
        .await
        {
            Ok(()) => {
                imported += 1;
                covered.extend(p.targets.iter().copied());
            }
            Err(FileError::Blocked(e)) => {
                return Ok(if imported == 0 {
                    Outcome::Blocked(e)
                } else {
                    Outcome::Blocked(format!("{imported} file(s) imported; then: {e}"))
                });
            }
            Err(FileError::Internal(e)) => return Err(e),
        }
    }
    let missing: Vec<MediaId> = g
        .targets
        .iter()
        .map(|t| t.target)
        .filter(|t| !covered.contains(t))
        .collect();
    Ok(Outcome::Imported {
        files: imported,
        skipped: sel.skipped,
        missing,
    })
}

enum FileError {
    Blocked(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for FileError {
    fn from(e: anyhow::Error) -> Self {
        FileError::Internal(e)
    }
}

struct Staged {
    journal: i64,
    staged: PathBuf,
    dest: PathBuf,
    dest_dir: PathBuf,
    method: &'static str,
}

#[allow(clippy::too_many_arguments)]
async fn import_file(
    st: &AppState,
    g: &Grab,
    root: &Path,
    lib: &LibraryConfig,
    mm: &MediaManagement,
    p: &Planned,
    source_probe: &Probe,
    quality: Option<Quality>,
    revision: Revision,
    now: OffsetDateTime,
) -> Result<(), FileError> {
    // Phase 1 (blocking): directories, conflicts, free space, journal, staging.
    let store = st.store.clone();
    let (gid, root2, p2) = (g.id.clone(), root.to_path_buf(), p.clone());
    let hardlinks = mm.copy_using_hardlinks;
    let min_free = mm.minimum_free_space_mb.saturating_mul(1024 * 1024);
    let quality_s = quality.map(|q| q.as_str().to_string());
    let staged = tokio::task::spawn_blocking(move || {
        stage(
            &store, &gid, &root2, &p2, hardlinks, min_free, quality_s, revision,
        )
    })
    .await
    .map_err(|e| FileError::Internal(e.into()))??;
    // Phase 2: a copy is a new file and is probed again; a hardlink is the probed inode.
    let probe_text = if staged.method == "copy" {
        let canonical = probe::validate(&staged.staged, &[root]).map_err(FileError::Blocked)?;
        match probe::run(&lib.ffprobe, &canonical, lib.probe_timeout).await {
            Ok(pr) => pr.evidence(),
            Err(e) => {
                let (store, j, path, why) = (
                    st.store.clone(),
                    staged.journal,
                    staged.staged.clone(),
                    format!("copied file failed the media probe: {e}"),
                );
                tokio::task::spawn_blocking(move || rollback_staged(&store, j, &path, &why))
                    .await
                    .map_err(|e| FileError::Internal(e.into()))??;
                return Err(FileError::Blocked(format!(
                    "the copied file failed the media probe: {e}"
                )));
            }
        }
    } else {
        source_probe.evidence()
    };
    // Phase 3 (blocking): backup a replaced file, publish without clobbering, commit, clean up.
    let store = st.store.clone();
    let (g2, root2, p2) = (g.clone(), root.to_path_buf(), p.clone());
    let quality_s = quality.map(|q| q.as_str().to_string());
    tokio::task::spawn_blocking(move || {
        publish(
            &store,
            &g2,
            &root2,
            &p2,
            &staged,
            &probe_text,
            quality_s,
            revision,
            now,
        )
    })
    .await
    .map_err(|e| FileError::Internal(e.into()))??;
    Ok(())
}

fn journal_set(c: &Connection, id: i64, state: &str, error: Option<&str>) -> Result<()> {
    c.execute(
        "UPDATE import_journal SET state = ?2, error = COALESCE(?3, error), updated_at = ?4 WHERE id = ?1",
        params![id, state, error, rfc3339(OffsetDateTime::now_utc())],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stage(
    store: &crate::store::Store,
    grab_id: &str,
    root: &Path,
    p: &Planned,
    hardlinks: bool,
    min_free: u64,
    quality: Option<String>,
    revision: Revision,
) -> Result<Staged, FileError> {
    let src_meta = std::fs::symlink_metadata(&p.source).map_err(|e| {
        FileError::Blocked(format!("{}: cannot stat the source: {}", p.rel, e.kind()))
    })?;
    if !src_meta.file_type().is_file() {
        return Err(FileError::Blocked(format!(
            "{} is not a regular file",
            p.rel
        )));
    }
    if p.size > 0 && src_meta.len() != p.size {
        return Err(FileError::Blocked(format!(
            "{}: size on disk differs from the torrent",
            p.rel
        )));
    }
    let dest_dir = paths::ensure_dirs(root, &p.dest_dir).map_err(FileError::Blocked)?;
    let dest = dest_dir.join(&p.dest_name);
    let replaced_abs = p.replace.as_ref().map(|r| root.join(&r.path));
    if std::fs::symlink_metadata(&dest).is_ok() && replaced_abs.as_deref() != Some(dest.as_path()) {
        return Err(FileError::Blocked(format!(
            "{} already exists in the library; nothing is overwritten",
            dest.strip_prefix(root).unwrap_or(&dest).display()
        )));
    }
    let (dest_dev, _) = crate::fsutil::file_id(&dest_dir)
        .map_err(|e| FileError::Blocked(format!("cannot stat the destination: {}", e.kind())))?;
    let same_fs = crate::fsutil::file_id(&p.source).is_ok_and(|(dev, _)| dev == dest_dev);
    let mut method = if hardlinks && same_fs {
        "hardlink"
    } else {
        "copy"
    };
    let free = paths::free_bytes(&dest_dir).unwrap_or(0);
    let needed = if method == "copy" { src_meta.len() } else { 0 };
    if free < needed.saturating_add(min_free) {
        return Err(FileError::Blocked(format!(
            "not enough free space in the library ({} MiB free; {} MiB needed plus the {} MiB minimum)",
            free / 1_048_576,
            needed / 1_048_576,
            min_free / 1_048_576
        )));
    }
    let targets =
        serde_json::to_string(&p.targets.iter().map(|t| t.to_string()).collect::<Vec<_>>())
            .map_err(|e| FileError::Internal(e.into()))?;
    let dest_rel = dest
        .strip_prefix(root)
        .ok()
        .and_then(paths::rel_string)
        .ok_or_else(|| FileError::Blocked("destination escaped the root".into()))?;
    let journal = store.blocking(|c| {
        c.execute(
            "INSERT INTO import_journal (grab_id, source, root, dest, targets, size, method, state, replaced_file_id, replaced_path, quality, revision_version, revision_real, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'planned', ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                grab_id,
                p.source.to_string_lossy(),
                root.to_string_lossy(),
                dest_rel,
                targets,
                src_meta.len() as i64,
                method,
                p.replace.as_ref().map(|r| r.file_id),
                p.replace.as_ref().map(|r| r.path.clone()),
                quality,
                i64::from(revision.version),
                i64::from(revision.real),
                rfc3339(OffsetDateTime::now_utc())
            ],
        )?;
        let id = c.last_insert_rowid();
        let staged = dest_dir.join(format!(".um-import-{id}.partial"));
        c.execute("UPDATE import_journal SET staged = ?2 WHERE id = ?1", params![id, staged.to_string_lossy()])?;
        Ok(id)
    })?;
    let staged = dest_dir.join(format!(".um-import-{journal}.partial"));
    let mut result = if method == "hardlink" {
        std::fs::hard_link(&p.source, &staged).map_err(|e| format!("cannot hardlink: {}", e.kind()))
    } else {
        copy_new(&p.source, &staged)
    };
    // Like native, a refused hardlink (e.g. fs.protected_hardlinks for another user's file)
    // falls back to a copy when there is room for it.
    if result.is_err() && method == "hardlink" {
        let room =
            paths::free_bytes(&dest_dir).unwrap_or(0) >= src_meta.len().saturating_add(min_free);
        if room {
            let _ = std::fs::remove_file(&staged);
            method = "copy";
            result = copy_new(&p.source, &staged);
            store.blocking(|c| {
                c.execute(
                    "UPDATE import_journal SET method = 'copy' WHERE id = ?1",
                    [journal],
                )?;
                Ok(())
            })?;
        }
    }
    if let Err(e) = result {
        let _ = std::fs::remove_file(&staged);
        store.blocking(|c| journal_set(c, journal, "rolled_back", Some(&e)))?;
        return Err(FileError::Blocked(format!("{}: {e}", p.rel)));
    }
    let ino = crate::fsutil::file_id(&staged)
        .map(|(_, ino)| ino)
        .map_err(|e| FileError::Internal(e.into()))?;
    store.blocking(|c| {
        c.execute(
            "UPDATE import_journal SET state = 'staged', staged_ino = ?2 WHERE id = ?1",
            params![journal, ino as i64],
        )?;
        Ok(())
    })?;
    Ok(Staged {
        journal,
        staged,
        dest,
        dest_dir,
        method,
    })
}

fn copy_new(src: &Path, dst: &Path) -> Result<(), String> {
    let mut input =
        std::fs::File::open(src).map_err(|e| format!("cannot open the source: {}", e.kind()))?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(|e| format!("cannot create the staged copy: {}", e.kind()))?;
    std::io::copy(&mut input, &mut out).map_err(|e| format!("copy failed: {}", e.kind()))?;
    out.sync_all()
        .map_err(|e| format!("sync failed: {}", e.kind()))?;
    Ok(())
}

fn rollback_staged(
    store: &crate::store::Store,
    journal: i64,
    staged: &Path,
    why: &str,
) -> Result<(), FileError> {
    let _ = std::fs::remove_file(staged);
    store.blocking(|c| journal_set(c, journal, "rolled_back", Some(why)))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn publish(
    store: &crate::store::Store,
    g: &Grab,
    root: &Path,
    p: &Planned,
    s: &Staged,
    probe_text: &str,
    quality: Option<String>,
    revision: Revision,
    now: OffsetDateTime,
) -> Result<(), FileError> {
    let mut backup: Option<PathBuf> = None;
    if let Some(r) = &p.replace {
        let old = root.join(&r.path);
        paths::check_no_symlinks(root, &old).map_err(FileError::Blocked)?;
        match std::fs::symlink_metadata(&old) {
            Ok(m) if m.file_type().is_file() => {
                let b = old.with_file_name(format!(".um-backup-{}.orig", s.journal));
                std::fs::rename(&old, &b).map_err(|e| {
                    FileError::Blocked(format!("cannot move the replaced file aside: {}", e.kind()))
                })?;
                let ino = crate::fsutil::file_id(&b).map(|(_, ino)| ino).unwrap_or(0);
                store.blocking(|c| {
                    c.execute(
                        "UPDATE import_journal SET state = 'backed_up', backup = ?2, backup_ino = ?3, updated_at = ?4 WHERE id = ?1",
                        params![s.journal, b.to_string_lossy(), ino as i64, rfc3339(OffsetDateTime::now_utc())],
                    )?;
                    Ok(())
                })?;
                backup = Some(b);
            }
            Ok(_) => {
                return Err(FileError::Blocked(format!(
                    "{} is not a regular file; not replaced",
                    r.path
                )));
            }
            Err(_) => {} // already gone: a plain import that retires the stale row
        }
    }
    if let Err(e) = std::fs::hard_link(&s.staged, &s.dest) {
        // Restore the original before reporting.
        if let (Some(b), Some(r)) = (&backup, &p.replace) {
            let _ = std::fs::rename(b, root.join(&r.path));
        }
        let _ = std::fs::remove_file(&s.staged);
        store.blocking(|c| {
            journal_set(
                c,
                s.journal,
                "rolled_back",
                Some(&format!("publish failed: {}", e.kind())),
            )
        })?;
        return Err(FileError::Blocked(format!(
            "cannot publish {}: {}",
            s.dest.display(),
            e.kind()
        )));
    }
    let _ = std::fs::remove_file(&s.staged);
    let _ = crate::fsutil::fsync_dir(&s.dest_dir);
    store.blocking(|c| journal_set(c, s.journal, "published", None))?;
    let dest_rel = s
        .dest
        .strip_prefix(root)
        .ok()
        .and_then(paths::rel_string)
        .ok_or_else(|| FileError::Blocked("destination escaped the root".into()))?;
    let mtime = std::fs::symlink_metadata(&s.dest)
        .map(|m| crate::fsutil::mtime(&m))
        .ok();
    store.blocking(|c| {
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now_s = rfc3339(now);
        let ev = FileEvidence {
            path: Some(dest_rel.clone()),
            size: Some(p.size),
            quality: quality.as_deref().and_then(|q| serde_json::from_value(json!(q)).ok()),
            revision: Some(revision),
            source: "import".into(),
            observed_at: now_s.clone(),
            probe: Some(probe_text.to_string()),
        };
        if let Some(r) = &p.replace
            && r.file_id > 0
        {
            tx.execute("UPDATE files SET present = 0 WHERE id = ?1", [r.file_id])?;
        }
        // A stale row for the same path (e.g. a scan of an earlier copy) is retired.
        tx.execute("UPDATE files SET present = 0 WHERE media_id = ?1 AND path = ?2 AND present = 1", params![g.media_id.to_string(), dest_rel])?;
        library::insert_file(&tx, &g.media_id.to_string(), &ev, &p.targets, Some(&g.id), mtime)?;
        tx.execute("UPDATE import_journal SET state = 'committed', probe = ?2, updated_at = ?3 WHERE id = ?1", params![s.journal, probe_text, now_s])?;
        tx.execute("UPDATE grabs SET imported_files = imported_files + 1 WHERE id = ?1", [&g.id])?;
        library::event(
            &tx,
            &now_s,
            if p.replace.is_some() { "file_replaced" } else { "file_imported" },
            Some(&g.media_id.to_string()),
            Some(&p.targets[0].to_string()),
            Some(&g.id),
            &format!("{} -> {dest_rel} ({})", p.rel, s.method),
            Some(&json!({ "method": s.method, "targets": p.targets, "replaced": p.replace.as_ref().map(|r| r.path.clone()), "probe": probe_text })),
        )?;
        tx.commit()?;
        Ok(())
    })?;
    if let Some(b) = backup {
        let _ = std::fs::remove_file(&b);
        store.blocking(|c| journal_set(c, s.journal, "backup_removed", None))?;
    }
    Ok(())
}

/// Startup recovery of interrupted imports. Uncommitted rows are rolled back: our staged or
/// published inode is removed (only if it is still ours) and a moved-aside original is
/// restored. Committed rows with a leftover backup have it removed.
pub fn recover(c: &Connection) -> Result<Vec<String>> {
    let mut notes = vec![];
    let mut stmt = c.prepare(
        "SELECT id, root, dest, state, staged, staged_ino, replaced_path, backup, backup_ino FROM import_journal WHERE state IN ('planned', 'staged', 'backed_up', 'published', 'committed') AND (state != 'committed' OR backup IS NOT NULL)",
    )?;
    type Row = (
        i64,
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
    );
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    let ino_of = |p: &Path| {
        std::fs::symlink_metadata(p)
            .ok()
            .filter(|m| m.file_type().is_file())
            .and_then(|_| crate::fsutil::file_id(p).ok())
            .map(|(_, ino)| ino as i64)
    };
    for (id, root, dest, state, staged, staged_ino, replaced, backup, backup_ino) in rows {
        let root = PathBuf::from(root);
        let dest_abs = root.join(&dest);
        if state == "committed" {
            if let Some(b) = backup.as_deref().map(PathBuf::from)
                && ino_of(&b).is_some()
                && ino_of(&b) == backup_ino
            {
                let _ = std::fs::remove_file(&b);
            }
            journal_set(c, id, "backup_removed", None)?;
            notes.push(format!(
                "journal {id}: removed the leftover backup of a committed replacement"
            ));
            continue;
        }
        if let Some(sp) = staged.as_deref().map(PathBuf::from)
            && ino_of(&sp).is_some()
            && (staged_ino.is_none() || ino_of(&sp) == staged_ino)
        {
            let _ = std::fs::remove_file(&sp);
        }
        // Our published (or linked-before-recorded) inode is removed; anything else is kept.
        if staged_ino.is_some() && ino_of(&dest_abs) == staged_ino {
            let _ = std::fs::remove_file(&dest_abs);
        }
        if let (Some(b), Some(r)) = (backup.as_deref().map(PathBuf::from), replaced.as_deref()) {
            let original = root.join(r);
            if ino_of(&b).is_some()
                && ino_of(&b) == backup_ino
                && std::fs::symlink_metadata(&original).is_err()
            {
                std::fs::rename(&b, &original).with_context(|| {
                    format!("journal {id}: cannot restore {}", original.display())
                })?;
                notes.push(format!("journal {id}: restored the original {r}"));
            }
        }
        journal_set(
            c,
            id,
            "rolled_back",
            Some("interrupted; rolled back at startup"),
        )?;
        notes.push(format!(
            "journal {id}: rolled back an interrupted {state} import of {dest}"
        ));
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grab(
        media: MediaId,
        targets: Vec<(MediaId, bool)>,
        full_season: bool,
        client_name: &str,
    ) -> Grab {
        Grab {
            id: "grb_x".into(),
            media_id: media,
            target: targets[0].0,
            plan_id: None,
            release_ref: "rel_x".into(),
            info_hash: "a".repeat(40),
            title: client_name.into(),
            indexer_id: Some(1),
            size: None,
            quality: Some("WEBDL-1080p".into()),
            revision: Some(Revision::default()),
            full_season,
            category: "tv".into(),
            save_path: "/downloads".into(),
            seed_ratio: None,
            seed_time_minutes: None,
            remove_completed: false,
            remove_failed: false,
            origin: "interactive".into(),
            state: "importing".into(),
            active: true,
            attempts: 0,
            next_attempt_at: String::new(),
            progress: Some(1.0),
            client_state: None,
            client_name: Some(client_name.into()),
            last_error: None,
            user_paused: false,
            stalled_since: None,
            problem_since: None,
            absent_since: None,
            last_progress: None,
            failure: None,
            retry: None,
            cleanup: "none".into(),
            cleanup_detail: None,
            cleanup_at: None,
            cleanup_files: None,
            imported_files: 0,
            created_at: String::new(),
            updated_at: String::new(),
            submitted_at: None,
            completed_at: None,
            imported_at: None,
            targets: targets
                .into_iter()
                .map(|(target, replaces)| downloads::GrabTarget { target, replaces })
                .collect(),
        }
    }

    fn entry(name: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            size: 10,
            complete: true,
            wanted: true,
        }
    }

    #[test]
    fn selection_maps_exact_episodes_and_skips_extras() {
        let series: MediaId = "series:tvdb:1".parse().unwrap();
        let e = |n| library::episode_id(series, 1, n);
        let g = grab(
            series,
            vec![(e(1), false), (e(2), false), (e(3), false)],
            true,
            "Show.S01.1080p.WEB-DL-GRP",
        );
        let item = ItemInfo {
            folder: "Show".into(),
            runtime: Some(45),
            episodes: Default::default(),
            existing: None,
            series: Some(series),
            identity: None,
            profile: None,
            original_language: None,
        };
        let files = [
            entry("Show.S01.1080p/Show.S01E01.1080p.WEB-DL-GRP.mkv"),
            entry("Show.S01.1080p/Show.S01E02E03.1080p.WEB-DL-GRP.mkv"),
            entry("Show.S01.1080p/Sample/Show.S01E01.sample.mkv"),
            entry("Show.S01.1080p/Show.S01E04.1080p.WEB-DL-GRP.mkv"),
            entry("Show.S01.1080p/info.nfo"),
            entry("../escape.mkv"),
        ];
        let s = select(&g, &files, Path::new("/dl"), &item, true);
        assert!(s.blocker.is_none());
        assert_eq!(s.planned.len(), 2);
        assert_eq!(s.planned[1].targets, vec![e(2), e(3)]);
        assert_eq!(s.planned[0].dest_dir, PathBuf::from("Show/Season 1"));
        assert_eq!(
            s.planned[0].dest_name, "Show.S01E01.1080p.WEB-DL-GRP.mkv",
            "season packs keep file names"
        );
        let reasons: Vec<&str> = s.skipped.iter().map(|x| x.reason.as_str()).collect();
        assert!(reasons.contains(&"sample or extra"));
        assert!(reasons.contains(&"covers episodes this download was not grabbed for"));
        assert!(reasons.contains(&"unsafe file name"));

        // A single-episode download takes the download title (native scene name).
        let g = grab(
            series,
            vec![(e(2), false)],
            false,
            "Show.S01E02.1080p.WEB-DL-GRP",
        );
        let s = select(
            &g,
            &[entry("Show.S01E02.1080p.WEB-DL-GRP/show.102.mkv")],
            Path::new("/dl"),
            &item,
            true,
        );
        assert_eq!(s.planned.len(), 1);
        assert_eq!(s.planned[0].dest_name, "Show.S01E02.1080p.WEB-DL-GRP.mkv");

        // Two files claiming one episode block.
        let g = grab(series, vec![(e(1), false)], false, "x");
        let s = select(
            &g,
            &[entry("a/Show.S01E01.mkv"), entry("b/Show.S01E01.mkv")],
            Path::new("/dl"),
            &item,
            true,
        );
        assert!(s.blocker.is_some());
    }

    #[test]
    fn movie_selection() {
        let movie: MediaId = "movie:tmdb:603".parse().unwrap();
        let g = grab(
            movie,
            vec![(movie, false)],
            false,
            "The.Matrix.1999.1080p.WEB-DL-GRP",
        );
        let item = ItemInfo {
            folder: "The Matrix (1999)".into(),
            runtime: Some(136),
            episodes: Default::default(),
            existing: None,
            series: None,
            identity: None,
            profile: None,
            original_language: None,
        };
        let s = select(
            &g,
            &[
                entry("The.Matrix.1999.1080p.WEB-DL-GRP/matrix.mkv"),
                entry("The.Matrix.1999.1080p.WEB-DL-GRP/matrix-sample.mkv"),
            ],
            Path::new("/dl"),
            &item,
            true,
        );
        assert_eq!(s.planned.len(), 1);
        assert_eq!(
            s.planned[0].dest_name,
            "The.Matrix.1999.1080p.WEB-DL-GRP.mkv"
        );
        assert_eq!(s.planned[0].dest_dir, PathBuf::from("The Matrix (1999)"));
        let s = select(
            &g,
            &[entry("a/cd1.mkv"), entry("a/cd2.mkv")],
            Path::new("/dl"),
            &item,
            true,
        );
        assert!(s.blocker.is_some());
    }
}
