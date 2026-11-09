//! Downloads: the durable grab outbox, qBittorrent submission and reconciliation, polling and
//! status mapping, failure classification with blocklisting, user actions, and the
//! completed/failed download cleanup policy.
//!
//! Identity: every grab records the torrent info-hash before anything is sent. A grab is
//! `unconfirmed` while an add is in flight; any later attempt first looks the hash up, so a
//! timeout, an ambiguous answer or a crash never produces a duplicate. Ownership: a torrent is
//! ours only if its hash belongs to an active (or cleanup-tracked) grab, its category is the
//! grab's category and it carries the `unified-media` tag. Anything else is foreign and is
//! never paused, resumed, removed or deleted.
//!
//! Cleanup (native `removeCompletedDownloads` / `removeFailedDownloads`): after a verified,
//! committed import, a download is removed with its data only when qBittorrent has stopped it
//! at a share limit that is actually met (per-torrent, global or none, evaluated explicitly)
//! and the indexer seed goal recorded at grab time is met. Unknown or unmet criteria keep it
//! seeding. The removal is journaled (state `removing` plus the exact file list) before the
//! client is asked, so a crash after deletion is finished on restart and never mistaken for a
//! vanished download. Library files are separate links or copies and are never touched.

use crate::api::AppState;
use crate::client::Client;
use crate::config::MigrationMode;
use crate::ids::MediaId;
use crate::import::rfc3339;
use crate::library;
use crate::paths;
use crate::prowlarr::Payload;
use crate::qbit::{AddRequest, ClientState, Preferences, TorrentInfo};
use crate::quality::Quality;
use crate::release::Revision;
use crate::settings::{MediaKind, Settings};
use crate::upstream::UpstreamError;
use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Tag on every torrent this server submits.
pub const TAG: &str = "unified-media";
/// Submission attempts before a grab fails.
pub const MAX_SUBMIT_ATTEMPTS: i64 = 6;
/// A torrent missing from the client for this long is treated as removed.
pub const ABSENT_GRACE: Duration = Duration::from_secs(10 * 60);
/// A client error or missing-files state lasting this long fails the download.
pub const ERROR_GRACE: Duration = Duration::from_secs(30 * 60);
/// Automatic searches for cancelled targets wait this long.
pub const CANCEL_COOLDOWN: Duration = Duration::from_secs(24 * 3600);
/// Our own unlink fallback starts this long after the client was asked to delete.
const CLIENT_DELETE_GRACE: Duration = Duration::from_secs(60);

pub const ACTIVE_STATES: &[&str] = &[
    "queued",
    "unconfirmed",
    "downloading",
    "completed",
    "importing",
    "import_blocked",
];

fn ts(t: OffsetDateTime) -> String {
    rfc3339(t)
}

fn parse_ts(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

fn since(s: Option<&str>, now: OffsetDateTime) -> Option<Duration> {
    let t = parse_ts(s?)?;
    Some(Duration::try_from(now - t).unwrap_or(Duration::ZERO))
}

// ------------------------------------------------------------------ release links

/// Private link material for a search result, persisted so a plan or an interactive grab can
/// be executed later. Never serialised into API responses.
#[derive(Debug, Clone)]
pub struct ReleaseLink {
    pub release_ref: String,
    pub target: MediaId,
    pub media_id: MediaId,
    pub info_hash: Option<String>,
    pub magnet: Option<String>,
    pub download_url: Option<String>,
    pub title: String,
    pub size: Option<i64>,
    pub indexer_id: Option<i64>,
    pub quality: Quality,
    pub revision: Revision,
    pub full_season: bool,
    pub covers: Vec<MediaId>,
    pub replaces: Vec<MediaId>,
    pub interactive: bool,
}

/// Whether any of `targets` is covered by no active grab (the coverage the outbox guards).
pub fn any_uncovered(c: &Connection, targets: &[String]) -> Result<bool> {
    let mut stmt = c.prepare(
        "SELECT 1 FROM grab_targets t JOIN grabs g ON g.id = t.grab_id WHERE g.active = 1 AND t.target = ?1",
    )?;
    for t in targets {
        if !stmt.exists([t])? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Link material for every approved candidate of a search run.
pub fn links_for(
    run: &crate::search::SearchRun,
    target: &crate::target::Target,
    interactive: bool,
) -> Vec<ReleaseLink> {
    run.candidates
        .iter()
        .filter(|c| c.decision.approved)
        .filter_map(|c| link_for(c, target, interactive))
        .collect()
}

pub fn link_for(
    c: &crate::search::Candidate,
    target: &crate::target::Target,
    interactive: bool,
) -> Option<ReleaseLink> {
    use crate::target::Target;
    if c.release.magnet_url.is_none() && c.release.download_url.is_none() {
        return None;
    }
    let (media_id, covers, replaces) = match target {
        Target::Movie(m) => (
            m.id,
            vec![m.id],
            if c.decision.replacement {
                vec![m.id]
            } else {
                vec![]
            },
        ),
        Target::Season(s) | Target::Episode { season: s, .. } => {
            let ep = |n: &u16| library::episode_id(s.series.id, s.season, *n);
            let replaces = if c.decision.replacement {
                c.decision.owned_overlap.iter().map(ep).collect()
            } else {
                vec![]
            };
            (
                s.series.id,
                c.decision.covered_episodes.iter().map(ep).collect(),
                replaces,
            )
        }
    };
    if covers.is_empty() {
        return None;
    }
    Some(ReleaseLink {
        release_ref: c.release_ref.clone(),
        target: crate::plan::canonical_id(target),
        media_id,
        info_hash: c
            .release
            .info_hash
            .as_deref()
            .map(|h| h.trim().to_ascii_lowercase()),
        magnet: c
            .release
            .magnet_url
            .as_ref()
            .map(|l| l.expose().to_string()),
        download_url: c
            .release
            .download_url
            .as_ref()
            .map(|l| l.expose().to_string()),
        title: c
            .release
            .title
            .clone()
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect(),
        size: c.release.size,
        indexer_id: c.release.indexer_id,
        quality: c.parsed.quality,
        revision: c.parsed.revision,
        full_season: c.decision.full_season,
        covers,
        replaces,
        interactive,
    })
}

fn ids_json(ids: &[MediaId]) -> String {
    serde_json::to_string(&ids.iter().map(|i| i.to_string()).collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".into())
}

fn ids_parse(s: &str) -> Vec<MediaId> {
    serde_json::from_str::<Vec<String>>(s)
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i.parse().ok())
        .collect()
}

pub fn save_links(c: &Connection, links: &[ReleaseLink], now: OffsetDateTime) -> Result<()> {
    for l in links {
        c.execute(
            "INSERT INTO release_links (release_ref, target, media_id, info_hash, magnet, download_url, title, size, indexer_id, quality, revision_version, revision_real, full_season, covers, replaces, interactive, seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT (release_ref, target) DO UPDATE SET info_hash = excluded.info_hash, magnet = excluded.magnet, download_url = excluded.download_url,
               title = excluded.title, size = excluded.size, indexer_id = excluded.indexer_id, quality = excluded.quality, revision_version = excluded.revision_version,
               revision_real = excluded.revision_real, full_season = excluded.full_season, covers = excluded.covers, replaces = excluded.replaces,
               interactive = MAX(interactive, excluded.interactive), seen_at = excluded.seen_at",
            params![
                l.release_ref, l.target.to_string(), l.media_id.to_string(), l.info_hash, l.magnet, l.download_url, l.title, l.size, l.indexer_id,
                l.quality.as_str(), i64::from(l.revision.version), i64::from(l.revision.real), l.full_season, ids_json(&l.covers), ids_json(&l.replaces),
                l.interactive, ts(now)
            ],
        )?;
    }
    Ok(())
}

/// Link material for `release_ref`, preferring the row recorded for `target`.
pub fn load_link(
    c: &Connection,
    release_ref: &str,
    target: Option<MediaId>,
) -> Result<Option<ReleaseLink>> {
    let mut stmt = c.prepare(
        "SELECT release_ref, target, media_id, info_hash, magnet, download_url, title, size, indexer_id, quality, revision_version, revision_real, full_season, covers, replaces, interactive
         FROM release_links WHERE release_ref = ?1 ORDER BY CASE WHEN target = ?2 THEN 0 ELSE 1 END, seen_at DESC LIMIT 1",
    )?;
    let row = stmt
        .query_row(params![release_ref, target.map(|t| t.to_string())], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<i64>>(10)?,
                r.get::<_, Option<i64>>(11)?,
                r.get::<_, bool>(12)?,
                r.get::<_, String>(13)?,
                r.get::<_, String>(14)?,
                r.get::<_, bool>(15)?,
            ))
        })
        .optional()?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some(ReleaseLink {
        release_ref: r.0,
        target: r
            .1
            .parse()
            .map_err(|_| anyhow!("stored target is invalid"))?,
        media_id: r
            .2
            .parse()
            .map_err(|_| anyhow!("stored media id is invalid"))?,
        info_hash: r.3,
        magnet: r.4,
        download_url: r.5,
        title: r.6,
        size: r.7,
        indexer_id: r.8,
        quality: r
            .9
            .and_then(|q| serde_json::from_value(json!(q)).ok())
            .unwrap_or(Quality::Unknown),
        revision: Revision {
            version: r.10.and_then(|v| u8::try_from(v).ok()).unwrap_or(1),
            real: r.11.and_then(|v| u8::try_from(v).ok()).unwrap_or(0),
        },
        full_season: r.12,
        covers: ids_parse(&r.13),
        replaces: ids_parse(&r.14),
        interactive: r.15,
    }))
}

// ------------------------------------------------------------------ grabs

#[derive(Debug, Clone, Serialize)]
pub struct GrabTarget {
    pub target: MediaId,
    pub replaces: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Grab {
    pub id: String,
    pub media_id: MediaId,
    pub target: MediaId,
    pub plan_id: Option<String>,
    pub release_ref: String,
    /// Raw info-hash: internal identity only, never in API views or history.
    #[serde(skip)]
    pub info_hash: String,
    pub title: String,
    pub indexer_id: Option<i64>,
    pub size: Option<i64>,
    pub quality: Option<String>,
    pub revision: Option<Revision>,
    pub full_season: bool,
    pub category: String,
    pub save_path: String,
    pub seed_ratio: Option<f64>,
    pub seed_time_minutes: Option<i64>,
    pub remove_completed: bool,
    pub remove_failed: bool,
    pub origin: String,
    pub state: String,
    pub active: bool,
    pub attempts: i64,
    pub next_attempt_at: String,
    pub progress: Option<f64>,
    pub client_state: Option<String>,
    pub client_name: Option<String>,
    pub last_error: Option<String>,
    pub user_paused: bool,
    pub stalled_since: Option<String>,
    pub problem_since: Option<String>,
    pub absent_since: Option<String>,
    pub last_progress: Option<f64>,
    pub failure: Option<String>,
    pub retry: Option<String>,
    pub cleanup: String,
    pub cleanup_detail: Option<String>,
    pub cleanup_at: Option<String>,
    /// Journaled local paths of a removal in progress (private).
    #[serde(skip)]
    pub cleanup_files: Option<String>,
    pub imported_files: i64,
    pub created_at: String,
    pub updated_at: String,
    pub submitted_at: Option<String>,
    pub completed_at: Option<String>,
    pub imported_at: Option<String>,
    pub targets: Vec<GrabTarget>,
}

const GRAB_COLUMNS: &str = "id, media_id, target, plan_id, release_ref, info_hash, title, indexer_id, size, quality, revision_version, revision_real, full_season, category, save_path, seed_ratio, seed_time_minutes, remove_completed, remove_failed, origin, state, active, attempts, next_attempt_at, progress, client_state, client_name, last_error, user_paused, stalled_since, problem_since, absent_since, last_progress, failure, retry, cleanup, cleanup_detail, cleanup_at, imported_files, created_at, updated_at, submitted_at, completed_at, imported_at, cleanup_files";

fn grab_row(r: &rusqlite::Row) -> rusqlite::Result<Grab> {
    let id_of = |s: String| {
        s.parse::<MediaId>().map_err(|_| {
            rusqlite::Error::InvalidColumnType(0, "media id".into(), rusqlite::types::Type::Text)
        })
    };
    Ok(Grab {
        id: r.get(0)?,
        media_id: id_of(r.get(1)?)?,
        target: id_of(r.get(2)?)?,
        plan_id: r.get(3)?,
        release_ref: r.get(4)?,
        info_hash: r.get(5)?,
        title: r.get(6)?,
        indexer_id: r.get(7)?,
        size: r.get(8)?,
        quality: r.get(9)?,
        revision: match (r.get::<_, Option<i64>>(10)?, r.get::<_, Option<i64>>(11)?) {
            (Some(v), real) => Some(Revision {
                version: u8::try_from(v).unwrap_or(1),
                real: u8::try_from(real.unwrap_or(0)).unwrap_or(0),
            }),
            _ => None,
        },
        full_season: r.get(12)?,
        category: r.get(13)?,
        save_path: r.get(14)?,
        seed_ratio: r.get(15)?,
        seed_time_minutes: r.get(16)?,
        remove_completed: r.get(17)?,
        remove_failed: r.get(18)?,
        origin: r.get(19)?,
        state: r.get(20)?,
        active: r.get(21)?,
        attempts: r.get(22)?,
        next_attempt_at: r.get(23)?,
        progress: r.get(24)?,
        client_state: r.get(25)?,
        client_name: r.get(26)?,
        last_error: r.get(27)?,
        user_paused: r.get(28)?,
        stalled_since: r.get(29)?,
        problem_since: r.get(30)?,
        absent_since: r.get(31)?,
        last_progress: r.get(32)?,
        failure: r.get(33)?,
        retry: r.get(34)?,
        cleanup: r.get(35)?,
        cleanup_detail: r.get(36)?,
        cleanup_at: r.get(37)?,
        imported_files: r.get(38)?,
        created_at: r.get(39)?,
        updated_at: r.get(40)?,
        submitted_at: r.get(41)?,
        completed_at: r.get(42)?,
        imported_at: r.get(43)?,
        cleanup_files: r.get(44)?,
        targets: vec![],
    })
}

fn with_targets(c: &Connection, mut g: Grab) -> Result<Grab> {
    let mut stmt =
        c.prepare("SELECT target, replaces FROM grab_targets WHERE grab_id = ?1 ORDER BY target")?;
    g.targets = stmt
        .query_map([&g.id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(t, replaces)| t.parse().ok().map(|target| GrabTarget { target, replaces }))
        .collect();
    Ok(g)
}

pub fn get(c: &Connection, id: &str) -> Result<Option<Grab>> {
    let g = c
        .query_row(
            &format!("SELECT {GRAB_COLUMNS} FROM grabs WHERE id = ?1"),
            [id],
            grab_row,
        )
        .optional()?;
    g.map(|g| with_targets(c, g)).transpose()
}

/// Grabs matching an SQL condition (trusted, static fragments only).
pub fn select(c: &Connection, condition: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Grab>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {GRAB_COLUMNS} FROM grabs WHERE {condition}"
    ))?;
    let rows: Vec<Grab> = stmt
        .query_map(args, grab_row)?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter().map(|g| with_targets(c, g)).collect()
}

fn payload(c: &Connection, id: &str) -> Result<Payload> {
    let (kind, bytes): (String, Vec<u8>) = c.query_row(
        "SELECT payload_kind, payload FROM grabs WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(match kind.as_str() {
        "magnet" => {
            Payload::Magnet(String::from_utf8(bytes).context("stored magnet is not UTF-8")?)
        }
        _ => Payload::Torrent(bytes),
    })
}

/// User-facing status combining the grab state, the client state and cleanup.
pub fn status(g: &Grab) -> &'static str {
    match g.state.as_str() {
        "queued" => "pending_submission",
        "unconfirmed" => "submitting",
        "downloading" if g.user_paused => "paused",
        "downloading" => match g.client_state.as_deref() {
            Some("stalled") => "stalled",
            Some("fetching_metadata") => "fetching_metadata",
            Some("error") => "client_error",
            Some("missing_files") => "missing_files",
            Some("queued") => "queued_in_client",
            Some("checking") => "checking",
            Some("paused") => "paused",
            Some("moving") => "moving",
            _ => "downloading",
        },
        "completed" => "import_pending",
        "importing" => "importing",
        "import_blocked" => "import_blocked",
        "imported" | "failed" => match g.cleanup.as_str() {
            "seeding" => "seeding",
            "pending" | "removing" => "cleanup_pending",
            "blocked" => "cleanup_blocked",
            "cleaned" => "cleaned",
            _ if g.state == "failed" => "failed",
            _ => "imported",
        },
        "cancelled" => "cancelled",
        "released" => "released",
        _ => "unknown",
    }
}

pub fn view(g: &Grab) -> serde_json::Value {
    json!({
        "id": g.id,
        "media_id": g.media_id,
        "target": g.target,
        "plan_id": g.plan_id,
        "release_ref": g.release_ref,
        "title": g.title,
        "client_name": g.client_name,
        "size_bytes": g.size,
        "quality": g.quality,
        "revision": g.revision,
        "full_season": g.full_season,
        "origin": g.origin,
        "state": g.state,
        "status": status(g),
        "active": g.active,
        "progress": g.progress,
        "client_state": g.client_state,
        "user_paused": g.user_paused,
        "category": g.category,
        "attempts": g.attempts,
        "next_attempt_at": g.active.then_some(&g.next_attempt_at),
        "last_error": g.last_error,
        "failure": g.failure,
        "retry": g.retry,
        "seed_goal": { "ratio": g.seed_ratio, "seeding_time_minutes": g.seed_time_minutes },
        "cleanup": {
            "state": g.cleanup,
            "detail": g.cleanup_detail,
            "at": g.cleanup_at,
            "policy": { "remove_completed": g.remove_completed, "remove_failed": g.remove_failed },
        },
        "imported_files": g.imported_files,
        "targets": g.targets,
        "created_at": g.created_at,
        "updated_at": g.updated_at,
        "submitted_at": g.submitted_at,
        "completed_at": g.completed_at,
        "imported_at": g.imported_at,
    })
}

// ------------------------------------------------------------------ enqueue

#[derive(Debug)]
pub enum EnqueueError {
    /// Code (machine-readable) and message.
    Refused(&'static str, String),
    Upstream(crate::upstream::UpstreamError),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for EnqueueError {
    fn from(e: anyhow::Error) -> Self {
        EnqueueError::Internal(e)
    }
}

fn refused(code: &'static str, msg: impl Into<String>) -> EnqueueError {
    EnqueueError::Refused(code, msg.into())
}

pub fn media_kind(id: MediaId) -> MediaKind {
    match id {
        MediaId::Movie { .. } => MediaKind::Movie,
        _ => MediaKind::Series,
    }
}

/// Client parameters from the imported download client (with config overrides).
pub struct ClientPolicy {
    pub category: String,
    pub paused: bool,
    pub force_start: bool,
    pub sequential: bool,
    pub first_last: bool,
    pub content_layout: Option<&'static str>,
    pub remove_completed: bool,
    pub remove_failed: bool,
    pub post_import_category: Option<String>,
}

pub fn client_policy(
    st: &AppState,
    settings: &Settings,
    media: MediaKind,
) -> Result<ClientPolicy, String> {
    let (movie_category, tv_category, key) = match (&st.cfg.qbittorrent, &st.cfg.embedded) {
        (Some(q), _) => (&q.movie_category, &q.tv_category, "qbittorrent"),
        (None, Some(e)) => (&e.movie_category, &e.tv_category, "embedded"),
        (None, None) => {
            return Err("no download client ([qbittorrent] or [embedded]) is configured".into());
        }
    };
    let embedded = key == "embedded";
    let dc = settings.download_client(media).ok_or_else(|| {
        format!(
            "the imported {} configuration has no enabled torrent download client",
            media.native_app()
        )
    })?;
    // An imported qBittorrent client's policy, or managed setup's own download policy.
    if !dc.implementation.eq_ignore_ascii_case("QBittorrent")
        && dc.implementation != crate::setup::MANAGED_CLIENT
    {
        return Err(format!(
            "the imported download client is {}, not qBittorrent",
            dc.implementation
        ));
    }
    let override_cat = match media {
        MediaKind::Movie => movie_category.clone(),
        MediaKind::Series => tv_category.clone(),
    };
    let category = override_cat
        .or_else(|| dc.category.clone().filter(|c| !c.trim().is_empty()))
        .ok_or_else(|| {
            format!("the download client has no category; set {key}.movie_category / tv_category")
        })?;
    let content_layout = match dc.content_layout {
        None | Some(0) => None,
        Some(1) => Some("Original"),
        Some(2) if !embedded => Some("Subfolder"),
        Some(2) => {
            return Err(
                "the imported client uses the Subfolder content layout, which the embedded engine does not implement (it lays files out as the Original layout)".into(),
            );
        }
        Some(other) => {
            return Err(format!(
                "download client content layout {other} is not supported"
            ));
        }
    };
    if embedded && dc.sequential_order == Some(true) {
        return Err("the imported client downloads in sequential order, which the embedded engine does not implement".into());
    }
    if embedded && dc.first_and_last == Some(true) {
        return Err("the imported client prioritises first and last pieces, which the embedded engine does not implement".into());
    }
    let (paused, force_start) = match dc.initial_state {
        None | Some(0) => (false, false),
        Some(1) => (false, true),
        Some(2) => (true, false),
        Some(other) => {
            return Err(format!(
                "download client initial state {other} is not supported"
            ));
        }
    };
    let cleanup = st.cfg.download_cleanup;
    Ok(ClientPolicy {
        category,
        paused,
        force_start,
        sequential: dc.sequential_order.unwrap_or(false),
        first_last: dc.first_and_last.unwrap_or(false),
        content_layout,
        remove_completed: cleanup && dc.remove_completed_downloads == Some(true),
        remove_failed: cleanup && dc.remove_failed_downloads == Some(true),
        // Coexistence: the native post-import category belongs to the native app; our
        // downloads stay in their dedicated category.
        post_import_category: dc
            .post_import_category
            .clone()
            .filter(|c| !c.trim().is_empty())
            .filter(|_| st.cfg.migration == MigrationMode::Legacy),
    })
}

/// Coexistence mode: the effective qBittorrent category must not be any category an enabled
/// native torrent client (Radarr or Sonarr) uses, including post-import categories. A shared
/// category would let the native app's completed-download handling act on our downloads.
fn shared_category(settings: &Settings, media: MediaKind, category: &str) -> Option<String> {
    let key = match media {
        MediaKind::Movie => "movie_category",
        MediaKind::Series => "tv_category",
    };
    settings
        .download_clients
        .iter()
        .filter(|c| c.enabled && c.protocol.eq_ignore_ascii_case("torrent"))
        // Only native clients compete; managed setup's own policy is this server's.
        .filter(|c| matches!(c.native.app.as_str(), "radarr" | "sonarr"))
        .flat_map(|c| {
            [c.category.as_deref(), c.post_import_category.as_deref()]
                .into_iter()
                .flatten()
                .map(move |cat| (c.media, cat))
        })
        .find(|(_, cat)| cat.trim().eq_ignore_ascii_case(category.trim()))
        .map(|(native, cat)| {
            format!(
                "the qBittorrent category '{cat}' is also used by the native {} download client; coexistence mode needs a dedicated category: set [qbittorrent] {key} to a category no native client uses",
                match native {
                    MediaKind::Movie => "Radarr",
                    MediaKind::Series => "Sonarr",
                }
            )
        })
}

/// Coexistence mode: why this server may not acquire these targets, which stay with the
/// native app until they are adopted (`POST /api/v1/library` with `native_handoff_confirmed`)
/// and monitored here. `None` in legacy mode, or when every target is adopted and monitored.
/// This is the one acquisition gate: every grab goes through `enqueue`, whatever started it
/// (a plan, an interactive grab, RSS, a worker search or a request retry).
pub fn not_adopted(st: &AppState, targets: &[MediaId]) -> Option<String> {
    if st.cfg.migration != MigrationMode::Coexistence {
        return None;
    }
    let index = st.index();
    targets.iter().find_map(|t| not_adopted_one(&index, *t))
}

fn not_adopted_one(index: &crate::catalog::CatalogIndex, target: MediaId) -> Option<String> {
    let hint = "adopt it with POST /api/v1/library (native_handoff_confirmed, after suspending its native monitoring and confirming no native download is active)";
    let series = |k| index.series(k);
    let state = |a: &Option<String>| a.clone().unwrap_or_else(|| "unknown".into());
    match target {
        MediaId::Movie { tmdb } => match index.movie(tmdb) {
            Some(m) if m.monitored => None,
            Some(m) => Some(format!(
                "{target} is not adopted and monitored here ({}); Radarr keeps it until you {hint}",
                state(&m.acquisition)
            )),
            None => Some(format!(
                "{target} is not in the managed catalog; add it with POST /api/v1/library"
            )),
        },
        MediaId::Series(k) | MediaId::Season(k, _) | MediaId::Episode(k, _, _) => {
            let Some(s) = series(k) else {
                return Some(format!(
                    "{target} is not in the managed catalog; add it with POST /api/v1/library"
                ));
            };
            if !s.monitored {
                return Some(format!(
                    "{target} is not adopted and monitored here ({}); Sonarr keeps it until you {hint}",
                    state(&s.acquisition)
                ));
            }
            let unmonitored: Vec<String> = s
                .episodes
                .iter()
                .flatten()
                .filter(|e| e.season > 0 && !e.monitored)
                .filter(|e| match target {
                    MediaId::Season(_, n) => e.season == n,
                    MediaId::Episode(_, n, x) => e.season == n && e.number == x,
                    _ => true,
                })
                .map(|e| format!("S{:02}E{:02}", e.season, e.number))
                .collect();
            let season_flag = match target {
                MediaId::Season(_, n) if s.episodes.is_none() => {
                    s.seasons.iter().any(|x| x.number == n && x.monitored)
                }
                _ => true,
            };
            if unmonitored.is_empty() && season_flag {
                None
            } else {
                Some(format!(
                    "{target} includes episodes that are not adopted here ({}); adopt the scope first: {hint}",
                    if unmonitored.is_empty() {
                        "the season".to_string()
                    } else {
                        unmonitored
                            .into_iter()
                            .take(10)
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ))
            }
        }
    }
}

/// Whether execution is possible for a media kind; the reason when not.
pub fn execution_blocker(st: &AppState, media: MediaKind) -> Option<(&'static str, String)> {
    if !st.cfg.execution_enabled {
        return Some((
            "execution_disabled",
            "execution is disabled ([execution] enabled = false); plans are shadow-only".into(),
        ));
    }
    let Some(l) = st.current() else {
        return Some((
            "catalog_not_imported",
            "no configuration import is active".into(),
        ));
    };
    if st.client.is_none() {
        return Some((
            "client_not_configured",
            "no download client ([qbittorrent] or [embedded]) is configured".into(),
        ));
    }
    if st
        .cfg
        .library
        .as_ref()
        .and_then(|lib| crate::ingest::root_for(lib, media))
        .is_none()
    {
        return Some((
            "client_not_configured",
            format!("no [library] root is configured for {}", media.native_app()),
        ));
    }
    if l.settings.media_management(media).is_none() {
        return Some((
            "import_unsupported",
            format!(
                "the active import has no {} media-management settings (made by an earlier build); re-import the snapshot to enable file import",
                media.native_app()
            ),
        ));
    }
    let blockers = l.settings.import_blockers(media);
    if !blockers.is_empty() {
        let codes: Vec<&str> = blockers.iter().map(|b| b.code.as_str()).collect();
        return Some((
            "import_unsupported",
            format!(
                "imported {} settings block file import ({}); nothing is downloaded that could not be imported",
                media.native_app(),
                codes.join(", ")
            ),
        ));
    }
    let policy = match client_policy(st, &l.settings, media) {
        Ok(p) => p,
        Err(e) => return Some(("client_not_configured", e)),
    };
    // Only a shared client exposes our downloads to the native apps' completed-download
    // handling; the embedded engine is private to this process.
    if st.cfg.migration == MigrationMode::Coexistence
        && st.cfg.qbittorrent.is_some()
        && let Some(why) = shared_category(&l.settings, media, &policy.category)
    {
        return Some(("category_shared", why));
    }
    None
}

/// What one link field holds, judged by its content rather than the field it came in.
#[derive(Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// A magnet URI with a valid BitTorrent info-hash: used as-is, never fetched.
    Magnet,
    /// An HTTP(S) URL: only ever fetched through `ProwlarrClient::fetch_release`, which
    /// accepts nothing but the configured origin's download proxy paths.
    Http,
    /// Anything else (another scheme, an invalid magnet, not a URL). The reason never quotes
    /// the link.
    Unusable(&'static str),
}

pub fn link_kind(raw: &str) -> LinkKind {
    let raw = raw.trim();
    if raw.len() > 16 * 1024 {
        return LinkKind::Unusable("the link is too long");
    }
    if raw
        .get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("magnet:"))
    {
        return match crate::torrent::magnet_hash(raw) {
            Ok(_) => LinkKind::Magnet,
            Err(_) => LinkKind::Unusable("not a valid magnet URI with a BitTorrent info-hash"),
        };
    }
    match reqwest::Url::parse(raw) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => LinkKind::Http,
        Ok(_) => LinkKind::Unusable("unsupported link scheme"),
        Err(_) => LinkKind::Unusable("not a URL"),
    }
}

/// Resolves a release's payload from its link fields. Prowlarr's search API may put its HTTP
/// download proxy (which serves a `.torrent` or redirects to a magnet) in `magnetUrl`, with
/// or without a `downloadUrl`, so each field is classified by content (`link_kind`):
///
/// 1. a literal magnet URI (from `magnetUrl`, else `downloadUrl`) is used without a fetch
///    (native The Pirate Bay releases are always magnets built from their info-hash);
/// 2. otherwise each distinct HTTP link, `downloadUrl` first and then an HTTP `magnetUrl`,
///    is fetched through the backend that produced the release: the configured Prowlarr's
///    download proxy (`fetch_release`), or the native indexer with the release's id, which
///    accepts only its own origin and explicitly configured download origins (bounded
///    size, at most one checked redirect or a redirect to a valid magnet, validated
///    `.torrent`, never its API key on another origin). The first usable payload wins.
///
/// An unusable field never hides a usable one. Nothing is rebuilt from the advertised
/// info-hash. When every fetch failed, a transient error is returned in preference (so
/// callers defer instead of rejecting), else the first error; when nothing could even be
/// tried the release is refused. Errors never quote a link.
async fn resolve_payload(st: &AppState, link: &ReleaseLink) -> Result<Payload, EnqueueError> {
    let fields = [
        ("magnetUrl", link.magnet.as_deref()),
        ("downloadUrl", link.download_url.as_deref()),
    ];
    let mut problems: Vec<String> = vec![];
    let mut http: Vec<&str> = vec![];
    for (field, raw) in fields {
        let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
            continue;
        };
        match link_kind(raw) {
            LinkKind::Magnet => return Ok(Payload::Magnet(raw.to_string())),
            LinkKind::Http => http.push(raw),
            LinkKind::Unusable(why) => problems.push(format!("{field}: {why}")),
        }
    }
    // `downloadUrl` is the canonical link; an HTTP `magnetUrl` is tried after it.
    http.reverse();
    http.dedup();
    if http.is_empty() {
        return Err(if problems.is_empty() {
            refused("no_download_link", "the release has no download link")
        } else {
            refused(
                "invalid_payload",
                format!("no usable download link ({})", problems.join("; ")),
            )
        });
    }
    let backend = st.search.as_ref().ok_or_else(|| {
        refused(
            "provider_not_configured",
            "no release search backend ([[indexer]] or [prowlarr]) is configured",
        )
    })?;
    let mut errors = vec![];
    for url in http {
        match backend.fetch(link.indexer_id, url).await {
            Ok(payload) => return Ok(payload),
            Err(e) => errors.push(e),
        }
    }
    let transient = errors.iter().position(UpstreamError::transient);
    Err(EnqueueError::Upstream(
        errors.swap_remove(transient.unwrap_or(0)),
    ))
}

/// With the embedded engine: why `payload` could not find any peer (`None` when it can).
/// Peers come from the payload's trackers (when `[embedded] trackers` is on), the DHT (when
/// on, and never for a torrent marked private) or the configured initial peers. A payload
/// with none of these would sit in the queue forever, so it is refused instead.
pub fn peer_discovery_problem(st: &AppState, payload: &Payload) -> Option<String> {
    let e = st
        .cfg
        .embedded
        .as_ref()
        .filter(|_| st.cfg.qbittorrent.is_none())?;
    if !e.initial_peers.is_empty() {
        return None;
    }
    let (trackers, private) = match payload {
        Payload::Magnet(m) => (
            reqwest::Url::parse(m)
                .map(|u| u.query_pairs().filter(|(k, _)| k == "tr").count())
                .unwrap_or(0),
            false,
        ),
        Payload::Torrent(t) => {
            let private = crate::torrent::parse(t).ok().is_some_and(|d| {
                d.get("info")
                    .and_then(|(i, _)| i.get("private"))
                    .is_some_and(|(v, _)| matches!(v, crate::torrent::Bencode::Int(1)))
            });
            (crate::torznab::torrent_urls(t).len(), private)
        }
    };
    let via_trackers = e.trackers && trackers > 0;
    let via_dht = e.dht && !private;
    if via_trackers || via_dht {
        return None;
    }
    Some(match (trackers, private) {
        (0, false) => "the payload names no tracker and the embedded engine has DHT off and no initial peers: it could never find a peer. Enable [embedded] dht (public torrents), add trusted magnet_trackers, or configure initial_peers".into(),
        (0, true) => "the torrent is private and names no tracker, and no initial peers are configured: it could never find a peer".into(),
        (_, true) => "the torrent is private (no DHT) and [embedded] trackers is off, with no initial peers: it could never find a peer".into(),
        (_, false) => "[embedded] trackers is off and DHT is off, with no initial peers: the payload could never find a peer".into(),
    })
}

pub struct EnqueueRequest {
    pub link: ReleaseLink,
    pub plan_id: Option<String>,
    pub origin: &'static str,
}

/// Validates, fetches the payload (through the release's own backend), computes identity and
/// inserts the grab atomically with coverage, blocklist and duplicate checks.
pub async fn enqueue(st: &AppState, req: EnqueueRequest) -> Result<Grab, EnqueueError> {
    let media = media_kind(req.link.media_id);
    if let Some((code, msg)) = execution_blocker(st, media) {
        return Err(refused(code, msg));
    }
    let loaded = st.current().expect("checked");
    let settings = &loaded.settings;
    let policy =
        client_policy(st, settings, media).map_err(|e| refused("client_not_configured", e))?;
    let link = &req.link;
    if let Some(why) = not_adopted(st, &link.covers) {
        return Err(refused("not_adopted", why));
    }
    if let (Some(r), Some(id)) = (
        st.search.as_ref().and_then(|b| b.native()),
        link.indexer_id.and_then(|i| u32::try_from(i).ok()),
    ) {
        r.admit_grab(id)
            .await
            .map_err(|why| refused("indexer_limit_reached", why))?;
    }
    let payload = resolve_payload(st, link).await?;
    if let Some(why) = peer_discovery_problem(st, &payload) {
        return Err(refused("no_peer_discovery", why));
    }
    let hash = match &payload {
        Payload::Magnet(m) => crate::torrent::magnet_hash(m),
        Payload::Torrent(t) => crate::torrent::info_hash(t),
    }
    .map_err(|e| refused("invalid_payload", format!("{e:#}")))?;
    if let Some(h) = link
        .info_hash
        .as_deref()
        .filter(|h| crate::torrent::is_info_hash(h))
        && h != hash
    {
        return Err(refused(
            "invalid_payload",
            "the downloaded torrent's info-hash does not match the search result",
        ));
    }
    // The payload's own names must not contradict the item, whatever the search title said.
    if let Some(why) = payload_contradiction(st, link.media_id, &payload) {
        let (media_s, release_ref, title, h) = (
            link.media_id.to_string(),
            link.release_ref.clone(),
            link.title.clone(),
            hash.clone(),
        );
        let now_s = ts(OffsetDateTime::now_utc());
        let why2 = why.clone();
        st.store
            .with(move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO blocklist (media_id, release_ref, info_hash, title, reason, grab_id, created_at) VALUES (?1, ?2, ?3, ?4, 'wrong_payload', NULL, ?5)",
                    params![media_s, release_ref, h, title, now_s],
                )?;
                library::event(c, &now_s, "release_rejected", Some(&media_s), None, None, &format!("{title}: {why2}"), Some(&json!({ "reason": "wrong_payload" })))?;
                Ok(())
            })
            .await?;
        return Err(refused(
            "wrong_payload",
            format!("{why}; the release is blocklisted"),
        ));
    }
    let native = st.search.as_ref().is_some_and(|b| b.native().is_some());
    let (seed_ratio, seed_time) = link
        .indexer_id
        .and_then(|i| u32::try_from(i).ok())
        .and_then(|i| settings.indexer(media, i))
        .map(|ix| {
            let (ratio, time, pack) = crate::indexer::seed_goals(ix, native);
            let time = if link.full_season {
                pack.or(time)
            } else {
                time
            };
            (ratio.filter(|r| *r >= 0.0), time.filter(|t| *t >= 0))
        })
        .unwrap_or((None, None));
    let save_path = st.client.as_ref().expect("checked").save_path();
    let now = OffsetDateTime::now_utc();
    let id = format!(
        "grb_{}",
        crate::fsutil::random_hex(12).map_err(EnqueueError::Internal)?
    );
    let (kind, bytes) = match &payload {
        Payload::Magnet(m) => ("magnet", m.as_bytes().to_vec()),
        Payload::Torrent(t) => ("torrent", t.clone()),
    };
    let link = link.clone();
    let plan_id = req.plan_id.clone();
    let origin = req.origin;
    let id2 = id.clone();
    let category = policy.category.clone();
    let coexistence = st.cfg.migration == MigrationMode::Coexistence;
    let result = st
        .store
        .with(move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // The adoption check above ran before the payload fetch. Repeat it on the stored
            // rows in this transaction, which a release cannot interleave with: either the
            // release committed first and nothing is recorded, or this grab is recorded first
            // and the release is refused as an active download.
            if coexistence
                && let Some(why) = library::not_adopted_now(&tx, link.media_id, &link.covers)?
            {
                return Ok(Err(("not_adopted", why)));
            }
            for t in &link.covers {
                let busy: Option<String> = tx
                    .query_row(
                        "SELECT g.id FROM grab_targets t JOIN grabs g ON g.id = t.grab_id WHERE g.active = 1 AND t.target = ?1",
                        [t.to_string()],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(g) = busy {
                    return Ok(Err(("covers_queued", format!("{t} is already being downloaded by {g}"))));
                }
            }
            let blocked: Option<i64> = tx
                .query_row(
                    "SELECT id FROM blocklist WHERE media_id = ?1 AND (release_ref = ?2 OR info_hash = ?3)",
                    params![link.media_id.to_string(), link.release_ref, hash],
                    |r| r.get(0),
                )
                .optional()?;
            if blocked.is_some() {
                return Ok(Err(("blocklisted", "the release is on the blocklist for this item".to_string())));
            }
            let dup: Option<String> = tx
                .query_row("SELECT id FROM grabs WHERE active = 1 AND info_hash = ?1", [&hash], |r| r.get(0))
                .optional()?;
            if let Some(g) = dup {
                return Ok(Err(("duplicate_torrent", format!("the torrent is already tracked by {g}"))));
            }
            let now_s = ts(now);
            tx.execute(
                "INSERT INTO grabs (id, media_id, target, plan_id, release_ref, info_hash, title, indexer_id, size, quality, revision_version, revision_real, full_season, category, save_path, seed_ratio, seed_time_minutes, remove_completed, remove_failed, origin, payload_kind, payload, state, active, attempts, next_attempt_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, 'queued', 1, 0, ?23, ?23, ?23)",
                params![
                    id2, link.media_id.to_string(), link.target.to_string(), plan_id, link.release_ref, hash, link.title, link.indexer_id, link.size,
                    link.quality.as_str(), i64::from(link.revision.version), i64::from(link.revision.real), link.full_season, category, save_path,
                    seed_ratio, seed_time, policy.remove_completed, policy.remove_failed, origin, kind, bytes, now_s
                ],
            )?;
            for t in &link.covers {
                tx.execute(
                    "INSERT INTO grab_targets (grab_id, target, replaces) VALUES (?1, ?2, ?3)",
                    params![id2, t.to_string(), link.replaces.contains(t)],
                )?;
            }
            library::event(
                &tx,
                &now_s,
                "grabbed",
                Some(&link.media_id.to_string()),
                Some(&link.target.to_string()),
                Some(&id2),
                &format!("grabbed {} ({origin})", link.title),
                Some(&json!({ "release_ref": link.release_ref, "covers": link.covers, "replaces": link.replaces, "quality": link.quality.as_str(), "revision": link.revision })),
            )?;
            tx.commit()?;
            Ok(Ok(()))
        })
        .await?;
    if let Err((code, msg)) = result {
        return Err(refused(code, msg));
    }
    st.reload_index().await?;
    st.worker.notify.notify_one();
    let id3 = id.clone();
    Ok(st
        .store
        .with(move |c| get(c, &id3))
        .await?
        .expect("just inserted"))
}

/// Identity of a managed item (movie or series) from the current index.
pub fn identity_of(st: &AppState, media: MediaId) -> Option<crate::identity::Identity> {
    let index = st.index();
    match media {
        MediaId::Movie { tmdb } => index.movie(tmdb).map(crate::identity::Identity::movie),
        other => other
            .series_key()
            .and_then(|k| index.series(k))
            .map(crate::identity::Identity::series),
    }
}

/// A contradiction between the payload's own names (torrent name and video files, or a
/// magnet's display name) and the item.
fn payload_contradiction(st: &AppState, media: MediaId, payload: &Payload) -> Option<String> {
    let id = identity_of(st, media)?;
    match payload {
        Payload::Torrent(t) => {
            let (name, files) = crate::torrent::names(t).ok()?;
            if let Some(why) = id.contradiction([name.as_str()]) {
                return Some(why);
            }
            files
                .iter()
                .filter(|f| {
                    f.rsplit_once('.').is_some_and(|(_, e)| {
                        crate::ingest::VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str())
                    })
                })
                .find_map(|f| id.contradiction(f.split('/')))
        }
        Payload::Magnet(m) => {
            let url = reqwest::Url::parse(m).ok()?;
            let dn = url
                .query_pairs()
                .find(|(k, _)| k == "dn")
                .map(|(_, v)| v.into_owned())?;
            id.contradiction([dn.as_str()])
        }
    }
}

// ------------------------------------------------------------------ submission

/// How messages name the configured download client.
fn client_name(st: &AppState) -> &'static str {
    match st.client.as_deref() {
        Some(c) if c.embedded_semantics() => "the embedded engine",
        _ => "qBittorrent",
    }
}

fn ours(t: &TorrentInfo, g: &Grab, post_import: Option<&str>) -> bool {
    (t.category == g.category || post_import.is_some_and(|p| t.category == p)) && t.has_tag(TAG)
}

async fn update(
    st: &AppState,
    id: &str,
    sql: &'static str,
    args: Vec<rusqlite::types::Value>,
) -> Result<()> {
    let id = id.to_string();
    st.store
        .with(move |c| {
            let mut all: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Text(id)];
            all.extend(args);
            c.execute(sql, rusqlite::params_from_iter(all))?;
            Ok(())
        })
        .await
}

fn text(s: impl Into<String>) -> rusqlite::types::Value {
    rusqlite::types::Value::Text(s.into())
}

fn opt_text(s: Option<String>) -> rusqlite::types::Value {
    s.map(rusqlite::types::Value::Text)
        .unwrap_or(rusqlite::types::Value::Null)
}

fn backoff(attempts: i64) -> Duration {
    Duration::from_secs((30u64 << attempts.clamp(0, 8) as u32).min(3600))
}

async fn record_event(
    st: &AppState,
    g: &Grab,
    kind: &'static str,
    message: String,
    data: Option<serde_json::Value>,
) -> Result<()> {
    let (media, target, id) = (g.media_id.to_string(), g.target.to_string(), g.id.clone());
    let now = ts(OffsetDateTime::now_utc());
    st.store
        .with(move |c| {
            library::event(
                c,
                &now,
                kind,
                Some(&media),
                Some(&target),
                Some(&id),
                &message,
                data.as_ref(),
            )
        })
        .await
}

/// Submits due grabs (`queued`, or `unconfirmed` awaiting reconciliation). Returns how many
/// were confirmed in the client.
pub async fn submit_due(st: &AppState, now: OffsetDateTime) -> Result<usize> {
    let Some(qb) = st.client.clone() else {
        return Ok(0);
    };
    // The embedded engine has no queue of its own: grabs beyond its active-download limit
    // wait in the outbox (no attempt is counted).
    let cap = st
        .cfg
        .embedded
        .as_ref()
        .filter(|_| qb.embedded().is_some())
        .map(|e| e.max_active_downloads);
    let now_s = ts(now);
    let due = st
        .store
        .with(move |c| select(c, "state IN ('queued', 'unconfirmed') AND next_attempt_at <= ?1 ORDER BY created_at LIMIT 20", &[&now_s]))
        .await?;
    let mut confirmed = 0;
    for g in due {
        let _guard = st.worker.grabs.lock().await;
        // Re-read under the lock: a user action may have changed it.
        let id = g.id.clone();
        let Some(g) = st.store.with(move |c| get(c, &id)).await? else {
            continue;
        };
        if !matches!(g.state.as_str(), "queued" | "unconfirmed") {
            continue;
        }
        if g.state == "queued"
            && let Some(cap) = cap
        {
            let busy: i64 = st
                .store
                .with(|c| {
                    Ok(c.query_row(
                        "SELECT COUNT(*) FROM grabs WHERE active = 1 AND state IN ('downloading', 'unconfirmed')",
                        [],
                        |r| r.get(0),
                    )?)
                })
                .await?;
            if busy as usize >= cap {
                continue;
            }
        }
        if submit_one(st, &qb, &g, now).await? {
            confirmed += 1;
        }
    }
    Ok(confirmed)
}

async fn submit_one(st: &AppState, qb: &Client, g: &Grab, now: OffsetDateTime) -> Result<bool> {
    let name = client_name(st);
    let loaded = st.current();
    let settings = match loaded.as_deref() {
        Some(l) => &l.settings,
        None => return Ok(false),
    };
    let media = media_kind(g.media_id);
    let post = client_policy(st, settings, media)
        .ok()
        .and_then(|p| p.post_import_category);
    // 1. Reconcile by hash before any add.
    let present = match qb.info(std::slice::from_ref(&g.info_hash)).await {
        Ok(list) => list
            .into_iter()
            .find(|t| t.hash.eq_ignore_ascii_case(&g.info_hash)),
        Err(e) => {
            let next = ts(now + backoff(g.attempts));
            update(st, &g.id, "UPDATE grabs SET last_error = ?2, next_attempt_at = ?3, updated_at = ?4 WHERE id = ?1", vec![text(format!("{} {}", qb.label(), e.describe())), text(next), text(ts(now))]).await?;
            return Ok(false);
        }
    };
    if let Some(t) = present {
        if ours(&t, g, post.as_deref()) {
            confirm(st, g, &t, now).await?;
            return Ok(true);
        }
        // Same hash, not ours: never touch or adopt it.
        fail(st, g, "foreign_torrent", &format!("the torrent already exists in {name} under another category or without the unified-media tag; it is left untouched"), false, now).await?;
        return Ok(false);
    }
    if g.attempts >= MAX_SUBMIT_ATTEMPTS {
        fail(
            st,
            g,
            "submission_failed",
            &format!(
                "{name} did not accept the torrent after {} attempts ({})",
                g.attempts,
                g.last_error.as_deref().unwrap_or("no detail")
            ),
            false,
            now,
        )
        .await?;
        return Ok(false);
    }
    // 2. Durable "in flight" marker before the add call.
    update(
        st,
        &g.id,
        "UPDATE grabs SET state = 'unconfirmed', attempts = attempts + 1, next_attempt_at = ?2, updated_at = ?3 WHERE id = ?1",
        vec![text(ts(now + backoff(g.attempts))), text(ts(now))],
    )
    .await?;
    let policy = match client_policy(st, settings, media) {
        Ok(p) => p,
        Err(e) => {
            update(
                st,
                &g.id,
                "UPDATE grabs SET last_error = ?2 WHERE id = ?1",
                vec![text(e)],
            )
            .await?;
            return Ok(false);
        }
    };
    let id = g.id.clone();
    let payload = st.store.with(move |c| payload(c, &id)).await?;
    let req = AddRequest {
        payload,
        save_path: g.save_path.clone(),
        category: g.category.clone(),
        tags: TAG.into(),
        paused: policy.paused,
        ratio_limit: g.seed_ratio,
        seeding_time_limit_minutes: g.seed_time_minutes,
        sequential: policy.sequential,
        first_last_piece: policy.first_last,
        content_layout: policy.content_layout,
    };
    let answer = qb.add(&req).await;
    // 3. Verify by hash whatever the answer was.
    let found = qb
        .info(std::slice::from_ref(&g.info_hash))
        .await
        .ok()
        .and_then(|l| {
            l.into_iter()
                .find(|t| t.hash.eq_ignore_ascii_case(&g.info_hash))
        });
    match (answer, found) {
        (_, Some(t)) if ours(&t, g, post.as_deref()) => {
            if policy.force_start {
                let _ = qb.set_force_start(&g.info_hash).await;
            }
            confirm(st, g, &t, now).await?;
            Ok(true)
        }
        (_, Some(_)) => {
            fail(st, g, "foreign_torrent", "a torrent with this hash appeared under another category or without the unified-media tag; it is left untouched", false, now).await?;
            Ok(false)
        }
        (Ok(true), None) => {
            // Accepted but not visible yet (magnet metadata or a slow client): reconcile later.
            update(
                st,
                &g.id,
                "UPDATE grabs SET last_error = ?3, next_attempt_at = ?2 WHERE id = ?1",
                vec![
                    text(ts(now + Duration::from_secs(15))),
                    text(format!("accepted by {name}; waiting for it to appear")),
                ],
            )
            .await?;
            Ok(false)
        }
        (Ok(false), None) => {
            update(
                st,
                &g.id,
                "UPDATE grabs SET last_error = 'qBittorrent answered Fails.' WHERE id = ?1",
                vec![],
            )
            .await?;
            Ok(false)
        }
        (Err(e), None) => {
            let invalid = matches!(&e, crate::upstream::UpstreamError::BadResponse { reason } if reason.contains("invalid"));
            if invalid {
                fail(
                    st,
                    g,
                    "invalid_torrent",
                    &format!("{} {}", qb.label(), e.describe()),
                    true,
                    now,
                )
                .await?;
            } else {
                update(
                    st,
                    &g.id,
                    "UPDATE grabs SET last_error = ?2 WHERE id = ?1",
                    vec![text(format!(
                        "add outcome unknown: {} {}; reconciling by hash",
                        qb.label(),
                        e.describe()
                    ))],
                )
                .await?;
            }
            Ok(false)
        }
    }
}

async fn confirm(st: &AppState, g: &Grab, t: &TorrentInfo, now: OffsetDateTime) -> Result<()> {
    update(
        st,
        &g.id,
        "UPDATE grabs SET state = 'downloading', submitted_at = COALESCE(submitted_at, ?2), updated_at = ?2, last_error = NULL, client_state = ?3, client_name = ?4, progress = ?5 WHERE id = ?1 AND state IN ('queued', 'unconfirmed')",
        vec![text(ts(now)), text(t.state().as_str()), text(t.name.chars().take(300).collect::<String>()), rusqlite::types::Value::Real(t.progress)],
    )
    .await?;
    record_event(
        st,
        g,
        "download_started",
        format!("{} accepted {}", client_name(st), g.title),
        None,
    )
    .await
}

// ------------------------------------------------------------------ failure

/// Marks a grab failed: inactive (coverage released), optionally blocklisted, the re-search
/// choice recorded from the imported policy, the torrent stopped if ours and incomplete, and
/// cleanup scheduled per `removeFailedDownloads` (never when files were imported).
pub async fn fail(
    st: &AppState,
    g: &Grab,
    code: &str,
    message: &str,
    blocklist: bool,
    now: OffsetDateTime,
) -> Result<()> {
    let media = media_kind(g.media_id);
    let loaded = st.current();
    let policy = loaded.as_deref().and_then(|l| l.settings.policy(media));
    let research = match g.origin.as_str() {
        "interactive" => policy.is_some_and(|p| p.redownload_failed_interactive),
        _ => policy.is_some_and(|p| p.redownload_failed),
    } && code != "foreign_torrent"
        // A payload that was not what it claimed (wrong item or quality) is replaced
        // automatically: the request still stands.
        || matches!(code, "wrong_payload" | "wrong_quality" | "wrong_language");
    let cleanup = if code == "foreign_torrent"
        || code == "submission_failed"
        || g.submitted_at.is_none()
    {
        ("none", None)
    } else if g.imported_files > 0 {
        (
            "kept",
            Some("files from this download were imported; its data is kept".to_string()),
        )
    } else if g.remove_failed {
        (
            "seeding",
            Some(
                "removeFailedDownloads: removal pending the ownership and seeding checks"
                    .to_string(),
            ),
        )
    } else {
        ("disabled", Some("removeFailedDownloads is off (or [execution] download_cleanup = false); the download is kept".to_string()))
    };
    let (id, media_s, target_s, release_ref, hash, title) = (
        g.id.clone(),
        g.media_id.to_string(),
        g.target.to_string(),
        g.release_ref.clone(),
        g.info_hash.clone(),
        g.title.clone(),
    );
    let (code_s, message_s) = (code.to_string(), message.to_string());
    let target = g.target;
    let now_s = ts(now);
    st.store
        .with(move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let n = tx.execute(
                "UPDATE grabs SET state = 'failed', active = 0, failure = ?2, last_error = ?3, retry = ?4, cleanup = ?5, cleanup_detail = ?6, updated_at = ?7, lease_owner = NULL, lease_until = NULL WHERE id = ?1 AND active = 1",
                params![id, code_s, message_s, if research { "research" } else { "none" }, cleanup.0, cleanup.1, now_s],
            )?;
            if n == 0 {
                return Ok(());
            }
            if blocklist {
                tx.execute(
                    "INSERT OR IGNORE INTO blocklist (media_id, release_ref, info_hash, title, reason, grab_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![media_s, release_ref, hash, title, code_s, id, now_s],
                )?;
            }
            library::event(&tx, &now_s, "download_failed", Some(&media_s), Some(&target_s), Some(&id), &format!("{title}: {message_s}"), Some(&json!({ "failure": code_s, "blocklisted": blocklist, "research": research })))?;
            if research {
                crate::worker::queue_command(&tx, "search", Some(target), &now_s)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    if let Some(qb) = &st.client
        && g.submitted_at.is_some()
        && code != "foreign_torrent"
        && let Ok(list) = qb.info(std::slice::from_ref(&g.info_hash)).await
        && let Some(t) = list
            .into_iter()
            .find(|t| t.hash.eq_ignore_ascii_case(&g.info_hash))
        && ours(&t, g, None)
        && t.progress < 1.0
    {
        let _ = qb.pause(&g.info_hash).await;
    }
    st.reload_index().await?;
    st.worker.notify.notify_one();
    Ok(())
}

// ------------------------------------------------------------------ polling

/// Polls the client for every active grab and maps states; completes, fails or releases.
/// A client outage changes nothing (no absence or stall is inferred without an answer), and
/// time without an answer does not count as time without progress.
pub async fn poll(
    st: &AppState,
    now: OffsetDateTime,
    stall_timeout: Duration,
) -> Result<PollReport> {
    let mut report = PollReport::default();
    let Some(qb) = st.client.clone() else {
        return Ok(report);
    };
    let grabs = st
        .store
        .with(|c| select(c, "active = 1 AND state IN ('downloading', 'completed', 'import_blocked') ORDER BY created_at", &[]))
        .await?;
    if grabs.is_empty() {
        return Ok(report);
    }
    let hashes: Vec<String> = grabs.iter().map(|g| g.info_hash.clone()).collect();
    let mut infos: HashMap<String, TorrentInfo> = HashMap::new();
    for chunk in hashes.chunks(50) {
        match qb.info(chunk).await {
            Ok(list) => infos.extend(list.into_iter().map(|t| (t.hash.to_ascii_lowercase(), t))),
            Err(e) => {
                st.worker.client_unanswered();
                report.client_error = Some(format!("{} {}", qb.label(), e.describe()));
                return Ok(report);
            }
        }
    }
    let unobserved = unanswered_time(st, now).await?;
    let post = st.current().map(|l| {
        let m = client_policy(st, &l.settings, MediaKind::Movie)
            .ok()
            .and_then(|p| p.post_import_category);
        let s = client_policy(st, &l.settings, MediaKind::Series)
            .ok()
            .and_then(|p| p.post_import_category);
        (m, s)
    });
    for g in grabs {
        let _guard = st.worker.grabs.lock().await;
        let gid = g.id.clone();
        let Some(g) = st.store.with(move |c| get(c, &gid)).await? else {
            continue;
        };
        if !g.active {
            continue;
        }
        report.polled += 1;
        let post_cat = post
            .as_ref()
            .and_then(|(m, s)| match media_kind(g.media_id) {
                MediaKind::Movie => m.clone(),
                MediaKind::Series => s.clone(),
            });
        let Some(t) = infos.get(&g.info_hash) else {
            match since(g.absent_since.as_deref(), now) {
                None => {
                    update(
                        st,
                        &g.id,
                        "UPDATE grabs SET absent_since = ?2, updated_at = ?2 WHERE id = ?1",
                        vec![text(ts(now))],
                    )
                    .await?;
                }
                Some(d) if d >= ABSENT_GRACE => {
                    fail(
                        st,
                        &g,
                        "removed_from_client",
                        &format!(
                            "the torrent was removed from {} outside arr",
                            client_name(st)
                        ),
                        false,
                        now,
                    )
                    .await?;
                    report.failed += 1;
                }
                Some(_) => {}
            }
            continue;
        };
        if !ours(t, &g, post_cat.as_deref()) {
            release(st, &g, &format!("the torrent's category or unified-media tag was changed in {}; it is no longer managed", client_name(st)), now).await?;
            report.released += 1;
            continue;
        }
        let state = t.state();
        // Progress since the previous poll (the first observation has no baseline).
        let progressed = g.last_progress.is_some_and(|p| t.progress > p + 1e-9);
        let mut stalled_since = g.stalled_since.clone();
        if !unobserved.is_zero() {
            stalled_since = stalled_since
                .as_deref()
                .and_then(parse_ts)
                .map(|t| ts((t + unobserved).min(now)));
        }
        let mut problem_since = g.problem_since.clone();
        match state {
            // No progress while downloading, whatever the label: connected peers that deliver
            // no data (or none that completes a piece) are not progress. Pause, queueing,
            // checking, moving and completion reset the clock below; progress resets it.
            ClientState::Downloading | ClientState::Stalled | ClientState::FetchingMetadata
                if !progressed =>
            {
                stalled_since.get_or_insert_with(|| ts(now));
            }
            ClientState::Error | ClientState::MissingFiles => {
                problem_since.get_or_insert_with(|| ts(now));
            }
            _ => {
                stalled_since = None;
                problem_since = None;
            }
        }
        if progressed {
            stalled_since = None;
        }
        update(
            st,
            &g.id,
            "UPDATE grabs SET progress = ?2, client_state = ?3, client_name = ?4, last_progress = ?2, stalled_since = ?5, problem_since = ?6, absent_since = NULL, updated_at = ?7 WHERE id = ?1",
            vec![
                rusqlite::types::Value::Real(t.progress),
                text(state.as_str()),
                text(t.name.chars().take(300).collect::<String>()),
                opt_text(stalled_since.clone()),
                opt_text(problem_since.clone()),
                text(ts(now)),
            ],
        )
        .await?;
        if g.state == "downloading" {
            if let Some(d) = since(problem_since.as_deref(), now)
                && d >= ERROR_GRACE
            {
                let (code, blocklist) = if state == ClientState::MissingFiles {
                    ("missing_files", false)
                } else {
                    ("client_error", true)
                };
                fail(
                    st,
                    &g,
                    code,
                    &format!(
                        "{} reported {} for {} min",
                        client_name(st),
                        state.as_str(),
                        d.as_secs() / 60
                    ),
                    blocklist,
                    now,
                )
                .await?;
                report.failed += 1;
                continue;
            }
            if let Some(d) = since(stalled_since.as_deref(), now)
                && d >= stall_timeout
                && !g.user_paused
            {
                fail(
                    st,
                    &g,
                    "stalled",
                    &format!(
                        "no download progress for {} min ({})",
                        d.as_secs() / 60,
                        state.as_str()
                    ),
                    true,
                    now,
                )
                .await?;
                report.failed += 1;
                continue;
            }
            if state.complete() && t.progress >= 1.0 && t.amount_left == 0 {
                update(st, &g.id, "UPDATE grabs SET state = 'completed', completed_at = ?2, next_attempt_at = ?2, updated_at = ?2 WHERE id = ?1 AND state = 'downloading'", vec![text(ts(now))]).await?;
                record_event(
                    st,
                    &g,
                    "download_completed",
                    format!("{} finished downloading", g.title),
                    None,
                )
                .await?;
                report.completed += 1;
            }
        }
    }
    st.reload_index().await?;
    Ok(report)
}

/// Records this answered poll. When answers were missing since the previous answered poll
/// (an outage, or this process not running, which for the embedded engine means no engine
/// either), returns how long: running stall clocks move forward by it, so that time never
/// counts as time without progress. Zero otherwise.
async fn unanswered_time(st: &AppState, now: OffsetDateTime) -> Result<Duration> {
    let missed = st.worker.client_answered();
    let at = ts(now);
    let prev = st
        .store
        .with(move |c| {
            let prev: Option<String> = c
                .query_row(
                    "SELECT value FROM meta WHERE key = 'downloads_answered_at'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('downloads_answered_at', ?1) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [at],
            )?;
            Ok(prev)
        })
        .await;
    match prev {
        Ok(prev) if missed => Ok(since(prev.as_deref(), now).unwrap_or(Duration::ZERO)),
        Ok(_) => Ok(Duration::ZERO),
        Err(e) => {
            if missed {
                st.worker.client_unanswered();
            }
            Err(e)
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct PollReport {
    pub polled: usize,
    pub completed: usize,
    pub failed: usize,
    pub released: usize,
    pub client_error: Option<String>,
}

async fn release(st: &AppState, g: &Grab, message: &str, now: OffsetDateTime) -> Result<()> {
    update(
        st,
        &g.id,
        "UPDATE grabs SET state = 'released', active = 0, cleanup = 'kept', cleanup_detail = ?2, updated_at = ?3 WHERE id = ?1",
        vec![text(message), text(ts(now))],
    )
    .await?;
    record_event(st, g, "download_released", message.to_string(), None).await?;
    st.reload_index().await
}

// ------------------------------------------------------------------ user actions

#[derive(Debug)]
pub enum ActionError {
    NotFound,
    Conflict(&'static str, String),
    Upstream(crate::upstream::UpstreamError),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ActionError {
    fn from(e: anyhow::Error) -> Self {
        ActionError::Internal(e)
    }
}

async fn load(st: &AppState, id: &str) -> Result<Grab, ActionError> {
    let id = id.to_string();
    st.store
        .with(move |c| get(c, &id))
        .await?
        .ok_or(ActionError::NotFound)
}

async fn our_torrent(st: &AppState, g: &Grab) -> Result<Option<TorrentInfo>, ActionError> {
    let qb = st.client.as_ref().ok_or_else(|| {
        ActionError::Conflict(
            "client_not_configured",
            "no download client ([qbittorrent] or [embedded]) is configured".into(),
        )
    })?;
    let list = qb
        .info(std::slice::from_ref(&g.info_hash))
        .await
        .map_err(ActionError::Upstream)?;
    match list
        .into_iter()
        .find(|t| t.hash.eq_ignore_ascii_case(&g.info_hash))
    {
        None => Ok(None),
        Some(t) if ours(&t, g, post_category(st, g).as_deref()) => Ok(Some(t)),
        Some(_) => Err(ActionError::Conflict(
            "foreign_torrent",
            format!(
                "the torrent in {} is not owned by arr (category or tag differs); refusing to touch it",
                client_name(st)
            ),
        )),
    }
}

fn post_category(st: &AppState, g: &Grab) -> Option<String> {
    let l = st.current()?;
    client_policy(st, &l.settings, media_kind(g.media_id))
        .ok()?
        .post_import_category
}

pub async fn pause(st: &AppState, id: &str, pause: bool) -> Result<Grab, ActionError> {
    let _guard = st.worker.grabs.lock().await;
    let g = load(st, id).await?;
    if !matches!(
        g.state.as_str(),
        "downloading" | "completed" | "import_blocked"
    ) {
        return Err(ActionError::Conflict(
            "grab_state",
            format!(
                "a {} download cannot be {}",
                g.state,
                if pause { "paused" } else { "resumed" }
            ),
        ));
    }
    let t = our_torrent(st, &g).await?.ok_or_else(|| {
        ActionError::Conflict(
            "grab_state",
            format!("the torrent is not in {}", client_name(st)),
        )
    })?;
    let qb = st.client.as_ref().expect("checked");
    let r = if pause {
        qb.pause(&t.hash).await
    } else {
        qb.resume(&t.hash).await
    };
    r.map_err(ActionError::Upstream)?;
    update(
        st,
        &g.id,
        "UPDATE grabs SET user_paused = ?2, stalled_since = NULL, updated_at = ?3 WHERE id = ?1",
        vec![
            rusqlite::types::Value::Integer(pause.into()),
            text(ts(OffsetDateTime::now_utc())),
        ],
    )
    .await?;
    record_event(
        st,
        &g,
        if pause {
            "download_paused"
        } else {
            "download_resumed"
        },
        format!("{} {}", g.title, if pause { "paused" } else { "resumed" }),
        None,
    )
    .await?;
    load(st, id).await
}

/// What a cancel does with the client side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Removal {
    /// Leave the torrent and its data in qBittorrent (an incomplete one is stopped).
    Keep,
    /// Remove our torrent entry; downloaded data stays on disk.
    Torrent,
    /// Remove our torrent and its tracked download data (never library files).
    TorrentAndData,
}

pub struct CancelOptions {
    pub blocklist: bool,
    pub removal: Removal,
    pub ignore_seed_criteria: bool,
    pub research: bool,
}

pub async fn cancel(st: &AppState, id: &str, o: CancelOptions) -> Result<Grab, ActionError> {
    let _guard = st.worker.grabs.lock().await;
    let g = load(st, id).await?;
    if !g.active && !matches!(g.state.as_str(), "imported" | "failed" | "cancelled") {
        return Err(ActionError::Conflict(
            "grab_state",
            format!(
                "the download is {} ({}); nothing to cancel",
                g.state,
                status(&g)
            ),
        ));
    }
    if g.cleanup == "removing" {
        return Err(ActionError::Conflict(
            "grab_state",
            "removal is already in progress".into(),
        ));
    }
    let now = OffsetDateTime::now_utc();
    let submitted = g.submitted_at.is_some() || g.state == "unconfirmed";
    let torrent = if submitted && st.client.is_some() {
        our_torrent(st, &g).await?
    } else {
        None
    };
    let qb = st.client.clone();
    let cname = client_name(st);
    let mut detail = format!("cancelled; the torrent and its data are left in {cname}");
    match (o.removal, &torrent, &qb) {
        (Removal::Keep, Some(t), Some(qb)) if t.progress < 1.0 => {
            qb.pause(&t.hash).await.map_err(ActionError::Upstream)?;
            detail = format!("cancelled; the incomplete torrent is stopped and left in {cname}");
        }
        (Removal::Torrent, Some(t), Some(qb)) => {
            qb.delete(&t.hash, false)
                .await
                .map_err(ActionError::Upstream)?;
            detail = format!(
                "cancelled; the torrent was removed from {cname} and its data kept on disk"
            );
        }
        (Removal::TorrentAndData, Some(t), Some(_)) => {
            if !o.ignore_seed_criteria && t.progress >= 1.0 {
                let prefs = st.client.as_ref().expect("client").preferences().await.ok();
                let secs = seeding_secs(st, t).await;
                if let Seed::Unmet { reason } | Seed::Unknown { reason } = seed_criteria(
                    t,
                    secs,
                    prefs.as_ref(),
                    g.seed_ratio,
                    g.seed_time_minutes,
                    true,
                ) {
                    return Err(ActionError::Conflict(
                        "seed_criteria_unmet",
                        format!("{reason}; pass ignore_seed_criteria = true to remove it anyway"),
                    ));
                }
            }
            match prepare_removal(st, &g, t).await {
                Ok(files) => {
                    journal_removal(
                        st,
                        &g,
                        &files,
                        "explicit removal requested with cancel",
                        now,
                    )
                    .await?;
                    detail = "cancelled; removing the torrent and its download data".into();
                }
                Err(why) => return Err(ActionError::Conflict("cleanup_blocked", why)),
            }
        }
        _ => {}
    }
    let blocklist = o.blocklist;
    let research = o.research;
    let was_active = g.active;
    let (gid, media_s, target_s, release_ref, hash, title) = (
        g.id.clone(),
        g.media_id.to_string(),
        g.target.to_string(),
        g.release_ref.clone(),
        g.info_hash.clone(),
        g.title.clone(),
    );
    let targets: Vec<MediaId> = g.targets.iter().map(|t| t.target).collect();
    let target = g.target;
    let removal = o.removal;
    let detail2 = detail.clone();
    st.store
        .with(move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let now_s = ts(now);
            if was_active {
                tx.execute(
                    "UPDATE grabs SET state = 'cancelled', active = 0, updated_at = ?2, lease_owner = NULL, lease_until = NULL WHERE id = ?1",
                    params![gid, now_s],
                )?;
            }
            if removal != Removal::TorrentAndData {
                tx.execute("UPDATE grabs SET cleanup = 'kept', cleanup_detail = ?2 WHERE id = ?1 AND cleanup != 'removing'", params![gid, detail2])?;
            }
            if blocklist {
                tx.execute(
                    "INSERT OR IGNORE INTO blocklist (media_id, release_ref, info_hash, title, reason, grab_id, created_at) VALUES (?1, ?2, ?3, ?4, 'cancelled', ?5, ?6)",
                    params![media_s, release_ref, hash, title, gid, now_s],
                )?;
            }
            if was_active {
                if research {
                    crate::worker::queue_command(&tx, "search", Some(target), &now_s)?;
                } else {
                    library::hold_search(&tx, &targets, now + CANCEL_COOLDOWN)?;
                }
            }
            library::event(&tx, &now_s, "download_cancelled", Some(&media_s), Some(&target_s), Some(&gid), &format!("{title}: {detail2}"), Some(&json!({ "blocklisted": blocklist, "removal": removal, "research": research })))?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    st.reload_index().await?;
    st.worker.notify.notify_one();
    load(st, id).await
}

/// Retry: re-import (`import_blocked`), resubmit now (`queued`/`unconfirmed`), re-evaluate
/// cleanup (`cleanup_blocked`/`client_removed`), or search for an alternative (`failed`).
pub async fn retry(st: &AppState, id: &str) -> Result<(Grab, &'static str), ActionError> {
    let _guard = st.worker.grabs.lock().await;
    let g = load(st, id).await?;
    let now = ts(OffsetDateTime::now_utc());
    let action = match (g.state.as_str(), g.cleanup.as_str()) {
        ("import_blocked", _) => {
            update(st, &g.id, "UPDATE grabs SET state = 'completed', next_attempt_at = ?2, updated_at = ?2 WHERE id = ?1", vec![text(now)]).await?;
            "reimport"
        }
        ("queued" | "unconfirmed", _) => {
            update(
                st,
                &g.id,
                "UPDATE grabs SET next_attempt_at = ?2, updated_at = ?2 WHERE id = ?1",
                vec![text(now)],
            )
            .await?;
            "resubmit"
        }
        (_, "blocked") => {
            update(st, &g.id, "UPDATE grabs SET cleanup = 'seeding', cleanup_detail = 'cleanup re-evaluation requested', updated_at = ?2 WHERE id = ?1", vec![text(now)]).await?;
            "cleanup"
        }
        ("failed" | "cancelled", _) => {
            let target = g.target;
            let now2 = now.clone();
            st.store
                .with(move |c| {
                    crate::worker::queue_command(c, "search", Some(target), &now2)?;
                    Ok(())
                })
                .await?;
            "search"
        }
        _ => {
            return Err(ActionError::Conflict(
                "grab_state",
                format!(
                    "nothing to retry for a {} download ({})",
                    g.state,
                    status(&g)
                ),
            ));
        }
    };
    record_event(
        st,
        &g,
        "download_retry",
        format!("retry requested: {action}"),
        None,
    )
    .await?;
    st.worker.notify.notify_one();
    Ok((load(st, id).await?, action))
}

// ------------------------------------------------------------------ cleanup

/// Seed-criteria verdict for a completed download.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Seed {
    Met { reason: String },
    Unmet { reason: String },
    Unknown { reason: String },
}

fn limit_ratio(t: &TorrentInfo, prefs: Option<&Preferences>) -> Result<Option<f64>, String> {
    match t.ratio_limit {
        Some(l) if l >= 0.0 => Ok(Some(l)),
        Some(l) if (l + 2.0).abs() < 1e-9 => match prefs {
            None => Err("the global share ratio limit could not be read".into()),
            Some(p) => match (p.max_ratio_enabled, p.max_ratio) {
                (Some(false), _) => Ok(None),
                (Some(true), Some(r)) if r >= 0.0 => Ok(Some(r)),
                _ => Err("the global share ratio limit is not reported".into()),
            },
        },
        Some(l) if (l + 1.0).abs() < 1e-9 => Ok(None),
        _ => Err("the torrent's ratio limit is not reported".into()),
    }
}

fn limit_minutes(t: &TorrentInfo, prefs: Option<&Preferences>) -> Result<Option<i64>, String> {
    match t.seeding_time_limit {
        Some(l) if l >= 0 => Ok(Some(l)),
        Some(-2) => match prefs {
            None => Err("the global seeding time limit could not be read".into()),
            Some(p) => match (p.max_seeding_time_enabled, p.max_seeding_time) {
                (Some(false), _) => Ok(None),
                (Some(true), Some(m)) if m >= 0 => Ok(Some(m)),
                _ => Err("the global seeding time limit is not reported".into()),
            },
        },
        Some(-1) => Ok(None),
        _ => Err("the torrent's seeding time limit is not reported".into()),
    }
}

/// Whether a completed download may be removed. Requires: complete, stopped by the client
/// (`pausedUP`/`stoppedUP`, unless `require_stopped` is false for an explicit removal), a
/// share limit that applies and is met, and the indexer seed goal recorded at grab time met.
/// A stopped state alone is never enough: a manually paused torrent may still owe seeding.
pub fn seed_criteria(
    t: &TorrentInfo,
    seeding_secs: Option<i64>,
    prefs: Option<&Preferences>,
    goal_ratio: Option<f64>,
    goal_minutes: Option<i64>,
    explicit: bool,
) -> Seed {
    if t.progress < 1.0 || t.amount_left > 0 {
        return Seed::Unmet {
            reason: "the download is incomplete".into(),
        };
    }
    let state = t.state();
    if !explicit && state != ClientState::CompletedPaused {
        return Seed::Unmet {
            reason: if state == ClientState::Seeding {
                "still seeding (the client has not stopped it at a share limit)".into()
            } else {
                format!("client state is {}", state.as_str())
            },
        };
    }
    let Some(ratio) = t.ratio else {
        return Seed::Unknown {
            reason: "the share ratio is not reported".into(),
        };
    };
    let (lr, lm) = match (limit_ratio(t, prefs), limit_minutes(t, prefs)) {
        (Ok(r), Ok(m)) => (r, m),
        (Err(e), _) | (_, Err(e)) => return Seed::Unknown { reason: e },
    };
    let time_met = |m: i64| seeding_secs.map(|s| s >= m.saturating_mul(60));
    let ratio_met = lr.map(|l| ratio >= l);
    let minutes_met = lm.and_then(time_met);
    if !explicit {
        if lr.is_none() && lm.is_none() {
            return Seed::Unmet { reason: "no share limit applies (unlimited ratio and seeding time); the download keeps seeding".into() };
        }
        if ratio_met != Some(true) && minutes_met != Some(true) {
            if lm.is_some() && minutes_met.is_none() && ratio_met != Some(true) {
                return Seed::Unknown {
                    reason: "the seeding time is not reported".into(),
                };
            }
            return Seed::Unmet {
                reason: format!(
                    "share limit not reached (ratio {ratio:.2}{}, seeding {} min{})",
                    lr.map(|l| format!(" of {l:.2}")).unwrap_or_default(),
                    seeding_secs.map(|s| s / 60).unwrap_or(0),
                    lm.map(|m| format!(" of {m}")).unwrap_or_default()
                ),
            };
        }
    }
    if goal_ratio.is_some() || goal_minutes.is_some() {
        let goal_met = goal_ratio.is_some_and(|r| ratio >= r)
            || goal_minutes.is_some_and(|m| time_met(m) == Some(true));
        if !goal_met {
            return Seed::Unmet {
                reason: format!(
                    "the indexer seed goal is not met (ratio {ratio:.2}{}{})",
                    goal_ratio
                        .map(|r| format!(" of {r:.2}"))
                        .unwrap_or_default(),
                    goal_minutes
                        .map(|m| format!(", seeding {} of {m} min", seeding_secs.unwrap_or(0) / 60))
                        .unwrap_or_default()
                ),
            };
        }
    }
    Seed::Met {
        reason: format!(
            "ratio {ratio:.2}, seeding {} min; share limit and seed goal met",
            seeding_secs.unwrap_or(0) / 60
        ),
    }
}

async fn seeding_secs(st: &AppState, t: &TorrentInfo) -> Option<i64> {
    if t.seeding_time.is_some() {
        return t.seeding_time;
    }
    st.client
        .as_ref()?
        .properties(&t.hash)
        .await
        .ok()
        .flatten()?
        .seeding_time
}

/// A file journaled for removal: local path, size and inode at journal time.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct TrackedFile {
    pub path: PathBuf,
    pub size: u64,
    pub ino: u64,
}

/// Ownership, confinement, library and journal checks before removing a download's data.
/// Returns the existing tracked files.
async fn prepare_removal(
    st: &AppState,
    g: &Grab,
    t: &TorrentInfo,
) -> Result<Vec<TrackedFile>, String> {
    let (save_root, local_root) = st
        .cfg
        .client_paths()
        .ok_or("no download client is configured")?;
    if !ours(t, g, post_category(st, g).as_deref()) {
        return Err("the torrent is not owned by arr (category or tag differs)".into());
    }
    if paths::normalize_client(&t.save_path).as_deref() != Some(g.save_path.trim_end_matches('/')) {
        return Err(format!(
            "the torrent was moved (save path is no longer {}); its data is left alone",
            g.save_path
        ));
    }
    let local_save = paths::client_to_local(save_root, local_root, &t.save_path)
        .ok_or("the torrent's save path is outside the mapped download directory")?;
    // Imported library copies must be committed and present before their source goes.
    let gid = g.id.clone();
    let (pending, committed): (i64, Vec<(String, String, i64)>) = st
        .store
        .with(move |c| {
            let pending: i64 = c.query_row(
                "SELECT COUNT(*) FROM import_journal WHERE grab_id = ?1 AND state NOT IN ('committed', 'backup_removed', 'rolled_back')",
                [&gid],
                |r| r.get(0),
            )?;
            let mut stmt = c.prepare("SELECT root, dest, size FROM import_journal WHERE grab_id = ?1 AND state IN ('committed', 'backup_removed')")?;
            let rows = stmt.query_map([&gid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((pending, rows))
        })
        .await
        .map_err(|e| format!("{e:#}"))?;
    if pending > 0 {
        return Err("an import of this download is not finalised yet".into());
    }
    if g.state == "imported" && committed.is_empty() {
        return Err("no committed library file is recorded for this download".into());
    }
    for (root, dest, size) in &committed {
        let p = Path::new(root).join(dest);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_file() && m.len() == *size as u64 => {}
            Ok(_) => {
                return Err(format!(
                    "the imported library file {dest} changed; refusing to remove its source"
                ));
            }
            Err(_) => {
                return Err(format!(
                    "the imported library file {dest} is missing; refusing to remove its source"
                ));
            }
        }
    }
    let qb = st.client.as_ref().ok_or("no client")?;
    let files = qb
        .files(&t.hash)
        .await
        .map_err(|e| {
            format!(
                "cannot list the torrent's files: {} {}",
                qb.label(),
                e.describe()
            )
        })?
        .ok_or("the torrent disappeared while checking")?;
    let library_roots: Vec<PathBuf> = st
        .cfg
        .library
        .iter()
        .flat_map(|l| l.movies.iter().chain(&l.tv))
        .cloned()
        .collect();
    let mut out = vec![];
    for f in &files {
        let rel = paths::safe_rel(&f.name).ok_or_else(|| {
            format!(
                "unsafe file name in torrent: {:?}",
                f.name.chars().take(80).collect::<String>()
            )
        })?;
        for candidate in [rel.clone(), paths::with_suffix(&rel, ".!qB")] {
            let abs = local_save.join(&candidate);
            if library_roots.iter().any(|r| abs.starts_with(r)) {
                return Err("a torrent file maps into a library root".into());
            }
            match paths::check_no_symlinks(local_root, &abs) {
                Ok(()) => {}
                Err(e) => return Err(format!("{}: {e}", candidate.display())),
            }
            match std::fs::symlink_metadata(&abs) {
                Ok(m) if m.file_type().is_file() => {
                    let ino = crate::fsutil::file_id(&abs)
                        .map_err(|e| format!("{}: {}", candidate.display(), e.kind()))?
                        .1;
                    out.push(TrackedFile {
                        path: abs,
                        size: m.len(),
                        ino,
                    });
                }
                Ok(_) => return Err(format!("{} is not a regular file", candidate.display())),
                Err(_) => {}
            }
        }
    }
    Ok(out)
}

async fn journal_removal(
    st: &AppState,
    g: &Grab,
    files: &[TrackedFile],
    why: &str,
    now: OffsetDateTime,
) -> Result<()> {
    let list = serde_json::to_string(files)?;
    update(
        st,
        &g.id,
        "UPDATE grabs SET cleanup = 'removing', cleanup_files = ?2, cleanup_detail = ?3, cleanup_at = ?4, updated_at = ?4 WHERE id = ?1",
        vec![text(list), text(why), text(ts(now))],
    )
    .await?;
    let qb = st.client.as_ref().context("no client")?;
    if let Err(e) = qb.delete(&g.info_hash, true).await {
        update(
            st,
            &g.id,
            "UPDATE grabs SET cleanup_detail = ?2 WHERE id = ?1",
            vec![text(format!(
                "{why}; removal request failed ({} {}); retrying",
                qb.label(),
                e.describe()
            ))],
        )
        .await?;
    }
    Ok(())
}

/// Finishes a journaled removal once the torrent is gone: deletes remaining tracked files
/// (only if still the same regular file, confined, not symlinked) and empty directories.
fn finish_removal(
    local_root: &Path,
    local_save: &Path,
    files: &[TrackedFile],
) -> Result<usize, String> {
    let mut removed = 0;
    let mut dirs: Vec<PathBuf> = vec![];
    for f in files {
        if !f.path.starts_with(local_save) {
            return Err(format!(
                "{} is outside the download directory",
                f.path.display()
            ));
        }
        paths::check_no_symlinks(local_root, &f.path)?;
        match std::fs::symlink_metadata(&f.path) {
            Ok(m)
                if m.file_type().is_file()
                    && m.len() == f.size
                    && crate::fsutil::file_id(&f.path).is_ok_and(|(_, ino)| ino == f.ino) =>
            {
                std::fs::remove_file(&f.path)
                    .map_err(|e| format!("cannot remove {}: {}", f.path.display(), e.kind()))?;
                removed += 1;
            }
            Ok(_) => {
                return Err(format!(
                    "{} changed after it was journaled; left alone",
                    f.path.display()
                ));
            }
            Err(_) => {}
        }
        let mut d = f.path.parent();
        while let Some(dir) = d {
            if dir == local_save || !dir.starts_with(local_save) {
                break;
            }
            dirs.push(dir.to_path_buf());
            d = dir.parent();
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    for d in dirs {
        let _ = std::fs::remove_dir(&d); // only succeeds when empty
    }
    Ok(removed)
}

#[derive(Debug, Default, Serialize)]
pub struct CleanupReport {
    pub checked: usize,
    pub removed: usize,
    pub seeding: usize,
    pub blocked: usize,
    pub client_error: Option<String>,
}

/// Evaluates every cleanup-tracked download (imported or failed with removal pending, and
/// journaled removals).
pub async fn cleanup(st: &AppState, now: OffsetDateTime) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();
    let (Some(qb), Some((save_root, local_root))) = (st.client.clone(), st.cfg.client_paths())
    else {
        return Ok(report);
    };
    let (save_root, local_root) = (save_root.to_string(), local_root.to_path_buf());
    let cname = client_name(st);
    let grabs = st
        .store
        .with(|c| {
            select(
                c,
                "cleanup IN ('seeding', 'removing') ORDER BY updated_at LIMIT 100",
                &[],
            )
        })
        .await?;
    if grabs.is_empty() {
        return Ok(report);
    }
    let hashes: Vec<String> = grabs.iter().map(|g| g.info_hash.clone()).collect();
    let mut infos: HashMap<String, TorrentInfo> = HashMap::new();
    for chunk in hashes.chunks(50) {
        match qb.info(chunk).await {
            Ok(list) => infos.extend(list.into_iter().map(|t| (t.hash.to_ascii_lowercase(), t))),
            Err(e) => {
                report.client_error = Some(format!("{} {}", qb.label(), e.describe()));
                return Ok(report);
            }
        }
    }
    let prefs = qb.preferences().await.ok();
    for g in grabs {
        let _guard = st.worker.grabs.lock().await;
        let gid = g.id.clone();
        let Some(g) = st.store.with(move |c| get(c, &gid)).await? else {
            continue;
        };
        if g.active && g.cleanup != "removing" {
            continue;
        }
        report.checked += 1;
        let info = infos.get(&g.info_hash);
        match g.cleanup.as_str() {
            "removing" => {
                if let Some(t) = info {
                    if !ours(t, &g, post_category(st, &g).as_deref()) {
                        set_cleanup(st, &g, "blocked", "the torrent's category or tag changed during removal; it is left alone", now).await?;
                        report.blocked += 1;
                        continue;
                    }
                    // Still present: ask again (idempotent), finish on a later pass.
                    if since(g.cleanup_at.as_deref(), now).is_some_and(|d| d >= CLIENT_DELETE_GRACE)
                    {
                        let _ = qb.delete(&g.info_hash, true).await;
                    }
                    continue;
                }
                let files: Vec<TrackedFile> =
                    serde_json::from_str(g.cleanup_files.as_deref().unwrap_or("[]"))
                        .unwrap_or_default();
                let local_save = paths::client_to_local(&save_root, &local_root, &g.save_path)
                    .unwrap_or_else(|| local_root.clone());
                let root = local_root.clone();
                let result =
                    tokio::task::spawn_blocking(move || finish_removal(&root, &local_save, &files))
                        .await?;
                match result {
                    Ok(n) => {
                        set_cleanup(st, &g, "cleaned", &format!("download removed from {cname} with its data ({n} leftover file(s) removed here); library files kept"), now).await?;
                        record_event(
                            st,
                            &g,
                            "download_removed",
                            format!("{}: removed from {cname} with its download data", g.title),
                            None,
                        )
                        .await?;
                        report.removed += 1;
                    }
                    Err(e) => {
                        set_cleanup(st, &g, "blocked", &e, now).await?;
                        report.blocked += 1;
                    }
                }
            }
            _ => {
                let Some(t) = info else {
                    set_cleanup(
                        st,
                        &g,
                        "client_removed",
                        &format!(
                            "the torrent is no longer in {cname}; any remaining data is left alone"
                        ),
                        now,
                    )
                    .await?;
                    continue;
                };
                if !ours(t, &g, post_category(st, &g).as_deref()) {
                    set_cleanup(st, &g, "blocked", "the torrent's category or unified-media tag was changed; it is treated as user-managed", now).await?;
                    report.blocked += 1;
                    continue;
                }
                // A failed, incomplete download has no seeding obligation; everything else
                // must meet its share limit and seed goal.
                let verdict = if g.state == "failed" && t.progress < 1.0 {
                    Seed::Met {
                        reason: "failed incomplete download".into(),
                    }
                } else {
                    let secs = seeding_secs(st, t).await;
                    seed_criteria(
                        t,
                        secs,
                        prefs.as_ref(),
                        g.seed_ratio,
                        g.seed_time_minutes,
                        false,
                    )
                };
                match verdict {
                    Seed::Met { reason } => match prepare_removal(st, &g, t).await {
                        Ok(files) => {
                            let why = if g.state == "failed" {
                                format!("removeFailedDownloads: {reason}")
                            } else {
                                format!("removeCompletedDownloads: {reason}")
                            };
                            journal_removal(st, &g, &files, &why, now).await?;
                        }
                        Err(e) => {
                            set_cleanup(st, &g, "blocked", &e, now).await?;
                            report.blocked += 1;
                        }
                    },
                    Seed::Unmet { reason } | Seed::Unknown { reason } => {
                        if g.cleanup_detail.as_deref() != Some(reason.as_str()) {
                            update(st, &g.id, "UPDATE grabs SET cleanup_detail = ?2, updated_at = ?3 WHERE id = ?1", vec![text(reason), text(ts(now))]).await?;
                        }
                        report.seeding += 1;
                    }
                }
            }
        }
    }
    Ok(report)
}

async fn set_cleanup(
    st: &AppState,
    g: &Grab,
    state: &str,
    detail: &str,
    now: OffsetDateTime,
) -> Result<()> {
    update(
        st,
        &g.id,
        "UPDATE grabs SET cleanup = ?2, cleanup_detail = ?3, cleanup_at = ?4, updated_at = ?4 WHERE id = ?1",
        vec![text(state), text(detail), text(ts(now))],
    )
    .await
}

/// Initial cleanup state after a successful import.
pub fn after_import_cleanup(g: &Grab) -> (&'static str, &'static str) {
    if g.remove_completed {
        (
            "seeding",
            "removeCompletedDownloads: kept seeding until the client stops it at a met share limit",
        )
    } else {
        (
            "disabled",
            "removeCompletedDownloads is off (or [execution] download_cleanup = false); the download keeps seeding",
        )
    }
}

// ------------------------------------------------------------------ queue views

/// The queue: active downloads and cleanup-tracked ones, plus (with `include_recent`) any
/// download updated in the last 7 days.
pub fn queue(
    c: &Connection,
    include_recent: bool,
    limit: u32,
    offset: u32,
) -> Result<(Vec<Grab>, u64)> {
    let since = if include_recent {
        ts(OffsetDateTime::now_utc() - Duration::from_secs(7 * 86_400))
    } else {
        "9999".to_string()
    };
    let cond = "(active = 1 OR cleanup IN ('seeding', 'removing', 'blocked') OR updated_at >= ?1)";
    let total: i64 = c.query_row(
        &format!("SELECT COUNT(*) FROM grabs WHERE {cond}"),
        [&since],
        |r| r.get(0),
    )?;
    let rows = select(
        c,
        &format!("{cond} ORDER BY active DESC, created_at DESC LIMIT ?2 OFFSET ?3"),
        &[&since, &limit, &offset],
    )?;
    Ok((rows, total as u64))
}

/// Blocked release refs/hashes for a media item (used by searches).
pub fn blocked_for(c: &Connection, media: MediaId) -> Result<HashSet<String>> {
    let mut stmt = c.prepare("SELECT release_ref, info_hash FROM blocklist WHERE media_id = ?1")?;
    let mut out = HashSet::new();
    for row in stmt.query_map([media.to_string()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })? {
        let (r, h) = row?;
        out.insert(r);
        if let Some(h) = h {
            out.insert(format!("hash:{h}"));
        }
    }
    Ok(out)
}

/// Counts of grabs by status (diagnostics).
pub fn counts(c: &Connection) -> Result<BTreeMap<String, i64>> {
    let all = select(
        c,
        "active = 1 OR cleanup IN ('seeding', 'removing', 'blocked')",
        &[],
    )?;
    let mut out = BTreeMap::new();
    for g in all {
        *out.entry(status(&g).to_string()).or_insert(0) += 1;
    }
    Ok(out)
}

/// Startup recovery: a grab left `importing` by a crash returns to `completed` (the import
/// journal is rolled back separately); an `unconfirmed` add is reconciled on the next pass.
pub fn recover(c: &Connection, now: OffsetDateTime) -> Result<usize> {
    let n = c.execute(
        "UPDATE grabs SET state = 'completed', lease_owner = NULL, lease_until = NULL, next_attempt_at = ?1, updated_at = ?1 WHERE state = 'importing'",
        [ts(now)],
    )?;
    c.execute(
        "UPDATE grabs SET next_attempt_at = ?1 WHERE state = 'unconfirmed'",
        [ts(now)],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(state: &str, ratio: f64, ratio_limit: f64, seeding: i64, time_limit: i64) -> TorrentInfo {
        serde_json::from_value(json!({
            "hash": "a".repeat(40), "state": state, "progress": 1.0, "amount_left": 0, "ratio": ratio,
            "ratio_limit": ratio_limit, "seeding_time": seeding, "seeding_time_limit": time_limit,
        }))
        .unwrap()
    }

    fn prefs(ratio: Option<f64>, minutes: Option<i64>) -> Preferences {
        Preferences {
            max_ratio_enabled: Some(ratio.is_some()),
            max_ratio: Some(ratio.unwrap_or(-1.0)),
            max_seeding_time_enabled: Some(minutes.is_some()),
            max_seeding_time: Some(minutes.unwrap_or(-1)),
        }
    }

    fn met(s: &Seed) -> bool {
        matches!(s, Seed::Met { .. })
    }

    #[test]
    fn seed_criteria_are_explicit_and_conservative() {
        let p = prefs(None, None);
        // Per-torrent ratio limit met and stopped by the client.
        assert!(met(&seed_criteria(
            &t("pausedUP", 2.1, 2.0, 60, -2),
            Some(60),
            Some(&p),
            Some(2.0),
            None,
            false
        )));
        assert!(met(&seed_criteria(
            &t("stoppedUP", 2.0, 2.0, 60, -2),
            Some(60),
            Some(&p),
            Some(2.0),
            None,
            false
        )));
        // Manually paused before the limit: unmet.
        assert!(!met(&seed_criteria(
            &t("pausedUP", 0.5, 2.0, 60, -2),
            Some(60),
            Some(&p),
            Some(2.0),
            None,
            false
        )));
        // Still seeding even though the ratio is reached.
        assert!(!met(&seed_criteria(
            &t("uploading", 2.5, 2.0, 60, -2),
            Some(60),
            Some(&p),
            Some(2.0),
            None,
            false
        )));
        // Unlimited per torrent and no global limit: never removed automatically.
        assert!(!met(&seed_criteria(
            &t("pausedUP", 9.0, -1.0, 60, -1),
            Some(60),
            Some(&p),
            None,
            None,
            false
        )));
        // Global limit: unknown preferences keep it; enabled global limit applies.
        assert!(matches!(
            seed_criteria(
                &t("pausedUP", 3.0, -2.0, 60, -2),
                Some(60),
                None,
                None,
                None,
                false
            ),
            Seed::Unknown { .. }
        ));
        assert!(met(&seed_criteria(
            &t("pausedUP", 3.0, -2.0, 60, -2),
            Some(60),
            Some(&prefs(Some(3.0), None)),
            None,
            None,
            false
        )));
        assert!(!met(&seed_criteria(
            &t("pausedUP", 2.9, -2.0, 60, -2),
            Some(60),
            Some(&prefs(Some(3.0), None)),
            None,
            None,
            false
        )));
        // Time limit (minutes vs seconds) met; ratio goal recorded at grab time must also be met.
        assert!(met(&seed_criteria(
            &t("pausedUP", 0.1, -1.0, 7200, 120),
            Some(7200),
            Some(&p),
            None,
            Some(120),
            false
        )));
        assert!(!met(&seed_criteria(
            &t("pausedUP", 0.1, -1.0, 7200, 120),
            Some(7200),
            Some(&p),
            Some(2.0),
            None,
            false
        )));
        // The client limit was lowered below the indexer goal: kept.
        assert!(!met(&seed_criteria(
            &t("pausedUP", 1.0, 1.0, 60, -2),
            Some(60),
            Some(&p),
            Some(2.0),
            None,
            false
        )));
        // Missing fields are unknown, never met.
        let mut no_ratio = t("pausedUP", 2.0, 2.0, 60, -2);
        no_ratio.ratio = None;
        assert!(matches!(
            seed_criteria(&no_ratio, Some(60), Some(&p), None, None, false),
            Seed::Unknown { .. }
        ));
        let mut no_limit = t("pausedUP", 2.0, 2.0, 60, -2);
        no_limit.ratio_limit = None;
        assert!(matches!(
            seed_criteria(&no_limit, Some(60), Some(&p), None, None, false),
            Seed::Unknown { .. }
        ));
        // Incomplete is never met.
        let mut partial = t("pausedUP", 5.0, 2.0, 60, -2);
        partial.progress = 0.5;
        assert!(!met(&seed_criteria(
            &partial,
            Some(60),
            Some(&p),
            None,
            None,
            true
        )));
        // Explicit removal ignores the client stop state but keeps the seed goal.
        assert!(met(&seed_criteria(
            &t("uploading", 2.5, 2.0, 60, -2),
            Some(60),
            Some(&p),
            Some(2.0),
            None,
            true
        )));
        assert!(!met(&seed_criteria(
            &t("uploading", 1.5, 2.0, 60, -2),
            Some(60),
            Some(&p),
            Some(2.0),
            None,
            true
        )));
    }

    #[cfg(unix)] // makes symlinks
    #[test]
    fn finish_removal_refuses_changed_and_symlinked_files() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("dl");
        let save = root.clone();
        std::fs::create_dir_all(save.join("Rel")).unwrap();
        let f = save.join("Rel/a.mkv");
        std::fs::write(&f, b"data").unwrap();
        let m = std::fs::metadata(&f).unwrap();
        let tracked = vec![TrackedFile {
            path: f.clone(),
            size: m.len(),
            ino: m.ino(),
        }];
        // Replaced file (different inode, same name and size) is left alone.
        std::fs::write(save.join("Rel/new"), b"data").unwrap();
        std::fs::rename(save.join("Rel/new"), &f).unwrap();
        assert_ne!(std::fs::metadata(&f).unwrap().ino(), m.ino());
        assert!(finish_removal(&root, &save, &tracked).is_err());
        assert!(f.exists());
        // Symlinked directory component is refused.
        let outside = tmp.path().join("library");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("b.mkv"), b"lib").unwrap();
        std::os::unix::fs::symlink(&outside, save.join("Link")).unwrap();
        let lm = std::fs::metadata(outside.join("b.mkv")).unwrap();
        let evil = vec![TrackedFile {
            path: save.join("Link/b.mkv"),
            size: lm.len(),
            ino: lm.ino(),
        }];
        assert!(finish_removal(&root, &save, &evil).is_err());
        assert!(outside.join("b.mkv").exists());
        // A matching file is removed with its empty directory.
        let m = std::fs::metadata(&f).unwrap();
        let ok = vec![TrackedFile {
            path: f.clone(),
            size: m.len(),
            ino: m.ino(),
        }];
        assert_eq!(finish_removal(&root, &save, &ok).unwrap(), 1);
        assert!(!save.join("Rel").exists());
    }
}
