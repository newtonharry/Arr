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
use crate::downloads::{self, Grab, policy_kind};
use crate::ids::MediaId;
use crate::import::rfc3339;
use crate::library;
use crate::paths;
use crate::probe::{self, Probe};
use crate::quality::Quality;
use crate::release::{self, Numbering, Revision};
use crate::settings::{MediaKind, MediaManagement};
use crate::sport::reason::{FileOp as FO, Landed, Reason as R};
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
pub const OTHER_VIDEO: &[&str] = &[
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

/// Where sport lands: `[library] sport`, else a `Sport` folder beside the movies root (so it is
/// on the same drive as the library and the downloads beside it, and a hard link works).
pub fn sport_root(lib: &LibraryConfig) -> Option<PathBuf> {
    lib.sport_root()
}

/// The root a media id is filed under. Explicit for every kind: an event is filed under the
/// sport root and never under a movie or TV root.
pub fn library_root(lib: &LibraryConfig, id: MediaId) -> Option<PathBuf> {
    match id {
        MediaId::Movie { .. } => lib.movies.clone(),
        MediaId::Series(_) | MediaId::Season(..) | MediaId::Episode(..) => lib.tv.clone(),
        MediaId::Event(_) => sport_root(lib),
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

/// A file name that says it isn't an episode (an extra, a trailer, a special...). Only used
/// before matching a file by elimination, which must never take an extra for an episode.
fn extra_name(name: &str) -> bool {
    const MARKERS: &[&str] = &[
        "extra",
        "extras",
        "bonus",
        "featurette",
        "featurettes",
        "special",
        "specials",
        "trailer",
        "trailers",
        "teaser",
        "promo",
        "preview",
        "recap",
        "interview",
        "interviews",
        "deleted",
        "behind",
        "making",
        "commentary",
        "ncop",
        "nced",
    ];
    let stem = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    stem.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| MARKERS.contains(&w))
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
    /// Folders the destination root must stay apart from, by where they really are (a sport
    /// import: the downloads and the other library roots). Empty for movies and series, whose
    /// import is unchanged.
    pub keep_apart: Vec<PathBuf>,
}

/// Whether `root` (and, after publishing, `file`) is still apart from every folder in `apart`,
/// by where things really are. An error is a plain sentence; a path that cannot be resolved
/// counts as not apart.
fn confined(root: &Path, file: Option<&Path>, apart: &[PathBuf]) -> Result<(), R> {
    // (a path that cannot be resolved: the plain sentence names no payload text, only the
    // configured folders)
    let unusable = |detail: String| {
        tracing::warn!("sport root cannot be resolved: {detail}");
        R::SportFolderUnusable
    };
    let under = paths::resolved(root).map_err(unusable)?;
    for a in apart {
        if paths::overlap(root, a).map_err(unusable)? {
            return Err(R::SportFolderApart);
        }
    }
    if let Some(f) = file {
        let real = paths::resolved(f).map_err(unusable)?;
        if !real.starts_with(&under) {
            return Err(R::TakenBack {
                landed: Landed::NotSportFolder,
            });
        }
        for a in apart {
            if real.starts_with(paths::resolved(a).map_err(unusable)?) {
                return Err(R::TakenBack {
                    landed: Landed::InsideDownloads,
                });
            }
        }
    }
    Ok(())
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
    /// Targets of a season pack that no video file of the download holds, not even by
    /// elimination.
    pub unfiled: Vec<MediaId>,
    /// Files taken for an episode by elimination (shown in the history).
    pub notes: Vec<String>,
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

/// An importable video of a pack: its safe relative path, or `None` for anything else
/// (not a video, a sample or extra, an unsafe name).
fn candidate(name: &str) -> Option<String> {
    let rel = paths::safe_rel(name)?;
    let rel = paths::rel_string(&rel).unwrap_or_else(|| rel.to_string_lossy().into_owned());
    let ext = ext_of(&rel)?;
    (VIDEO_EXTENSIONS.contains(&ext.as_str()) && !excluded(&rel) && !sample_name(&rel))
        .then_some(rel)
}

/// The episodes each video of a pack holds (`episodes[i]` for `cands[i]`, a path with its
/// size): what its file name says, or, in a full-season pack, one file by elimination.
///
/// A full season can have one file named only after the pack, with no episode number, and
/// one episode of the season with no file: that file is that episode. Only when all of these
/// hold: exactly one file maps to no episode and its name has no numbering but, at most, the
/// season's own (`...S05 1080p...`), every other file is of one season, exactly one of that
/// season's `known` episodes (the catalog's) has no file, and the file is within half to
/// twice the median size of the others. Samples and extras are never candidates. Otherwise a
/// file with no episode in its name holds none. `eliminated` is the file matched this way.
pub struct Held {
    pub episodes: Vec<Vec<MediaId>>,
    pub eliminated: Option<usize>,
}

pub fn held_episodes(
    cands: &[(String, u64)],
    series: MediaId,
    known: &[MediaId],
    full_season: bool,
) -> Held {
    let parsed: Vec<Numbering> = cands
        .iter()
        .map(|(rel, _)| release::parse(rel.rsplit('/').next().unwrap_or(rel)).numbering)
        .collect();
    let mut episodes: Vec<Vec<MediaId>> = parsed
        .iter()
        .map(|n| match n {
            Numbering::Episodes { season, episodes } => episodes
                .iter()
                .map(|e| library::episode_id(series, *season, *e))
                .collect(),
            _ => vec![],
        })
        .collect();
    let eliminated = full_season
        .then(|| {
            let unmapped: Vec<usize> = (0..cands.len())
                .filter(|i| episodes[*i].is_empty())
                .collect();
            let [i] = unmapped[..] else { return None };
            if extra_name(&cands[i].0) {
                return None;
            }
            let seasons: std::collections::BTreeSet<u16> = parsed
                .iter()
                .filter_map(|n| match n {
                    Numbering::Episodes { season, .. } => Some(*season),
                    _ => None,
                })
                .collect();
            let [season] = seasons.into_iter().collect::<Vec<_>>()[..] else {
                return None;
            };
            // Named for nothing but the pack: no numbering, or this season's alone.
            match parsed[i] {
                Numbering::None => {}
                Numbering::Season { season: s } if s == season => {}
                _ => return None,
            }
            let have: Vec<MediaId> = episodes.iter().flatten().copied().collect();
            let lacking: Vec<MediaId> = known
                .iter()
                .copied()
                .filter(|k| matches!(k, MediaId::Episode(_, s, _) if *s == season))
                .filter(|k| !have.contains(k))
                .collect();
            let [episode] = lacking[..] else { return None };
            let mut sizes: Vec<u64> = (0..cands.len())
                .filter(|j| *j != i)
                .map(|j| cands[j].1)
                .collect();
            sizes.sort_unstable();
            let median = sizes[sizes.len() / 2];
            let size = cands[i].1;
            // Sample-sized or oversized: not an episode of this pack.
            if size == 0 || median == 0 || size < median / 2 || size > median.saturating_mul(2) {
                return None;
            }
            episodes[i] = vec![episode];
            Some(i)
        })
        .flatten();
    Held {
        episodes,
        eliminated,
    }
}

/// Which files of a season pack to download for a grab that covers only some episodes.
pub struct PackPlan {
    /// `None`: every episode the pack holds is wanted, so nothing is left out (all files, as
    /// without a selection). `Some(indexes)`: download only these.
    pub only: Option<Vec<usize>>,
    /// Targets with no file in the pack at all.
    pub missing: Vec<MediaId>,
    /// The file (an index into `files`) matched to its episode by elimination.
    pub matched: Option<(usize, MediaId)>,
}

/// `files` are the pack's files as the client names them (`<torrent name>/<path>`) with their
/// sizes, padding entries included so that indexes stay the torrent's file indexes. A file is
/// wanted when it is an importable video (not a sample or extra) whose episodes (see
/// `held_episodes`) are all in `targets`, which is exactly the test `select` applies at
/// import. Subtitles, nfo files and the like are not fetched when a selection is made.
/// `known` is the series' episodes in the catalog. `Err`: no file maps to a wanted episode.
pub fn pack_plan(
    files: &[(String, u64)],
    series: MediaId,
    targets: &[MediaId],
    known: &[MediaId],
) -> Result<PackPlan, String> {
    let (at, cands): (Vec<usize>, Vec<(String, u64)>) = files
        .iter()
        .enumerate()
        .filter_map(|(i, (name, size))| candidate(name).map(|rel| (i, (rel, *size))))
        .unzip();
    let held = held_episodes(&cands, series, known, true);
    let mut wanted = vec![];
    let mut left_out = false;
    for (n, eps) in held.episodes.iter().enumerate() {
        if !eps.is_empty() && eps.iter().all(|id| targets.contains(id)) {
            wanted.push(at[n]);
        } else {
            left_out = true;
        }
    }
    if wanted.is_empty() {
        return Err(
            "no video file in the pack is for an episode this download was grabbed for".into(),
        );
    }
    let missing = targets
        .iter()
        .copied()
        .filter(|t| !held.episodes.iter().any(|e| e.contains(t)))
        .collect();
    Ok(PackPlan {
        only: left_out.then_some(wanted),
        missing,
        matched: held.eliminated.map(|n| (at[n], held.episodes[n][0])),
    })
}

fn episode_label(e: MediaId) -> String {
    match e {
        MediaId::Episode(_, s, n) => format!("S{s:02}E{n:02}"),
        other => other.to_string(),
    }
}

/// The catalog's episodes of a series, as `held_episodes` wants them.
fn known_episodes(item: &ItemInfo) -> Vec<MediaId> {
    let Some(series) = item.series else {
        return vec![];
    };
    item.episodes
        .iter()
        .flat_map(|(s, m)| m.keys().map(move |n| library::episode_id(series, *s, *n)))
        .collect()
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
    // A full-season pack's one unnamed episode file, matched to its episode by elimination
    // exactly as the selection did when the pack was added. Every video counts here, selected
    // or not.
    let mut elim: Option<(String, MediaId)> = None;
    let mut unfiled: Vec<MediaId> = vec![];
    let mut notes: Vec<String> = vec![];
    if g.full_season
        && let Some(series) = item.series
    {
        let cands: Vec<(String, u64)> = files
            .iter()
            .filter_map(|f| candidate(&f.name).map(|rel| (rel, f.size)))
            .collect();
        let held = held_episodes(&cands, series, &known_episodes(item), true);
        elim = held
            .eliminated
            .map(|n| (cands[n].0.clone(), held.episodes[n][0]));
        unfiled = targets
            .iter()
            .copied()
            .filter(|t| !held.episodes.iter().any(|e| e.contains(t)))
            .collect();
    }
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
                    keep_apart: vec![],
                });
            }
            many => {
                blocker = Some(format!(
                    "{} video files in the download; one file per movie is imported (cancel, or remove the extras in the client and retry)",
                    many.len()
                ));
            }
        },
        // (a sport download never reaches here: `import_event` takes it first)
        MediaId::Series(_) | MediaId::Season(..) | MediaId::Episode(..) | MediaId::Event(_) => {
            let mut claimed: std::collections::BTreeMap<MediaId, String> = Default::default();
            for (f, rel) in &videos {
                let base = rel.rsplit('/').next().unwrap_or(rel);
                let parsed = release::parse(base);
                let eliminated = elim.as_ref().filter(|(r, _)| r == rel).map(|(_, e)| *e);
                let eps: Vec<MediaId> = match (&parsed.numbering, g.target) {
                    (_, _) if eliminated.is_some() => eliminated.into_iter().collect(),
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
                } else if eliminated.is_some() {
                    // Named after the pack, which Plex could not match: name it for its episode.
                    format!("{} - S{season:02}E{number:02}.{ext}", item.folder)
                } else {
                    base.to_string()
                };
                if let Some(e) = eliminated {
                    notes.push(format!(
                        "{rel} is {}, matched by elimination: the pack's only unnamed episode-sized file",
                        episode_label(e)
                    ));
                }
                planned.push(Planned {
                    source: local_save.join(rel),
                    rel: rel.clone(),
                    size: f.size,
                    targets: eps,
                    dest_dir: PathBuf::from(&item.folder).join(format!("Season {season}")),
                    dest_name: library::clean_name(&name, replace_illegal),
                    replace,
                    runtime: info.and_then(|i| i.0).or(item.runtime),
                    keep_apart: vec![],
                });
            }
        }
    }
    Selection {
        planned,
        skipped,
        blocker,
        unfiled,
        notes,
    }
}

/// Result of one import attempt.
#[derive(Debug)]
pub enum Outcome {
    Imported {
        files: usize,
        skipped: Vec<Skipped>,
        missing: Vec<MediaId>,
        /// Of `missing`, the targets no file of the download holds: they are wanted again.
        unfiled: Vec<MediaId>,
        /// Files taken for an episode by elimination.
        notes: Vec<String>,
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
/// What an error from the download client says at import time: for a sport grab the typed
/// reason (sign-in failed, unreachable, ...; the client's text goes to the log), for any other
/// the label and the error as before.
fn client_blocked(g: &Grab, label: &str, e: &crate::upstream::UpstreamError) -> String {
    match g.media_id {
        MediaId::Event(_) => {
            tracing::warn!(grab = %g.id, "client error at import: {label} {}", e.describe());
            R::from_upstream(e).encode()
        }
        MediaId::Movie { .. } | MediaId::Series(_) | MediaId::Season(..) | MediaId::Episode(..) => {
            format!("{label} {}", e.describe())
        }
    }
}

/// A place the import can be made to stop dead, for tests of crash recovery: `(grab id, point)`.
/// The points are `after_stage`, `after_link` (published but the staged copy not yet removed),
/// `after_published`, `after_commit` and `before_apply`. Stopping there leaves exactly what a
/// crash at that line leaves: the import's own writes so far, and the grab `importing`.
#[doc(hidden)]
pub static CRASH_AT: std::sync::Mutex<Vec<(String, &'static str)>> =
    std::sync::Mutex::new(Vec::new());

#[derive(Debug)]
struct Crashed;
impl std::fmt::Display for Crashed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stopped for a crash test")
    }
}
impl std::error::Error for Crashed {}

fn failpoint(grab_id: &str, point: &'static str) -> Result<(), FileError> {
    let hit = CRASH_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(g, p)| g == grab_id && *p == point);
    if hit {
        return Err(FileError::Internal(Crashed.into()));
    }
    Ok(())
}

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
            // (a crash test's stop: nothing more is written, as if the process had died)
            Err(e) if e.downcast_ref::<Crashed>().is_some() => continue,
            // A sport grab's error text is never shown: it can carry the payload's own names.
            Err(e) if matches!(g.media_id, MediaId::Event(_)) => {
                tracing::warn!(grab = %g.id, "sport import failed internally: {e:#}");
                Outcome::Blocked(R::Internal.encode())
            }
            Err(e) => Outcome::Blocked(format!("{e:#}")),
        };
        if matches!(outcome, Outcome::Imported { .. }) && failpoint(&g.id, "before_apply").is_err()
        {
            continue;
        }
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
            unfiled,
            notes,
        } => {
            let (cleanup, detail) = downloads::after_import_cleanup(g);
            let still: Vec<&MediaId> = missing.iter().filter(|m| !unfiled.contains(m)).collect();
            let note = if still.is_empty() {
                None
            } else {
                Some(format!(
                    "imported {files} file(s); not imported: {}",
                    still
                        .iter()
                        .map(|m| m.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            };
            let research = downloads::research_for_dropped(st, g, &unfiled, now);
            let message = if notes.is_empty() {
                format!("{title}: {files} file(s) imported", title = g.title)
            } else {
                format!(
                    "{title}: {files} file(s) imported ({})",
                    notes.join("; "),
                    title = g.title
                )
            };
            let (media_s, target_s) = (g.media_id.to_string(), g.target.to_string());
            let g2 = g.clone();
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
                    library::event(&tx, &now_s, "import_completed", Some(&media_s), Some(&target_s), Some(&id), &message, Some(&json!({ "skipped": skipped_json, "missing": missing, "matched_by_elimination": notes })))?;
                    downloads::drop_targets(&tx, &g2, &unfiled, &research, &now_s)?;
                    if plex {
                        crate::worker::queue_command(&tx, "plex_refresh", Some(media), &now_s)?;
                    }
                    tx.commit()?;
                    Ok(())
                })
                .await?;
            if let (Some(qb), Some(l)) = (&st.client, st.current())
                && let Ok(p) = downloads::client_policy(st, &l.settings, policy_kind(g.media_id))
                && let Some(cat) = p.post_import_category
            {
                let _ = qb.set_category(&g.info_hash, &cat).await;
            }
            st.reload_index().await?;
            st.worker.notify.notify_one();
            Ok(true)
        }
        Outcome::Blocked(reason) => {
            // (a sport grab's text is only what the code built from the stored event)
            let reason = match g.media_id {
                MediaId::Event(_) => crate::sport::reason::classify(&reason),
                MediaId::Movie { .. }
                | MediaId::Series(_)
                | MediaId::Season(..)
                | MediaId::Episode(..) => reason,
            };
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

#[deny(clippy::wildcard_enum_match_arm)]
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
        // (a sport event has its own import, `import_event`; a season or an episode is a
        // target of a series grab, never a grab's media id)
        MediaId::Season(..) | MediaId::Episode(..) | MediaId::Event(_) => {
            Err("grab media id is not a movie or series".into())
        }
    }
}

async fn import_grab(st: &AppState, g: &Grab, now: OffsetDateTime) -> Result<Outcome> {
    let Some(l) = st.current() else {
        return Ok(Outcome::Blocked("no configuration import is active".into()));
    };
    let media = policy_kind(g.media_id);
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
    let cname = if qb.embedded_semantics() {
        "the embedded engine"
    } else {
        "qBittorrent"
    };
    let Some(root) = library_root(&lib, g.media_id) else {
        return Ok(Outcome::Blocked(format!(
            "no [library] root for {}",
            if matches!(g.media_id, MediaId::Event(_)) {
                "sport"
            } else {
                media.label()
            }
        )));
    };
    let info = match qb.info(std::slice::from_ref(&g.info_hash)).await {
        Ok(list) => list
            .into_iter()
            .find(|t| t.hash.eq_ignore_ascii_case(&g.info_hash)),
        Err(e) => return Ok(Outcome::Blocked(client_blocked(g, qb.label(), &e))),
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
        Err(e) => return Ok(Outcome::Blocked(client_blocked(g, qb.label(), &e))),
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
    // A sport download is filed by its own path: one video, named from the stored event, into
    // `sport_files`. It shares the staging, the journal and the recovery, and nothing else.
    if let MediaId::Event(e) = g.media_id {
        return import_event(
            st,
            g,
            e,
            &lib,
            &mm,
            &root,
            &entries,
            &local_save,
            &local_root,
            now,
        )
        .await;
    }
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
            MediaId::Series(_) | MediaId::Season(..) | MediaId::Event(_) => None,
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
        // A copy that is what it was grabbed as keeps the authorisation it was grabbed under:
        // changing a title's quality level applies to later searches, never to a download
        // already underway.
        if let Some(prof) = profile
            && prof.tier_of(eff).is_none()
            && Some(eff) != grab_quality
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
    let unfiled = missing
        .iter()
        .copied()
        .filter(|t| sel.unfiled.contains(t))
        .collect();
    Ok(Outcome::Imported {
        files: imported,
        skipped: sel.skipped,
        missing,
        unfiled,
        notes: sel.notes,
    })
}

/// Imports the one video of a sport download.
///
/// The payload is judged again before anything is filed: nothing in it may be an executable or
/// an archive (the download fails, is blocklisted for this event and is handled as a failed
/// download is: removed per `removeFailedDownloads`); it must hold exactly one video (more is
/// `import_blocked`, waiting for a person); the probed video must be of a plausible duration and
/// size for the sport and part (`sport::get::bounds`; otherwise the download fails as an
/// invalid media file does for a movie). A part that is already filed is never overwritten.
#[allow(clippy::too_many_arguments)]
async fn import_event(
    st: &AppState,
    g: &Grab,
    e: crate::ids::EventRef,
    lib: &LibraryConfig,
    mm: &MediaManagement,
    root: &Path,
    entries: &[FileEntry],
    local_save: &Path,
    local_root: &Path,
    now: OffsetDateTime,
) -> Result<Outcome> {
    let event_id = e.event_id();
    let id2 = event_id.clone();
    let (ev, filed, known) = st
        .store
        .with(move |c| {
            let ev = crate::sport::data::event(c, &id2)?;
            let known = match &ev {
                Some(ev) => Some(crate::sport::get::known_for(c, ev)?),
                None => None,
            };
            Ok((
                ev,
                crate::sport::files::filed_by(c, &id2, &e.part.slug().unwrap_or_default())?,
                known,
            ))
        })
        .await?;
    let known = known.unwrap_or_default();
    let Some(ev) = ev else {
        return Ok(Outcome::Blocked(R::EventGone.encode()));
    };
    let Some(league) = crate::sport::catalog::find(e.league) else {
        return Ok(Outcome::Blocked(R::LeagueUnknown.encode()));
    };
    // Every sentence below is built from the stored event, never from the torrent's own names
    // (a file or a torrent can be called anything, including how the game went).
    match filed {
        // A file this very download filed (a crash between its commit and the grab being marked
        // imported): the bookkeeping is finished, nothing is filed again.
        Some(Some(by)) if by == g.id => {
            return Ok(Outcome::Imported {
                files: 1,
                skipped: vec![],
                missing: vec![],
                unfiled: vec![],
                notes: vec!["already filed by this download".into()],
            });
        }
        Some(_) => {
            return Ok(Outcome::Blocked(R::AlreadyHave.encode()));
        }
        None => {}
    }
    let wanted: Vec<&FileEntry> = entries.iter().filter(|f| f.wanted).collect();
    if let Some(bad) = crate::sport::get::unsafe_file(wanted.iter().map(|f| f.name.as_str())) {
        // (the extension is one of a fixed list, or a rar or zip volume's: never the file's name)
        let ext = bad.rsplit('.').next().unwrap_or("").to_ascii_lowercase();

        return Ok(Outcome::Failed(
            "unsafe_payload",
            R::unsafe_file(if crate::sport::get::known_unsafe_ext(&ext) {
                &ext
            } else {
                "archive"
            })
            .encode(),
        ));
    }
    // The torrent's own name and EVERY video file of the payload (hidden, sample and extras
    // folders included, before any of them is filtered out) are judged against the stored event
    // and the part picked, and the videos are counted over the whole payload. One exhaustive
    // classification of what that finds.
    let all_names: Vec<String> = entries.iter().map(|f| f.name.clone()).collect();
    match crate::sport::get::judge_payload(
        &ev,
        &known,
        g.client_name.as_deref(),
        &all_names,
        e.part,
    ) {
        crate::sport::get::PayloadCheck::Fine => {}
        crate::sport::get::PayloadCheck::Contradicts(why) => {
            return Ok(Outcome::Failed("wrong_payload", why.encode()));
        }
        crate::sport::get::PayloadCheck::SeveralVideos => {
            return Ok(Outcome::Blocked(R::SeveralVideos.encode()));
        }
    }
    let videos: Vec<(&FileEntry, String)> = wanted
        .iter()
        .filter(|f| f.complete)
        .filter_map(|f| candidate(&f.name).map(|rel| (*f, rel)))
        .collect();
    let (file, rel) = match videos.as_slice() {
        [one] => (one.0, one.1.clone()),
        _ => {
            return Ok(Outcome::Blocked(R::NoVideo.encode()));
        }
    };
    let ext = ext_of(&rel).unwrap_or_else(|| "mkv".into());
    let (dest_dir, dest_name) = {
        let (dir, _) = crate::sport::get::destination(&ev, e.part, &ext, true);
        // What is in the folder already, lower-cased: a name another event holds, ignoring case,
        // is not reused (it would be one file on a file system that ignores case).
        let taken: Vec<String> = std::fs::read_dir(root.join(&dir))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|f| f.file_name().into_string().ok())
            .map(|n| n.to_lowercase())
            .collect();
        crate::sport::get::destination_among(
            &ev,
            e.part,
            &ext,
            mm.replace_illegal_characters,
            &taken,
        )
    };
    let planned = Planned {
        source: local_save.join(&rel),
        // (what the journal and the history call it; the source path is `source`)
        rel: "the video".to_string(),
        size: file.size,
        targets: vec![g.media_id],
        dest_dir: dest_dir.clone(),
        dest_name,
        replace: None,
        runtime: None,
        keep_apart: vec![],
    };
    let canonical = match probe::validate(&planned.source, &[local_root]) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(grab = %g.id, "sport download not usable: {err}");
            return Ok(Outcome::Blocked(R::CannotUse.encode()));
        }
    };
    if let Err(err) = std::fs::File::open(&canonical) {
        return Ok(Outcome::Blocked(
            R::Unreadable {
                kind: err.kind().into(),
            }
            .encode(),
        ));
    }
    let pr = match probe::run(&lib.ffprobe, &canonical, lib.probe_timeout).await {
        Ok(pr) => pr,
        Err(err) => {
            tracing::warn!(grab = %g.id, "sport download failed the media probe: {err}");
            return Ok(Outcome::Failed("invalid_media", R::NotPlayable.encode()));
        }
    };
    // (`sample_detection = false` turns the bounds off, as it does the movie sample check)
    if let Some(why) = crate::sport::get::implausible_reason(
        crate::sport::get::bounds(league.sport, e.part),
        file.size,
        pr.duration_secs,
    )
    .filter(|_| lib.sample_detection)
    {
        return Ok(Outcome::Failed(
            "invalid_media",
            R::Implausible { why }.encode(),
        ));
    }
    let grab_quality: Option<Quality> = g
        .quality
        .as_deref()
        .and_then(|q| serde_json::from_value(json!(q)).ok());
    let eff = match crate::quality::effective(grab_quality, pr.width, pr.height) {
        Ok(q) => q,
        Err(err) => {
            tracing::warn!(grab = %g.id, "sport download quality: {err}");
            return Ok(Outcome::Failed("wrong_quality", R::WrongQuality.encode()));
        }
    };
    // The sport root is confined again here, by where it really is (a link in its path, made
    // after the configuration was read, is not followed into the downloads or another library);
    // `publish` checks again just before the file goes in place and once more after it has.
    let apart: Vec<PathBuf> = [local_root, local_save]
        .into_iter()
        .map(Path::to_path_buf)
        .chain(lib.movies.clone())
        .chain(lib.tv.clone())
        .collect();
    if let Err(why) = confined(root, None, &apart) {
        tracing::warn!(grab = %g.id, "sport root not confined: {why:?}");
        return Ok(Outcome::Blocked(why.encode()));
    }
    let planned = Planned {
        keep_apart: apart,
        ..planned
    };
    // The sport root is made the first time; it is never made at startup.
    if let Err(err) = std::fs::create_dir_all(root) {
        return Ok(Outcome::Blocked(
            R::CannotCreateFolder {
                kind: err.kind().into(),
            }
            .encode(),
        ));
    }
    match import_file(
        st,
        g,
        root,
        lib,
        mm,
        &planned,
        &pr,
        Some(eff),
        g.revision.unwrap_or_default(),
        now,
    )
    .await
    {
        Ok(()) => {}
        Err(FileError::Blocked(b)) => return Ok(Outcome::Blocked(b)),
        Err(FileError::Internal(err)) => return Err(err),
    }
    Ok(Outcome::Imported {
        files: 1,
        skipped: vec![],
        missing: vec![],
        unfiled: vec![],
        notes: vec![format!("saved in {}", root.join(&dest_dir).display())],
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
        let canonical = probe::validate(&staged.staged, &[root]).map_err(|e| {
            FileError::Blocked(match g.media_id {
                MediaId::Event(_) => R::CopyProbeFailed.encode(),
                _ => e,
            })
        })?;
        match probe::run(&lib.ffprobe, &canonical, lib.probe_timeout).await {
            Ok(pr) => pr.evidence(),
            Err(e) => {
                let (store, j, path, why) = (
                    st.store.clone(),
                    staged.journal,
                    staged.staged.clone(),
                    match g.media_id {
                        MediaId::Event(_) => R::CopyProbeFailed.encode(),
                        _ => format!("copied file failed the media probe: {e}"),
                    },
                );
                tokio::task::spawn_blocking(move || rollback_staged(&store, j, &path, &why))
                    .await
                    .map_err(|e| FileError::Internal(e.into()))??;
                tracing::warn!(grab = %g.id, "the copied file failed the media probe: {e}");
                return Err(FileError::Blocked(match g.media_id {
                    MediaId::Event(_) => R::CopyProbeFailed.encode(),
                    MediaId::Movie { .. }
                    | MediaId::Series(_)
                    | MediaId::Season(..)
                    | MediaId::Episode(..) => {
                        format!("the copied file failed the media probe: {e}")
                    }
                }));
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
    // The journal's `error` is returned by a route: for a sport grab it is only ever a sentence
    // the code built.
    let sport: bool = c
        .query_row(
            "SELECT g.media_id LIKE 'event:%' FROM import_journal j JOIN grabs g ON g.id = j.grab_id WHERE j.id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    let safe = error.filter(|_| sport).map(crate::sport::reason::classify);
    let error = if sport { safe.as_deref() } else { error };
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
    // A sport import's reasons are typed values (`sport::reason`); the others keep their text.
    let sport = !p.keep_apart.is_empty();
    let op = |what: crate::sport::reason::FileOp, kind: Option<std::io::ErrorKind>| R::FileOp {
        what,
        kind: kind.map(Into::into),
    };
    let src_meta = std::fs::symlink_metadata(&p.source).map_err(|e| {
        FileError::Blocked(if sport {
            op(FO::StatSource, Some(e.kind())).encode()
        } else {
            format!("{}: cannot stat the source: {}", p.rel, e.kind())
        })
    })?;
    if !src_meta.file_type().is_file() {
        return Err(FileError::Blocked(if sport {
            op(FO::NotRegular, None).encode()
        } else {
            format!("{} is not a regular file", p.rel)
        }));
    }
    if p.size > 0 && src_meta.len() != p.size {
        return Err(FileError::Blocked(if sport {
            op(FO::SizeDiffers, None).encode()
        } else {
            format!("{}: size on disk differs from the torrent", p.rel)
        }));
    }
    let dest_dir = paths::ensure_dirs(root, &p.dest_dir).map_err(|e| {
        FileError::Blocked(if sport {
            {
                tracing::warn!("sport folder unusable: {e}");
                R::SportFolderUnusable.encode()
            }
        } else {
            e
        })
    })?;
    let dest = dest_dir.join(&p.dest_name);
    let replaced_abs = p.replace.as_ref().map(|r| root.join(&r.path));
    if std::fs::symlink_metadata(&dest).is_ok() && replaced_abs.as_deref() != Some(dest.as_path()) {
        return Err(FileError::Blocked(if sport {
            R::DestinationTaken.encode()
        } else {
            format!(
                "{} already exists in the library; nothing is overwritten",
                dest.strip_prefix(root).unwrap_or(&dest).display()
            )
        }));
    }
    let (dest_dev, _) = crate::fsutil::file_id(&dest_dir).map_err(|e| {
        FileError::Blocked(if sport {
            op(FO::StatDestination, Some(e.kind())).encode()
        } else {
            format!("cannot stat the destination: {}", e.kind())
        })
    })?;
    let same_fs = crate::fsutil::file_id(&p.source).is_ok_and(|(dev, _)| dev == dest_dev);
    let mut method = if hardlinks && same_fs {
        "hardlink"
    } else {
        "copy"
    };
    let free = paths::free_bytes(&dest_dir).unwrap_or(0);
    let needed = if method == "copy" { src_meta.len() } else { 0 };
    if free < needed.saturating_add(min_free) {
        return Err(FileError::Blocked(if sport {
            R::NoSpace {
                free_mib: free / 1_048_576,
                needed_mib: needed / 1_048_576,
                min_mib: min_free / 1_048_576,
            }
            .encode()
        } else {
            format!(
                "not enough free space in the library ({} MiB free; {} MiB needed plus the {} MiB minimum)",
                free / 1_048_576,
                needed / 1_048_576,
                min_free / 1_048_576
            )
        }));
    }
    let targets =
        serde_json::to_string(&p.targets.iter().map(|t| t.to_string()).collect::<Vec<_>>())
            .map_err(|e| FileError::Internal(e.into()))?;
    let dest_rel = dest
        .strip_prefix(root)
        .ok()
        .and_then(paths::rel_string)
        .ok_or_else(|| {
            FileError::Blocked(if sport {
                op(FO::EscapedRoot, None).encode()
            } else {
                "destination escaped the root".into()
            })
        })?;
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
        let (stored, shown) = if sport {
            tracing::warn!(grab = %grab_id, "staging failed: {e}");
            let r = op(FO::LinkOrCopy, None).encode();
            (r.clone(), r)
        } else {
            (e.clone(), format!("{}: {e}", p.rel))
        };
        store.blocking(|c| journal_set(c, journal, "rolled_back", Some(&stored)))?;
        return Err(FileError::Blocked(shown));
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
    failpoint(grab_id, "after_stage")?;
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
    // A sport import checks again, immediately before the file goes in place, that its root is
    // still apart from the downloads and the other libraries by where it really is (a movie
    // or series import checks its root and what is below it when it stages, and not again).
    if !p.keep_apart.is_empty()
        && let Err(why) = confined(root, None, &p.keep_apart)
    {
        let _ = std::fs::remove_file(&s.staged);
        let why = why.encode();
        store.blocking(|c| journal_set(c, s.journal, "rolled_back", Some(&why)))?;
        return Err(FileError::Blocked(why));
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
                Some(&match g.media_id {
                    MediaId::Event(_) => R::PublishFailed {
                        kind: e.kind().into(),
                    }
                    .encode(),
                    _ => format!("publish failed: {}", e.kind()),
                }),
            )
        })?;
        return Err(FileError::Blocked(match g.media_id {
            MediaId::Event(_) => R::PublishFailed {
                kind: e.kind().into(),
            }
            .encode(),
            MediaId::Movie { .. }
            | MediaId::Series(_)
            | MediaId::Season(..)
            | MediaId::Episode(..) => format!("cannot publish {}: {}", s.dest.display(), e.kind()),
        }));
    }
    failpoint(&g.id, "after_link")?;
    // ...and once more with the file in place: where it really is must be under the sport root
    // and outside every download folder, or it is taken back through the journal. (Movies and
    // series do not verify after publishing.)
    if !p.keep_apart.is_empty()
        && let Err(why) = confined(root, Some(&s.dest), &p.keep_apart)
    {
        let _ = std::fs::remove_file(&s.dest);
        let _ = std::fs::remove_file(&s.staged);
        let why = why.encode();
        store.blocking(|c| journal_set(c, s.journal, "rolled_back", Some(&why)))?;
        return Err(FileError::Blocked(why));
    }
    let _ = std::fs::remove_file(&s.staged);
    let _ = crate::fsutil::fsync_dir(&s.dest_dir);
    store.blocking(|c| journal_set(c, s.journal, "published", None))?;
    failpoint(&g.id, "after_published")?;
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
        match g.media_id {
            // A sport file is recorded in `sport_files` and nowhere else: the `files` table
            // is never written for an event.
            MediaId::Event(e) => crate::sport::files::insert(
                &tx,
                &e.event_id(),
                &e.part.slug().unwrap_or_default(),
                &dest_rel,
                p.size,
                quality.as_deref(),
                probe_text,
                &g.id,
                &now_s,
            )?,
            MediaId::Movie { .. } | MediaId::Series(_) | MediaId::Season(..) | MediaId::Episode(..) => {
                // A stale row for the same path (e.g. a scan of an earlier copy) is retired.
                tx.execute("UPDATE files SET present = 0 WHERE media_id = ?1 AND path = ?2 AND present = 1", params![g.media_id.to_string(), dest_rel])?;
                library::insert_file(&tx, &g.media_id.to_string(), &ev, &p.targets, Some(&g.id), mtime)?;
            }
        }
        tx.execute("UPDATE import_journal SET state = 'committed', probe = ?2, updated_at = ?3 WHERE id = ?1", params![s.journal, probe_text, now_s])?;
        tx.execute("UPDATE grabs SET imported_files = imported_files + 1 WHERE id = ?1", [&g.id])?;
        library::event(
            &tx,
            &now_s,
            if p.replace.is_some() { "file_replaced" } else { "file_imported" },
            Some(&g.media_id.to_string()),
            Some(&p.targets[0].to_string()),
            Some(&g.id),
            &match g.media_id {
                // (a sport grab's title is the event's own name: the history begins with it)
                MediaId::Event(_) => format!("{}: filed as {dest_rel} ({})", g.title, s.method),
                MediaId::Movie { .. }
                | MediaId::Series(_)
                | MediaId::Season(..)
                | MediaId::Episode(..) => format!("{} -> {dest_rel} ({})", p.rel, s.method),
            },
            Some(&json!({ "method": s.method, "targets": p.targets, "replaced": p.replace.as_ref().map(|r| r.path.clone()), "probe": probe_text })),
        )?;
        tx.commit()?;
        Ok(())
    })?;
    failpoint(&g.id, "after_commit")?;
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
            replaced: vec![],
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
    fn a_pack_selection_picks_exactly_the_files_the_import_would_take() {
        let series: MediaId = "series:tvdb:1".parse().unwrap();
        let e = |n| library::episode_id(series, 1, n);
        let names: Vec<String> = [
            "Pack/Show.S01E01.1080p.mkv",
            "Pack/Show.S01E02E03.1080p.mkv",
            "Pack/Sample/Show.S01E04.sample.mkv",
            "Pack/Show.S01E04.1080p.mkv",
            "Pack/Show.S01E05.1080p.mkv",
            "Pack/Show.S01.nfo",
            "Pack/Show.S01E04.srt",
            ".pad/1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Episodes 1, 2, 3 and 5 are wanted (E04 is owned): a multi-episode file needs all
        // of its episodes to be wanted.
        let wanted = [e(1), e(2), e(3), e(5)];
        assert_eq!(only(&names, series, &wanted).unwrap(), Some(vec![0, 1, 4]));
        // The import agrees: the same files, and only those, are not skipped for coverage.
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
        let g = grab(
            series,
            wanted.iter().map(|t| (*t, false)).collect(),
            true,
            "Pack",
        );
        let entries: Vec<FileEntry> = names.iter().map(|n| entry(n)).collect();
        let s = select(&g, &entries, Path::new("/dl"), &item, true);
        let planned: Vec<&str> = s.planned.iter().map(|p| p.rel.as_str()).collect();
        assert_eq!(
            planned,
            [
                "Pack/Show.S01E01.1080p.mkv",
                "Pack/Show.S01E02E03.1080p.mkv",
                "Pack/Show.S01E05.1080p.mkv"
            ]
        );
        // Everything the pack holds is wanted: nothing is left out, all files are fetched.
        let all = [e(1), e(2), e(3), e(4), e(5)];
        assert_eq!(only(&names, series, &all).unwrap(), None);
        // Nothing the pack holds is wanted.
        let none = only(&names, series, &[e(9)]);
        assert!(none.unwrap_err().contains("no video file"));
        // Another season's file name is not for these episodes.
        let other = vec!["Pack/Show.S02E01.mkv".to_string()];
        assert!(only(&other, series, &[e(1)]).is_err());
    }

    fn only(
        names: &[String],
        series: MediaId,
        targets: &[MediaId],
    ) -> Result<Option<Vec<usize>>, String> {
        let files: Vec<(String, u64)> = names.iter().map(|n| (n.clone(), 10)).collect();
        pack_plan(&files, series, targets, &[]).map(|p| p.only)
    }

    /// A full season of six episodes whose E02 file is named only after the pack.
    fn season_pack(
        unnamed: &[(&str, u64)],
        owned_gap: Option<u16>,
    ) -> (MediaId, Vec<(String, u64)>, Vec<MediaId>) {
        let series: MediaId = "series:tvdb:1".parse().unwrap();
        let mut files: Vec<(String, u64)> = (1..=6u16)
            .filter(|n| *n != 2 && Some(*n) != owned_gap)
            .map(|n| (format!("Pack/Show.S01E{n:02}.1080p.mkv"), 250))
            .collect();
        files.extend(unnamed.iter().map(|(n, s)| (format!("Pack/{n}"), *s)));
        files.push(("Pack/Show.S01.nfo".into(), 1));
        let known = (1..=6).map(|n| library::episode_id(series, 1, n)).collect();
        (series, files, known)
    }

    #[test]
    fn the_last_episode_of_a_full_season_is_found_by_elimination() {
        let (series, files, known) = season_pack(&[("Show.S01.1080p.WEB.mkv", 230)], None);
        let targets = [1, 2, 4, 5, 6].map(|n| library::episode_id(series, 1, n));
        let plan = pack_plan(&files, series, &targets, &known).unwrap();
        let (i, ep) = plan.matched.unwrap();
        assert_eq!(files[i].0, "Pack/Show.S01.1080p.WEB.mkv");
        assert_eq!(ep, library::episode_id(series, 1, 2));
        assert!(plan.missing.is_empty());
        // E03 is owned: its file is left out, the unnamed one is selected.
        assert_eq!(plan.only.unwrap().len(), 5);

        // The import takes the same file for the same episode and says so.
        let item = ItemInfo {
            folder: "Show".into(),
            runtime: Some(45),
            episodes: (1..=6u16)
                .map(|n| (n, (Some(45), None)))
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .fold(Default::default(), |mut m: EpisodeMap, (n, v)| {
                    m.entry(1).or_default().insert(n, v);
                    m
                }),
            existing: None,
            series: Some(series),
            identity: None,
            profile: None,
            original_language: None,
        };
        let g = grab(
            series,
            targets.iter().map(|t| (*t, false)).collect(),
            true,
            "Pack",
        );
        let entries: Vec<FileEntry> = files
            .iter()
            .map(|(n, s)| FileEntry {
                size: *s,
                ..entry(n)
            })
            .collect();
        let s = select(&g, &entries, Path::new("/dl"), &item, true);
        assert!(s.blocker.is_none() && s.unfiled.is_empty());
        let p = s
            .planned
            .iter()
            .find(|p| p.rel == "Pack/Show.S01.1080p.WEB.mkv")
            .unwrap();
        assert_eq!(p.targets, vec![library::episode_id(series, 1, 2)]);
        assert_eq!(p.dest_name, "Show - S01E02.mkv");
        assert_eq!(s.notes.len(), 1);
        assert!(
            s.notes[0].contains("matched by elimination"),
            "{:?}",
            s.notes
        );
    }

    #[test]
    fn elimination_applies_only_to_one_unnamed_episode_sized_file_of_a_full_season() {
        // Two unnamed files: nothing says which is which.
        let (series, files, known) = season_pack(
            &[("Show.S01.1080p.WEB.mkv", 230), ("Show.S01.extra.mkv", 240)],
            None,
        );
        assert!(
            held_episodes(&cands(&files), series, &known, true)
                .eliminated
                .is_none()
        );
        // Sample-sized, or far too big for an episode.
        for size in [20, 100, 600] {
            let (series, files, known) = season_pack(&[("Show.S01.1080p.WEB.mkv", size)], None);
            assert!(
                held_episodes(&cands(&files), series, &known, true)
                    .eliminated
                    .is_none(),
                "{size}"
            );
        }
        // The only unnamed file is an extra by name: it is not taken for the missing episode,
        // which stays missing.
        for name in ["Show.S01.extra.mkv", "Show.S01.Bonus.Featurette.mkv"] {
            let (series, files, known) = season_pack(&[(name, 240)], None);
            assert!(
                held_episodes(&cands(&files), series, &known, true)
                    .eliminated
                    .is_none(),
                "{name}"
            );
            let targets = [1, 2, 4, 5, 6].map(|n| library::episode_id(series, 1, n));
            let plan = pack_plan(&files, series, &targets, &known).unwrap();
            assert_eq!(plan.missing, [library::episode_id(series, 1, 2)], "{name}");
        }
        // A sample by name is not a candidate at all.
        let (series, files, known) = season_pack(&[("Show.S01.sample.mkv", 230)], None);
        assert!(
            held_episodes(&cands(&files), series, &known, true)
                .eliminated
                .is_none()
        );
        // Not a full season.
        let (series, files, known) = season_pack(&[("Show.S01.1080p.WEB.mkv", 230)], None);
        assert!(
            held_episodes(&cands(&files), series, &known, false)
                .eliminated
                .is_none()
        );
        // Two episodes have no file (E02 and E05): the file could be either.
        let (series, files, known) = season_pack(&[("Show.S01.1080p.WEB.mkv", 230)], Some(5));
        assert!(
            held_episodes(&cands(&files), series, &known, true)
                .eliminated
                .is_none()
        );
        // The file names another season or episode.
        let (series, files, known) = season_pack(&[("Show.S02E01.1080p.mkv", 230)], None);
        assert!(
            held_episodes(&cands(&files), series, &known, true)
                .eliminated
                .is_none()
        );
        // Without the catalog's episodes nothing can be eliminated.
        let (series, files, _) = season_pack(&[("Show.S01.1080p.WEB.mkv", 230)], None);
        assert!(
            held_episodes(&cands(&files), series, &[], true)
                .eliminated
                .is_none()
        );
    }

    #[test]
    fn a_target_with_no_file_is_missing_and_a_pack_without_any_is_refused() {
        let (series, files, known) = season_pack(&[], None);
        // E02 has no file and nothing can stand in for it.
        let targets = [2, 4].map(|n| library::episode_id(series, 1, n));
        let plan = pack_plan(&files, series, &targets, &known).unwrap();
        assert_eq!(plan.missing, vec![library::episode_id(series, 1, 2)]);
        assert!(plan.matched.is_none());
        assert_eq!(plan.only.unwrap().len(), 1);
        let none = [library::episode_id(series, 1, 2)];
        assert!(pack_plan(&files, series, &none, &known).is_err());

        // A pack downloaded before this check: the import imports what is there and says which
        // target no file holds (E02), apart from one that has a file (E03, owned).
        let mut item = ItemInfo {
            folder: "Show".into(),
            runtime: Some(45),
            episodes: Default::default(),
            existing: None,
            series: Some(series),
            identity: None,
            profile: None,
            original_language: None,
        };
        for n in 1..=6u16 {
            item.episodes
                .entry(1)
                .or_default()
                .insert(n, (Some(45), None));
        }
        let wanted = [2, 3, 4].map(|n| library::episode_id(series, 1, n));
        let g = grab(
            series,
            wanted.iter().map(|t| (*t, false)).collect(),
            true,
            "Pack",
        );
        let entries: Vec<FileEntry> = files.iter().map(|(n, _)| entry(n)).collect();
        let s = select(&g, &entries, Path::new("/dl"), &item, true);
        assert_eq!(s.unfiled, vec![library::episode_id(series, 1, 2)]);
        assert!(s.notes.is_empty());
    }

    fn cands(files: &[(String, u64)]) -> Vec<(String, u64)> {
        files
            .iter()
            .filter_map(|(n, s)| candidate(n).map(|r| (r, *s)))
            .collect()
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
