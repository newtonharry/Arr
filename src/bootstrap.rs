//! Read-only bootstrap of an existing Plex library into the managed catalog, so a fresh
//! standalone deployment knows what is already owned before anything is requested.
//!
//! For each configured Plex section (`[plex] movie_section`/`tv_section`, with the Plex-side
//! root paths `movies_path`/`tv_path` mapped to `[library] movies`/`tv`):
//!
//! - items are enumerated page by page (`/library/sections/{id}/all?includeGuids=1`, then each
//!   show's `allLeaves`), bounded per batch; the cursor is durable, so a run resumes where it
//!   stopped and reports machine-readable progress, counts and issues;
//! - identity comes only from exact provider ids: TMDB for movies (an IMDB-only movie is
//!   resolved through Plex's metadata service, then TMDB's `find` when `[tmdb]` is configured,
//!   else reported unresolved),
//!   TVDB for shows; titles are never matched by name;
//! - ownership comes only from a validated local file: the Plex path must be inside the
//!   mapped Plex root, the relative name a plain path to a video file, reached without a
//!   symbolic link and present as a regular file under the local root; file names that name
//!   another title are refused; episodes are owned exactly as Plex numbers them (season and
//!   episode), never inferred from a series or season count, and specials are not supported;
//! - new titles are inserted owned-by-this-server but **not monitored** (nothing is searched
//!   or downloaded, no command is queued); a season's episode list is stored only when a full
//!   metadata list is available (TVmaze), otherwise it stays unknown, so missing episodes are
//!   never mistaken for a complete season;
//! - nothing that is already known is overwritten: existing titles keep their metadata and
//!   monitoring, and a target that already has present file evidence keeps it;
//! - a missing section, unmapped path or unavailable root fails the run and changes nothing.
//!
//! The library is only read (directory metadata); nothing is written there.

use crate::api::AppState;
use crate::catalog::{AltTitle, Episode, FileEvidence, Movie, SeasonInfo, Series};
use crate::error::{ApiError, ErrorCode};
use crate::ids::{MediaId, SeriesKey};
use crate::import::rfc3339;
use crate::library;
use crate::plex::Item;
use crate::settings::MediaKind;
use axum::http::StatusCode;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const DEFAULT_MOVIE_BATCH: u32 = 50;
pub const DEFAULT_SHOW_BATCH: u32 = 5;
const MAX_ISSUES: usize = 200;
const MAX_LEAVES: u64 = 5000;
const LEAF_PAGE: u32 = 200;
const SOURCE: &str = "unified-media (plex bootstrap)";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Counts {
    /// Plex items read.
    pub items_seen: usize,
    pub titles_created: usize,
    /// Titles already in the catalog (kept as they are).
    pub titles_existing: usize,
    pub files_recorded: usize,
    /// Files (or their targets) that already had present evidence.
    pub files_known: usize,
    pub episodes_owned: usize,
    /// No usable provider id.
    pub unresolved: usize,
    /// A file outside the configured Plex path.
    pub unmapped: usize,
    /// A mapped file that is not present under the local root.
    pub missing_locally: usize,
    /// Specials, unsafe names, symbolic links, non-video files.
    pub unsupported: usize,
    /// Identity conflicts (a name naming another title, duplicate ids, folder disagreement).
    pub conflicts: usize,
    /// Metadata lookups that failed (the item is retried on the next run).
    pub metadata_failures: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Issue {
    /// `unresolved`, `unmapped`, `missing_locally`, `unsupported`, `conflict`, `metadata`
    /// (a failed lookup; retried), or `note` (informational).
    pub kind: String,
    pub title: String,
    pub rating_key: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub media: MediaKind,
    pub section: Option<u32>,
    /// `running`, `done` or `failed` (the configuration prevents it; nothing changed).
    pub status: String,
    pub dry_run: bool,
    /// Items read so far (the resume cursor).
    pub offset: u64,
    pub total: Option<u64>,
    pub started_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub last_error: Option<String>,
    pub counts: Counts,
    /// The first issues met (bounded); `counts` has every one.
    pub issues: Vec<Issue>,
}

impl Run {
    fn new(media: MediaKind, section: Option<u32>, dry_run: bool, now: &str) -> Self {
        Run {
            media,
            section,
            status: "running".into(),
            dry_run,
            offset: 0,
            total: None,
            started_at: now.into(),
            updated_at: now.into(),
            finished_at: None,
            last_error: None,
            counts: Counts::default(),
            issues: vec![],
        }
    }

    fn issue(&mut self, kind: &str, item: &Item, detail: impl Into<String>) {
        match kind {
            "unresolved" => self.counts.unresolved += 1,
            "unmapped" => self.counts.unmapped += 1,
            "missing_locally" => self.counts.missing_locally += 1,
            "unsupported" => self.counts.unsupported += 1,
            "conflict" => self.counts.conflicts += 1,
            "note" => {}
            _ => self.counts.metadata_failures += 1,
        }
        if self.issues.len() < MAX_ISSUES {
            self.issues.push(Issue {
                kind: kind.into(),
                title: item.title.chars().take(200).collect(),
                rating_key: item.rating_key.clone(),
                detail: detail.into(),
            });
        }
    }
}

fn key(media: MediaKind, dry_run: bool) -> String {
    format!(
        "{}{}",
        match media {
            MediaKind::Movie => "movie",
            MediaKind::Series => "series",
        },
        if dry_run { ":dry_run" } else { "" }
    )
}

/// Every stored run.
pub fn runs(c: &rusqlite::Connection) -> anyhow::Result<Vec<Run>> {
    let mut stmt = c.prepare("SELECT state FROM bootstrap_runs ORDER BY media")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .iter()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect())
}

fn load(c: &rusqlite::Connection, k: &str) -> anyhow::Result<Option<Run>> {
    let s: Option<String> = c
        .query_row(
            "SELECT state FROM bootstrap_runs WHERE media = ?1",
            [k],
            |r| r.get(0),
        )
        .optional()?;
    Ok(s.and_then(|s| serde_json::from_str(&s).ok()))
}

fn save(c: &rusqlite::Connection, run: &Run) -> anyhow::Result<()> {
    c.execute(
        "INSERT INTO bootstrap_runs (media, state, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (media) DO UPDATE SET state = ?2, updated_at = ?3",
        params![
            key(run.media, run.dry_run),
            serde_json::to_string(run)?,
            run.updated_at
        ],
    )?;
    Ok(())
}

/// A validated local file for a Plex part.
#[derive(Debug, Clone)]
struct LocalFile {
    rel: String,
    size: u64,
    mtime: i64,
}

/// Maps a Plex file path into the local root and validates it (read-only).
fn local_file(
    plex_root: &str,
    root: &Path,
    plex_file: &str,
) -> Result<LocalFile, (&'static str, String)> {
    // Plex on Windows reports D:\Movies\x.mkv: compare in /-form, as relative paths are kept.
    let windows = |p: &str| {
        let b = p.as_bytes();
        (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || p.starts_with("\\\\")
    };
    let (plex_root, plex_file) = if windows(plex_root) {
        (plex_root.replace('\\', "/"), plex_file.replace('\\', "/"))
    } else {
        (plex_root.to_string(), plex_file.to_string())
    };
    let prefix = format!("{}/", plex_root.trim_end_matches('/'));
    let Some(rel) = plex_file.strip_prefix(&prefix) else {
        return Err((
            "unmapped",
            format!("the file is outside the configured Plex path {plex_root}"),
        ));
    };
    let Some(relp) = crate::paths::safe_rel(rel) else {
        return Err((
            "unsupported",
            "the file name is not a plain relative path".into(),
        ));
    };
    let ext_ok = rel.rsplit_once('.').is_some_and(|(_, e)| {
        crate::ingest::VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str())
    });
    if !ext_ok {
        return Err(("unsupported", "not a supported video file".into()));
    }
    let local = root.join(&relp);
    if let Err(e) = crate::paths::check_no_symlinks(root, &local) {
        return Err(("unsupported", format!("refused: {e}")));
    }
    match std::fs::symlink_metadata(&local) {
        Ok(m) if m.file_type().is_file() => Ok(LocalFile {
            rel: rel.to_string(),
            size: m.len(),
            mtime: crate::fsutil::mtime(&m),
        }),
        Ok(_) => Err(("unsupported", "not a regular file".into())),
        Err(e) => Err((
            "missing_locally",
            format!(
                "the mapped local file is not present ({}); check [plex] path mapping",
                e.kind()
            ),
        )),
    }
}

/// What one item contributes, decided before anything is written.
struct Planned {
    /// A new catalog title (inserted only when absent).
    movie: Option<Movie>,
    series: Option<(Series, Vec<Episode>)>,
    media_id: MediaId,
    /// Files with their exact targets.
    files: Vec<(LocalFile, Vec<MediaId>)>,
}

struct Ctx {
    plex_root: String,
    root: PathBuf,
    profile: Option<String>,
    native_root: Option<String>,
    now: OffsetDateTime,
}

/// Runs one bounded batch for `media` (persisting the cursor), or reports why it cannot.
pub async fn batch(
    st: &AppState,
    media: MediaKind,
    restart: bool,
    dry_run: bool,
    max_items: Option<u32>,
) -> Result<Run, ApiError> {
    let loaded = st.current().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::CatalogNotImported,
            "create managed settings first (arr setup, or POST /api/v1/setup)",
        )
    })?;
    if loaded.origin != "managed_setup" {
        return Err(ApiError::conflict(
            ErrorCode::SetupConflict,
            "a native snapshot import is active: its catalog already carries ownership evidence, so the Plex bootstrap (for standalone managed setups) is refused",
        ));
    }
    let plex = st.plex.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderNotConfigured,
            "Plex is not configured ([plex] base_url and token_file in config.toml)",
        )
    })?;
    let now = OffsetDateTime::now_utc();
    let now_s = rfc3339(now);
    let section = plex.section_for(media);
    let k = key(media, dry_run);
    let stored = {
        let k = k.clone();
        st.store
            .with(move |c| load(c, &k))
            .await
            .map_err(|e| ApiError::internal("bootstrap state", format!("{e:#}")))?
    };
    let mut run = match stored {
        // A run for another library (setup was retried with a different one) starts over.
        Some(r) if !restart && r.status != "failed" && r.section == section => r,
        _ => Run::new(media, section, dry_run, &now_s),
    };
    if run.status == "done" {
        return Ok(run);
    }
    run.updated_at = now_s.clone();
    run.section = section;
    // Configuration problems fail the run and change nothing.
    // (movies and TV only: the Plex refresh of a sport import is `worker::plex_refresh`'s)
    let (what, plex_root, root) = match media {
        MediaKind::Movie => (
            "movies",
            plex.config().movies_path.clone(),
            st.cfg.library.as_ref().and_then(|l| l.movies.clone()),
        ),
        MediaKind::Series => (
            "tv",
            plex.config().tv_path.clone(),
            st.cfg.library.as_ref().and_then(|l| l.tv.clone()),
        ),
    };
    let problem = match (&section, &plex_root, &root) {
        (None, _, _) => Some(format!(
            "no Plex section is configured for {what} ([plex] {})",
            if media == MediaKind::Movie {
                "movie_section"
            } else {
                "tv_section"
            }
        )),
        (_, None, _) => Some(format!(
            "the {what} root's Plex path is not configured ([plex] {what}_path): files cannot be mapped"
        )),
        (_, _, None) => Some(format!("no [library] {what} root is configured")),
        (_, _, Some(r)) => match std::fs::symlink_metadata(r) {
            Ok(m) if m.is_dir() => None,
            Ok(_) => Some(format!("the [library] {what} root is not a directory")),
            Err(e) => Some(format!(
                "the [library] {what} root is not accessible ({}); nothing changed",
                e.kind()
            )),
        },
    };
    if let Some(p) = problem {
        run.status = "failed".into();
        run.last_error = Some(p);
        let r2 = run.clone();
        st.store
            .with(move |c| save(c, &r2))
            .await
            .map_err(|e| ApiError::internal("bootstrap state", format!("{e:#}")))?;
        return Ok(run);
    }
    let (section, plex_root, root) = (
        section.expect("checked"),
        plex_root.expect("checked"),
        root.expect("checked"),
    );
    let ctx = Ctx {
        plex_root,
        root,
        profile: loaded.settings.sole_profile(media).map(|p| p.id.clone()),
        native_root: crate::ingest::native_root(st, media),
        now,
    };
    let size = max_items
        .unwrap_or(match media {
            MediaKind::Movie => DEFAULT_MOVIE_BATCH,
            MediaKind::Series => DEFAULT_SHOW_BATCH,
        })
        .clamp(1, crate::plex::MAX_PAGE);
    let page = match plex.section_items(section, run.offset, size).await {
        Ok(p) => p,
        Err(e) => {
            run.last_error = Some(format!("plex {}", e.describe()));
            let r2 = run.clone();
            let _ = st.store.with(move |c| save(c, &r2)).await;
            return Err(e.into_api("plex"));
        }
    };
    run.last_error = None;
    if page.total.is_some() {
        run.total = page.total;
    }
    let read = page.items.len() as u64;
    let mut planned = vec![];
    let mut retry_from: Option<u64> = None;
    for (i, item) in page.items.iter().enumerate() {
        run.counts.items_seen += 1;
        let expected = match media {
            MediaKind::Movie => "movie",
            MediaKind::Series => "show",
        };
        if item.kind != expected {
            run.issue(
                "unsupported",
                item,
                format!("a {} item in a {expected} section", item.kind),
            );
            continue;
        }
        let r = match media {
            MediaKind::Movie => plan_movie(st, &ctx, item, &mut run).await,
            MediaKind::Series => plan_show(st, &ctx, plex, item, &mut run).await,
        };
        match r {
            Ok(Some(p)) => planned.push(p),
            Ok(None) => {}
            // A metadata outage: stop here and resume at this item next time.
            Err(e) => {
                run.issue("metadata", item, e.clone());
                run.last_error = Some(format!("{}: {e}; the run resumes at this item", item.title));
                run.counts.items_seen -= 1;
                retry_from = Some(run.offset + i as u64);
                break;
            }
        }
    }
    // One transaction per batch, serialised with library mutations.
    if !dry_run && !planned.is_empty() {
        let _guard = st.library_lock().lock().await;
        let now2 = now;
        let applied = st
            .store
            .with(move |c| apply(c, planned, now2))
            .await
            .map_err(|e| ApiError::internal("bootstrap apply", format!("{e:#}")))?;
        add_counts(&mut run.counts, &applied);
        st.reload_index()
            .await
            .map_err(|e| ApiError::internal("bootstrap index", format!("{e:#}")))?;
    } else if dry_run {
        for p in &planned {
            run.counts.titles_created += usize::from(p.movie.is_some() || p.series.is_some());
            run.counts.files_recorded += p.files.len();
        }
    }
    run.offset = retry_from.unwrap_or(run.offset + read);
    let finished = retry_from.is_none()
        && (read == 0 || read < u64::from(size) || run.total.is_some_and(|t| run.offset >= t));
    if finished {
        run.status = "done".into();
        run.finished_at = Some(now_s.clone());
    }
    run.updated_at = rfc3339(OffsetDateTime::now_utc());
    let r2 = run.clone();
    st.store
        .with(move |c| save(c, &r2))
        .await
        .map_err(|e| ApiError::internal("bootstrap state", format!("{e:#}")))?;
    Ok(run)
}

fn add_counts(c: &mut Counts, d: &Counts) {
    c.titles_created += d.titles_created;
    c.titles_existing += d.titles_existing;
    c.files_recorded += d.files_recorded;
    c.files_known += d.files_known;
    c.episodes_owned += d.episodes_owned;
}

/// Validates every part of an item, sorting failures into issues.
fn files_of(ctx: &Ctx, item: &Item, run: &mut Run) -> Vec<LocalFile> {
    let mut out: Vec<LocalFile> = vec![];
    for (plex_file, _) in &item.parts {
        match local_file(&ctx.plex_root, &ctx.root, plex_file) {
            Ok(f) if !out.iter().any(|x| x.rel == f.rel) => out.push(f),
            Ok(_) => {}
            Err((kind, detail)) => run.issue(kind, item, detail),
        }
    }
    out
}

fn first_component(rel: &str) -> Option<&str> {
    rel.split_once('/').map(|(a, _)| a)
}

async fn plan_movie(
    st: &AppState,
    ctx: &Ctx,
    item: &Item,
    run: &mut Run,
) -> Result<Option<Planned>, String> {
    let tmdb = match item
        .provider_id("tmdb")
        .and_then(|t| t.parse::<u32>().ok())
        .filter(|t| *t > 0)
    {
        Some(t) => t,
        None => match (item.provider_id("imdb"), &st.movies) {
            (Some(imdb), Some(t)) => match t.find_imdb(&imdb).await {
                Ok(Some(id)) => id,
                Ok(None) => {
                    run.issue(
                        "unresolved",
                        item,
                        format!("no single movie is known for {imdb}"),
                    );
                    return Ok(None);
                }
                Err(e) => return Err(format!("movie metadata {}", e.describe())),
            },
            (Some(imdb), None) => {
                run.issue("unresolved", item, format!("only an IMDB id ({imdb}) and no movie metadata source (Plex sign-in or TMDB) to resolve it"));
                return Ok(None);
            }
            (None, _) => {
                run.issue("unresolved", item, "no TMDB or IMDB id in Plex");
                return Ok(None);
            }
        },
    };
    let files = files_of(ctx, item, run);
    if files.is_empty() {
        return Ok(None);
    }
    let folders: BTreeSet<Option<&str>> = files.iter().map(|f| first_component(&f.rel)).collect();
    if folders.len() > 1 {
        run.issue(
            "conflict",
            item,
            "the movie's files are in different folders",
        );
        return Ok(None);
    }
    let folder = folders.into_iter().next().flatten().map(str::to_string);
    let index = st.index();
    let id = MediaId::Movie { tmdb };
    let movie = match index.movie(tmdb) {
        Some(existing) => {
            if let (Some(a), Some(b)) = (&existing.folder, &folder)
                && a != b
            {
                run.issue("conflict", item, format!("the catalog files this movie under {a:?}, Plex under {b:?}; nothing changed"));
                return Ok(None);
            }
            existing.clone()
        }
        None => {
            // The library import already has the title, year and length from the user's own Plex
            // library, so Plex's metadata service is not asked about every movie; a TMDB key adds
            // release dates, alternative titles and the original language.
            let mut m = match st.movies.as_ref().and_then(|m| m.tmdb()) {
                Some(t) => match t.movie(tmdb).await {
                    Ok(Some(d)) => library::new_movie(
                        tmdb,
                        d.title.clone(),
                        crate::metadata::year_of(d.release_date.as_deref()),
                        d.runtime,
                        d.original_language
                            .as_deref()
                            .and_then(crate::metadata::language_name)
                            .map(str::to_owned),
                        d.alternative_titles
                            .titles
                            .iter()
                            .map(|a| a.title.clone())
                            .take(20)
                            .collect(),
                        d.imdb_id.clone(),
                        ctx.profile.clone(),
                        ctx.native_root.clone(),
                        folder.clone(),
                        d.release(),
                        SOURCE,
                        ctx.now,
                    ),
                    Ok(None) => {
                        run.issue("unresolved", item, format!("TMDB has no movie {tmdb}"));
                        return Ok(None);
                    }
                    Err(e) => return Err(format!("tmdb {}", e.describe())),
                },
                None => library::new_movie(
                    tmdb,
                    item.title.clone(),
                    item.year,
                    item.duration_ms
                        .map(|d| (d / 60_000) as u32)
                        .filter(|m| *m > 0),
                    None,
                    vec![],
                    item.provider_id("imdb"),
                    ctx.profile.clone(),
                    ctx.native_root.clone(),
                    folder.clone(),
                    crate::catalog::ReleaseDates {
                        in_cinemas: item.originally_available.clone(),
                        ..Default::default()
                    },
                    SOURCE,
                    ctx.now,
                ),
            };
            m.monitored = false;
            if m.title.trim().is_empty() {
                run.issue("unresolved", item, "no title");
                return Ok(None);
            }
            m
        }
    };
    let identity = crate::identity::Identity::movie(&movie);
    let mut kept = vec![];
    for f in files {
        if identity.contradiction(f.rel.split('/')).is_some() {
            run.issue("conflict", item, format!("{} names another title", f.rel));
        } else {
            kept.push((f, vec![id]));
        }
    }
    if kept.is_empty() {
        return Ok(None);
    }
    Ok(Some(Planned {
        movie: index.movie(tmdb).is_none().then_some(movie),
        series: None,
        media_id: id,
        files: kept,
    }))
}

async fn plan_show(
    st: &AppState,
    ctx: &Ctx,
    plex: &crate::plex::Plex,
    item: &Item,
    run: &mut Run,
) -> Result<Option<Planned>, String> {
    let Some(tvdb) = item
        .provider_id("tvdb")
        .and_then(|t| t.parse::<u32>().ok())
        .filter(|t| *t > 0)
    else {
        run.issue(
            "unresolved",
            item,
            "no TVDB id in Plex (series are keyed by TVDB)",
        );
        return Ok(None);
    };
    let key = SeriesKey::Tvdb(tvdb);
    let id = MediaId::Series(key);
    // Every episode Plex has, with its exact numbering.
    let mut leaves: Vec<Item> = vec![];
    let mut start = 0u64;
    loop {
        let page = plex
            .leaves(&item.rating_key, start, LEAF_PAGE)
            .await
            .map_err(|e| format!("plex {}", e.describe()))?;
        let n = page.items.len() as u64;
        leaves.extend(page.items);
        start += n;
        if n < u64::from(LEAF_PAGE) || page.total.is_some_and(|t| start >= t) || start >= MAX_LEAVES
        {
            break;
        }
    }
    let mut by_file: BTreeMap<String, (LocalFile, BTreeSet<(u16, u16)>)> = BTreeMap::new();
    for leaf in &leaves {
        let (Some(season), Some(number)) = (
            leaf.parent_index.and_then(|v| u16::try_from(v).ok()),
            leaf.index.and_then(|v| u16::try_from(v).ok()),
        ) else {
            run.issue(
                "unsupported",
                item,
                format!("episode {:?} has no season/episode number", leaf.title),
            );
            continue;
        };
        if season == 0 || number == 0 {
            run.issue(
                "unsupported",
                item,
                format!("S{season:02}E{number:02}: specials are not supported"),
            );
            continue;
        }
        for f in files_of(ctx, leaf, run) {
            by_file
                .entry(f.rel.clone())
                .or_insert_with(|| (f, BTreeSet::new()))
                .1
                .insert((season, number));
        }
    }
    if by_file.is_empty() {
        return Ok(None);
    }
    // One series folder: the most common one; files elsewhere are conflicts.
    let mut folders: BTreeMap<&str, usize> = BTreeMap::new();
    for rel in by_file.keys() {
        if let Some(f) = first_component(rel) {
            *folders.entry(f).or_default() += 1;
        }
    }
    let Some(folder) = folders
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(f, _)| f.to_string())
    else {
        run.issue(
            "unsupported",
            item,
            "episode files directly in the library root (a series folder is needed)",
        );
        return Ok(None);
    };
    let index = st.index();
    let existing = index.series(key).cloned();
    if let Some(s) = &existing
        && let Some(a) = &s.folder
        && *a != folder
    {
        run.issue(
            "conflict",
            item,
            format!(
                "the catalog files this series under {a:?}, Plex under {folder:?}; nothing changed"
            ),
        );
        return Ok(None);
    }
    let created = existing.is_none();
    let (mut series, episodes) = match existing {
        Some(s) => (s, vec![]),
        None => {
            // A full episode list only from metadata; Plex lists only what it has.
            let (show, eps) = match &st.tvmaze {
                Some(tv) => match tv.by_tvdb(tvdb).await {
                    Ok(Some(show)) => match tv.episodes(show.id).await {
                        Ok(eps) => (Some(show), eps),
                        Err(e) => return Err(format!("tvmaze {}", e.describe())),
                    },
                    Ok(None) => (None, vec![]),
                    Err(e) => return Err(format!("tvmaze {}", e.describe())),
                },
                None => (None, vec![]),
            };
            let list: Vec<Episode> = eps
                .iter()
                .filter_map(|e| {
                    Some(Episode {
                        season: u16::try_from(e.season?).ok().filter(|s| *s > 0)?,
                        number: u16::try_from(e.number?).ok().filter(|n| *n > 0)?,
                        title: e.name.clone(),
                        air_date_utc: e
                            .airstamp
                            .as_deref()
                            .and_then(|a| OffsetDateTime::parse(a, &Rfc3339).ok())
                            .map(rfc3339),
                        runtime_minutes: e.runtime.filter(|r| *r > 0),
                        has_file: false,
                        monitored: false,
                        file_quality: None,
                        file: None,
                        ownership_unknown: false,
                        queued: false,
                        replaceable: false,
                    })
                })
                .collect();
            let title = show
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| item.title.clone());
            if title.trim().is_empty() {
                run.issue("unresolved", item, "no title");
                return Ok(None);
            }
            let mut s = library::new_series(
                key,
                title,
                show.as_ref().and_then(|s| s.year()).or(item.year),
                show.as_ref().and_then(|s| s.runtime.or(s.average_runtime)),
                show.as_ref().and_then(|s| s.language.clone()),
                show.as_ref().map(|s| s.id),
                ctx.profile.clone(),
                ctx.native_root.clone(),
                Some(folder.clone()),
                SOURCE,
                ctx.now,
            );
            // Plex's own title for this exact TVDB id stays an alternate title when TVmaze names
            // the show differently (an English title for a show TVmaze lists under its
            // original-language one): the library's files usually carry it.
            if show.is_some()
                && let Some(plex_title) = Some(item.title.trim()).filter(|t| {
                    !t.is_empty() && t.chars().count() <= 200 && !t.chars().any(char::is_control)
                })
                && !crate::catalog::title_keys(&s.title, &[]).contains(&crate::title::key(
                    crate::title::split_trailing_year(plex_title).0,
                ))
            {
                s.alternate_titles.push(AltTitle {
                    title: plex_title.to_string(),
                    season: None,
                });
                run.issue(
                    "note",
                    item,
                    format!(
                        "TVmaze names this show {:?}; Plex's title {plex_title:?} (the same TVDB id) is kept as an alternate title",
                        s.title
                    ),
                );
            }
            s.monitored = false;
            s.tmdb_id = item.provider_id("tmdb").and_then(|t| t.parse().ok());
            s.imdb_id = item.provider_id("imdb");
            s.monitor_new_items = Some("none".into());
            let mut numbers: BTreeSet<u16> = list.iter().map(|e| e.season).collect();
            numbers.extend(
                by_file
                    .values()
                    .flat_map(|(_, eps)| eps.iter().map(|e| e.0)),
            );
            s.seasons = numbers
                .into_iter()
                .map(|n| SeasonInfo {
                    number: n,
                    monitored: false,
                    episode_file_count: None,
                    episode_count: None,
                    total_episode_count: None,
                })
                .collect();
            if list.is_empty() {
                run.issue("note", item, "no full episode list (TVmaze disabled or without this show): episode ownership is recorded, season lists stay unknown until TVmaze provides them");
            }
            (s, list)
        }
    };
    // Files that still name another title may name the show by an official TVmaze AKA of this
    // exact show. Only an AKA that the library's own file names match exactly is kept (as an
    // alternate title, so scans and decisions agree); nothing is abbreviated or fuzzy-matched,
    // and anything else stays a conflict. Asked only for a new show with such files.
    if created && let (Some(tv), Some(show_id)) = (&st.tvmaze, series.tvmaze_id) {
        let inner = |rel: &str| rel.split_once('/').map_or(rel, |(_, r)| r).to_string();
        let identity = crate::identity::Identity::series(&series);
        let mut open: Vec<String> = by_file
            .keys()
            .filter(|rel| first_component(rel) == Some(folder.as_str()))
            .map(|rel| inner(rel))
            .filter(|i| identity.contradiction(i.split('/')).is_some())
            .collect();
        if !open.is_empty() {
            let akas = tv
                .akas(show_id)
                .await
                .map_err(|e| format!("tvmaze {}", e.describe()))?;
            for aka in akas {
                if open.is_empty() {
                    break;
                }
                let mut trial = series.clone();
                trial.alternate_titles.push(AltTitle {
                    title: aka.clone(),
                    season: None,
                });
                let t = crate::identity::Identity::series(&trial);
                let rest: Vec<String> = open
                    .iter()
                    .filter(|i| t.contradiction(i.split('/')).is_some())
                    .cloned()
                    .collect();
                if rest.len() < open.len() {
                    run.issue(
                        "note",
                        item,
                        format!(
                            "{} file(s) name the show {aka:?}, an official TVmaze alternative title of this show; kept as an alternate title",
                            open.len() - rest.len()
                        ),
                    );
                    series = trial;
                    open = rest;
                }
            }
        }
    }
    let identity = crate::identity::Identity::series(&series);
    let known: BTreeSet<(u16, u16)> = if episodes.is_empty() {
        series
            .episodes
            .iter()
            .flatten()
            .map(|e| (e.season, e.number))
            .collect()
    } else {
        episodes.iter().map(|e| (e.season, e.number)).collect()
    };
    let mut files = vec![];
    for (rel, (f, eps)) in by_file {
        if first_component(&rel) != Some(folder.as_str()) {
            run.issue(
                "conflict",
                item,
                format!("{rel} is outside the series folder {folder:?}"),
            );
            continue;
        }
        let inner = rel.split_once('/').map_or(rel.as_str(), |(_, r)| r);
        if identity.contradiction(inner.split('/')).is_some() {
            run.issue("conflict", item, format!("{rel} names another title"));
            continue;
        }
        if !known.is_empty()
            && let Some((s, e)) = eps.iter().find(|x| !known.contains(x))
        {
            run.issue("note", item, format!("S{s:02}E{e:02} is not in the metadata episode list; its ownership is recorded but no episode row is invented"));
        }
        let targets = eps
            .iter()
            .map(|(s, e)| library::episode_id(id, *s, *e))
            .collect();
        files.push((f, targets));
    }
    if files.is_empty() {
        return Ok(None);
    }
    Ok(Some(Planned {
        movie: None,
        series: created.then_some((series, episodes)),
        media_id: id,
        files,
    }))
}

/// Writes a batch (insert-only) and returns the counts it changed.
fn apply(
    c: &mut rusqlite::Connection,
    planned: Vec<Planned>,
    now: OffsetDateTime,
) -> anyhow::Result<Counts> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now_s = rfc3339(now);
    let mut out = Counts::default();
    for p in planned {
        let created = match (&p.movie, &p.series) {
            (Some(m), _) => library::bootstrap_movie(&tx, m, now)?,
            (_, Some((s, eps))) => library::bootstrap_series(&tx, s, eps, now)?,
            _ => false,
        };
        if created {
            out.titles_created += 1;
        } else {
            out.titles_existing += 1;
        }
        let mid = p.media_id.to_string();
        for (f, targets) in p.files {
            let path_known: bool = tx
                .query_row(
                    "SELECT 1 FROM files WHERE media_id = ?1 AND path = ?2 AND present = 1",
                    params![mid, f.rel],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            let mut open = vec![];
            for t in &targets {
                if !library::has_present_file(&tx, &t.to_string())? {
                    open.push(*t);
                }
            }
            if path_known || open.is_empty() {
                out.files_known += 1;
                continue;
            }
            let mut ev: FileEvidence = crate::scan::file_evidence(&f.rel, f.size, &now_s);
            ev.source = "plex".into();
            library::insert_file(&tx, &mid, &ev, &open, None, Some(f.mtime))?;
            out.files_recorded += 1;
            if matches!(p.media_id, MediaId::Series(_)) {
                out.episodes_owned += open.len();
            }
        }
    }
    tx.commit()?;
    Ok(out)
}

#[cfg(all(test, unix))] // makes symlinks
mod tests {
    use super::*;

    #[test]
    fn local_files_are_mapped_and_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("movies");
        std::fs::create_dir_all(root.join("The Matrix (1999)")).unwrap();
        std::fs::write(root.join("The Matrix (1999)/The Matrix (1999).mkv"), b"x").unwrap();
        std::fs::write(root.join("The Matrix (1999)/notes.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("The Matrix (1999)"), root.join("Link")).unwrap();
        let ok = local_file(
            "/media_library/movies",
            &root,
            "/media_library/movies/The Matrix (1999)/The Matrix (1999).mkv",
        )
        .unwrap();
        assert_eq!(
            (ok.rel.as_str(), ok.size),
            ("The Matrix (1999)/The Matrix (1999).mkv", 1)
        );
        let kind = |p: &str| local_file("/media_library/movies", &root, p).unwrap_err().0;
        assert_eq!(kind("/elsewhere/The Matrix (1999)/x.mkv"), "unmapped");
        assert_eq!(
            kind("/media_library/movies2/x.mkv"),
            "unmapped",
            "a prefix is not a parent"
        );
        assert_eq!(kind("/media_library/movies/../etc/x.mkv"), "unsupported");
        assert_eq!(
            kind("/media_library/movies/The Matrix (1999)/notes.txt"),
            "unsupported"
        );
        assert_eq!(
            kind("/media_library/movies/Link/The Matrix (1999).mkv"),
            "unsupported",
            "symlinks refused"
        );
        assert_eq!(
            kind("/media_library/movies/Gone (2001)/Gone.mkv"),
            "missing_locally"
        );
    }
}
