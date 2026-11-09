//! Managed catalog: the durable, mutable library state in `state.sqlite`.
//!
//! Imports stay immutable. Each newly activated import seeds the managed catalog insert-only:
//! user choices (monitoring, removal, added items) are never overwritten, and snapshot
//! ownership evidence is refreshed only for items without live scan/import evidence. Every
//! route resolves media through a `CatalogIndex` rebuilt from these tables, so ownership,
//! monitoring and pending downloads are always current.
//!
//! Ownership is tri-state per movie/episode: a present file => owned; else a completed scan of
//! the item => missing; else the snapshot's own flag; else unknown (snapshot series without
//! per-episode data). Metadata refreshes never set ownership.

use crate::catalog::{AltTitle, Catalog, CatalogIndex, Episode, FileEvidence, Movie, Series};
use crate::ids::{MediaId, SeriesKey};
use crate::import::rfc3339;
use crate::quality::Quality;
use crate::release::Revision;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use time::OffsetDateTime;

/// `episode:<ns>:<id>:<season>:<episode>` for a series id.
pub fn episode_id(series: MediaId, season: u16, episode: u16) -> MediaId {
    MediaId::Episode(series.series_key().expect("series id"), season, episode)
}

fn now_s(now: OffsetDateTime) -> String {
    rfc3339(now)
}

fn quality_of(s: Option<String>) -> Option<Quality> {
    s.and_then(|q| serde_json::from_value(serde_json::Value::String(q)).ok())
}

fn revision_of(v: Option<i64>, r: Option<i64>) -> Option<Revision> {
    Some(Revision {
        version: u8::try_from(v?).ok()?,
        real: u8::try_from(r.unwrap_or(0)).ok()?,
    })
}

/// Inserts a file-evidence row and links it to its targets. Returns the row id.
pub fn insert_file(
    c: &Connection,
    media_id: &str,
    ev: &FileEvidence,
    targets: &[MediaId],
    grab_id: Option<&str>,
    mtime: Option<i64>,
) -> Result<i64> {
    c.execute(
        "INSERT INTO files (media_id, path, size, quality, revision_version, revision_real, source, present, observed_at, probe, grab_id, mtime)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?10, ?11)",
        params![
            media_id,
            ev.path,
            ev.size.map(|s| s as i64),
            ev.quality.map(|q| q.as_str()),
            ev.revision.map(|r| i64::from(r.version)),
            ev.revision.map(|r| i64::from(r.real)),
            ev.source,
            ev.observed_at,
            ev.probe,
            grab_id,
            mtime,
        ],
    )?;
    let id = c.last_insert_rowid();
    for t in targets {
        c.execute(
            "INSERT OR IGNORE INTO file_targets (file_id, target) VALUES (?1, ?2)",
            params![id, t.to_string()],
        )?;
    }
    Ok(id)
}

fn has_live_evidence(c: &Connection, media_id: &str) -> Result<bool> {
    let scanned: Option<String> = c
        .query_row(
            "SELECT scanned_at FROM media WHERE id = ?1",
            [media_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let live: i64 = c.query_row(
        "SELECT COUNT(*) FROM files WHERE media_id = ?1 AND source != 'snapshot'",
        [media_id],
        |r| r.get(0),
    )?;
    Ok(scanned.is_some() || live > 0)
}

fn replace_snapshot_files(
    c: &Connection,
    media_id: &str,
    files: &[(FileEvidence, Vec<MediaId>)],
) -> Result<()> {
    c.execute(
        "UPDATE files SET present = 0 WHERE media_id = ?1 AND source = 'snapshot' AND present = 1",
        [media_id],
    )?;
    for (ev, targets) in files {
        insert_file(c, media_id, ev, targets, None, None)?;
    }
    Ok(())
}

fn snapshot_evidence(file: Option<&FileEvidence>, observed_at: &str) -> FileEvidence {
    file.cloned().unwrap_or(FileEvidence {
        path: None,
        size: None,
        quality: None,
        revision: None,
        source: "snapshot".into(),
        observed_at: observed_at.into(),
        probe: None,
    })
}

/// Seeds the managed catalog from an import (insert-only for user state). Idempotent per
/// import id. Returns `(new_items, refreshed_items)`.
///
/// In coexistence mode new items are seeded *observed* (`managed = 0`): their catalog data,
/// ownership evidence and native monitoring are kept, but nothing about them is acquired
/// until they are adopted. In legacy mode they are managed with their native monitoring.
/// Existing rows keep their managed state and user choices. Episodes and seasons that a later
/// import adds to a managed series follow that series' own rules (series flag, season flag,
/// "monitor new seasons"), never the native flags, so a re-import cannot widen an adopted
/// scope. Native episode flags are always recorded in `native_monitored`.
pub fn seed(
    c: &mut Connection,
    catalog: &Catalog,
    now: OffsetDateTime,
    mode: crate::config::MigrationMode,
) -> Result<(usize, usize)> {
    let seed_managed = mode == crate::config::MigrationMode::Legacy;
    let tx = c.transaction()?;
    let done: Option<String> = tx
        .query_row(
            "SELECT import_id FROM seeded_imports WHERE import_id = ?1",
            [&catalog.import_id],
            |r| r.get(0),
        )
        .optional()?;
    if done.is_some() {
        return Ok((0, 0));
    }
    let now = now_s(now);
    let (mut added, mut refreshed) = (0, 0);
    for m in &catalog.movies {
        let id = m.id.to_string();
        // (managed, adopted): an observed row (neither) still carries the native intent.
        let exists: Option<(bool, bool)> = tx
            .query_row(
                "SELECT managed, adopted_at IS NOT NULL FROM media WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let files: Vec<(FileEvidence, Vec<MediaId>)> = if m.has_file {
            vec![(
                snapshot_evidence(m.file.as_ref(), &catalog.observed_at),
                vec![m.id],
            )]
        } else {
            vec![]
        };
        match exists {
            None => {
                tx.execute(
                    "INSERT INTO media (id, kind, data, monitored, managed, origin, added_at, updated_at) VALUES (?1, 'movie', ?2, ?3, ?5, 'snapshot', ?4, ?4)",
                    params![id, serde_json::to_string(m)?, m.monitored, now, seed_managed],
                )?;
                replace_snapshot_files(&tx, &id, &files)?;
                added += 1;
            }
            Some((managed, adopted)) => {
                // Newer native metadata for snapshot items; never the user's monitoring choice.
                tx.execute(
                    "UPDATE media SET data = ?2, updated_at = ?3 WHERE id = ?1 AND origin = 'snapshot'",
                    params![id, serde_json::to_string(m)?, now],
                )?;
                if !managed && !adopted {
                    // Observed: nobody chose anything here yet, so it follows the native intent.
                    tx.execute(
                        "UPDATE media SET monitored = ?2 WHERE id = ?1 AND origin = 'snapshot'",
                        params![id, m.monitored],
                    )?;
                }
                if !has_live_evidence(&tx, &id)? {
                    replace_snapshot_files(&tx, &id, &files)?;
                }
                refreshed += 1;
            }
        }
    }
    for s in &catalog.series {
        let id = s.id.to_string();
        let existing: Option<(bool, bool, bool)> = tx
            .query_row(
                "SELECT managed, monitored, monitor_new FROM media WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        // Observed (imported in coexistence mode, never adopted): it follows the latest native
        // intent, flags included, until it is adopted.
        let observed: bool = existing.is_some_and(|(managed, _, _)| !managed)
            && tx.query_row(
                "SELECT adopted_at IS NULL AND origin = 'snapshot' FROM media WHERE id = ?1",
                [&id],
                |r| r.get(0),
            )?;
        // Specials the native app effectively monitors: they stay native (season 0 is never
        // acquired here).
        let native_specials = if s.monitored {
            s.episodes
                .iter()
                .flatten()
                .filter(|e| e.season == 0 && e.monitored)
                .count()
        } else {
            0
        };
        let mut data = s.clone();
        data.episodes = None;
        // Native "monitor new seasons" (Sonarr monitorNewItems; absent = all).
        let native_new = s.monitor_new_items.as_deref() != Some("none");
        // A series this server already manages keeps its own rules for anything new.
        let own_rules = existing.filter(|(managed, _, _)| *managed);
        match existing {
            None => {
                tx.execute(
                    "INSERT INTO media (id, kind, data, monitored, managed, origin, added_at, updated_at, monitor_new) VALUES (?1, 'series', ?2, ?3, ?5, 'snapshot', ?4, ?4, ?6)",
                    params![id, serde_json::to_string(&data)?, s.monitored, now, seed_managed, native_new],
                )?;
                added += 1;
            }
            Some(_) => {
                tx.execute(
                    "UPDATE media SET data = ?2, updated_at = ?3 WHERE id = ?1 AND origin = 'snapshot'",
                    params![id, serde_json::to_string(&data)?, now],
                )?;
                if observed {
                    tx.execute(
                        "UPDATE media SET monitored = ?2, monitor_new = ?3 WHERE id = ?1",
                        params![id, s.monitored, native_new],
                    )?;
                }
                refreshed += 1;
            }
        }
        tx.execute(
            "UPDATE media SET native_specials = ?2 WHERE id = ?1",
            params![id, native_specials as i64],
        )?;
        for season in &s.seasons {
            let flag = match own_rules {
                Some((_, monitored, monitor_new)) => monitored && monitor_new,
                None => season.monitored,
            };
            tx.execute(
                "INSERT INTO seasons (series_id, season, monitored) VALUES (?1, ?2, ?3)
                 ON CONFLICT (series_id, season) DO UPDATE SET monitored = CASE WHEN ?4 THEN excluded.monitored ELSE seasons.monitored END",
                params![id, season.number, flag, observed],
            )?;
        }
        let live = has_live_evidence(&tx, &id)?;
        let mut files = vec![];
        for e in s.episodes.iter().flatten() {
            if e.season == 0 {
                continue;
            }
            let eid = episode_id(s.id, e.season, e.number);
            let flag = match own_rules {
                Some((_, monitored, monitor_new)) => {
                    let season: Option<bool> = tx
                        .query_row(
                            "SELECT monitored FROM seasons WHERE series_id = ?1 AND season = ?2",
                            params![id, e.season],
                            |r| r.get(0),
                        )
                        .optional()?;
                    monitored && season.unwrap_or(monitor_new)
                }
                None => e.monitored,
            };
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO episodes (series_id, season, number, title, air_date_utc, runtime, monitored, snapshot_owned, origin, native_monitored)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'snapshot', ?9)",
                params![id, e.season, e.number, e.title, e.air_date_utc, e.runtime_minutes, flag, e.has_file, e.monitored],
            )?;
            if inserted == 0 {
                tx.execute(
                    "UPDATE episodes SET title = ?4, air_date_utc = ?5, runtime = ?6, native_monitored = ?7,
                       monitored = CASE WHEN ?8 THEN ?7 ELSE monitored END
                     WHERE series_id = ?1 AND season = ?2 AND number = ?3",
                    params![id, e.season, e.number, e.title, e.air_date_utc, e.runtime_minutes, e.monitored, observed],
                )?;
                if !live {
                    tx.execute(
                        "UPDATE episodes SET snapshot_owned = ?4 WHERE series_id = ?1 AND season = ?2 AND number = ?3",
                        params![id, e.season, e.number, e.has_file],
                    )?;
                }
            }
            if e.has_file {
                files.push((
                    snapshot_evidence(e.file.as_ref(), &catalog.observed_at),
                    vec![eid],
                ));
            }
        }
        if !live {
            replace_snapshot_files(&tx, &id, &files)?;
        }
    }
    tx.execute(
        "INSERT INTO seeded_imports (import_id, seeded_at) VALUES (?1, ?2)",
        params![catalog.import_id, now],
    )?;
    tx.commit()?;
    Ok((added, refreshed))
}

/// Present file evidence per target (best = newest live evidence, else snapshot).
fn file_evidence_by_target(c: &Connection) -> Result<HashMap<String, FileEvidence>> {
    let mut stmt = c.prepare(
        "SELECT t.target, f.path, f.size, f.quality, f.revision_version, f.revision_real, f.source, f.observed_at, f.probe
         FROM file_targets t JOIN files f ON f.id = t.file_id WHERE f.present = 1
         ORDER BY CASE f.source WHEN 'snapshot' THEN 0 ELSE 1 END, f.observed_at, f.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            FileEvidence {
                path: r.get(1)?,
                size: r.get::<_, Option<i64>>(2)?.map(|s| s as u64),
                quality: quality_of(r.get(3)?),
                revision: revision_of(r.get(4)?, r.get(5)?),
                source: r.get(6)?,
                observed_at: r.get(7)?,
                probe: r.get(8)?,
            },
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (t, ev) = row?;
        out.insert(t, ev); // later rows (live, newer) win
    }
    Ok(out)
}

fn queued_targets(c: &Connection) -> Result<HashSet<String>> {
    let mut stmt = c.prepare(
        "SELECT t.target FROM grab_targets t JOIN grabs g ON g.id = t.grab_id WHERE g.active = 1",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect::<std::result::Result<HashSet<_>, _>>()
        .map_err(Into::into)
}

/// Rebuilds the in-memory index used by every route.
pub fn load_index(c: &Connection, mode: crate::config::MigrationMode) -> Result<CatalogIndex> {
    let files = file_evidence_by_target(c)?;
    let queued = queued_targets(c)?;
    let (import_id, observed_at): (String, String) = c
        .query_row(
            "SELECT import_id, seeded_at FROM seeded_imports ORDER BY seeded_at DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or_default();
    let mut movies = vec![];
    let mut series = vec![];
    let mut stmt = c.prepare(
        "SELECT id, kind, data, monitored, managed, origin, scanned_at, adopted_at IS NOT NULL, native_specials, metadata_hold FROM media ORDER BY id",
    )?;
    type Row = (
        String,
        String,
        String,
        bool,
        bool,
        String,
        Option<String>,
        bool,
        i64,
        Option<String>,
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
                r.get(9)?,
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let coexistence = mode == crate::config::MigrationMode::Coexistence;
    let mut episodes: HashMap<String, Vec<(Episode, Option<bool>)>> = HashMap::new();
    let mut ep_stmt = c.prepare(
        "SELECT series_id, season, number, title, air_date_utc, runtime, monitored, snapshot_owned FROM episodes ORDER BY series_id, season, number",
    )?;
    for row in ep_stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Episode {
                season: r.get(1)?,
                number: r.get(2)?,
                title: r.get(3)?,
                air_date_utc: r.get(4)?,
                runtime_minutes: r.get(5)?,
                has_file: false,
                monitored: r.get(6)?,
                file_quality: None,
                file: None,
                ownership_unknown: false,
                queued: false,
            },
            r.get::<_, Option<bool>>(7)?,
        ))
    })? {
        let (sid, e, snap) = row?;
        episodes.entry(sid).or_default().push((e, snap));
    }
    let mut season_monitor: HashMap<(String, u16), bool> = HashMap::new();
    let mut s_stmt = c.prepare("SELECT series_id, season, monitored FROM seasons")?;
    for row in s_stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, u16>(1)?,
            r.get::<_, bool>(2)?,
        ))
    })? {
        let (sid, n, m) = row?;
        season_monitor.insert((sid, n), m);
    }
    for (id, kind, data, monitored, managed, origin, scanned, adopted, specials, hold) in rows {
        let acquisition = crate::catalog::acquisition_state(managed, &origin, adopted);
        // Effective monitoring: only what this server acquires. In coexistence mode an item
        // must also have been adopted; imported items stay with the native app until then.
        let acquiring = managed && (!coexistence || acquisition == "adopted");
        if kind == "movie" {
            let mut m: Movie =
                serde_json::from_str(&data).with_context(|| format!("media {id}"))?;
            m.monitored = monitored && acquiring;
            m.acquisition = Some(acquisition.into());
            let ev = files.get(&id).cloned();
            m.has_file = ev.is_some();
            m.file_quality = ev.as_ref().and_then(|e| e.quality);
            m.file = ev;
            m.ownership_unknown = false;
            m.queued = queued.contains(&id);
            m.provenance.source_app = if origin == "api" {
                "unified-media".into()
            } else {
                m.provenance.source_app
            };
            if !managed {
                m.provenance.source_app = format!("{} (unmanaged)", m.provenance.source_app);
            }
            movies.push(m);
        } else {
            let mut s: Series =
                serde_json::from_str(&data).with_context(|| format!("media {id}"))?;
            s.monitored = monitored && acquiring;
            s.acquisition = Some(acquisition.into());
            s.native_specials_monitored = u32::try_from(specials).ok().filter(|n| *n > 0);
            s.metadata_hold = hold.and_then(|h| serde_json::from_str(&h).ok());
            for season in s.seasons.iter_mut() {
                if let Some(m) = season_monitor.get(&(id.clone(), season.number)) {
                    season.monitored = *m;
                }
            }
            let eps = episodes.remove(&id).unwrap_or_default();
            if !eps.is_empty() {
                let list = eps
                    .into_iter()
                    .map(|(mut e, snap)| {
                        let eid = episode_id(s.id, e.season, e.number).to_string();
                        let ev = files.get(&eid).cloned();
                        match (&ev, scanned.is_some(), snap) {
                            (Some(_), _, _) => e.has_file = true,
                            (None, true, _) | (None, false, Some(false)) => e.has_file = false,
                            (None, false, Some(true)) => e.has_file = true,
                            (None, false, None) => {
                                e.has_file = false;
                                e.ownership_unknown = origin != "api";
                            }
                        }
                        e.file_quality = ev.as_ref().and_then(|x| x.quality);
                        e.file = ev;
                        e.queued = queued.contains(&eid);
                        e
                    })
                    .collect();
                s.episodes = Some(list);
            }
            if origin == "api" {
                s.provenance.source_app = "unified-media".into();
            }
            if !managed {
                s.provenance.source_app = format!("{} (unmanaged)", s.provenance.source_app);
            }
            series.push(s);
        }
    }
    Ok(CatalogIndex::new(Catalog {
        schema_version: crate::catalog::CATALOG_SCHEMA_VERSION,
        import_id,
        observed_at,
        movies,
        series,
    }))
}

/// How a series' episodes are monitored when it is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorMode {
    /// Every episode.
    All,
    /// Episodes that have not aired yet.
    Future,
    /// Nothing (the series is managed but not searched).
    None,
}

/// Outcome of an add: whether the media row was new.
pub struct Added {
    pub created: bool,
}

pub fn is_managed(c: &Connection, id: &str) -> Result<Option<bool>> {
    Ok(
        c.query_row("SELECT managed FROM media WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?,
    )
}

/// Sets the quality profile of a title already in the catalog (a movie or a series). Files
/// already owned are untouched: the profile only decides what is looked for next. False when
/// the catalog has no such title.
pub fn set_quality_profile(
    c: &mut Connection,
    id: MediaId,
    profile: &str,
    now: OffsetDateTime,
) -> Result<bool> {
    let tx = c.transaction()?;
    let sid = id.to_string();
    let data: Option<String> = tx
        .query_row("SELECT data FROM media WHERE id = ?1", [&sid], |r| r.get(0))
        .optional()?;
    let Some(data) = data else {
        return Ok(false);
    };
    let mut v: serde_json::Value = serde_json::from_str(&data)?;
    v["quality_profile"] = serde_json::Value::String(profile.to_string());
    tx.execute(
        "UPDATE media SET data = ?2, updated_at = ?3 WHERE id = ?1",
        params![sid, serde_json::to_string(&v)?, now_s(now)],
    )?;
    tx.commit()?;
    Ok(true)
}

/// Adds (or re-manages) a movie.
pub fn add_movie(
    c: &mut Connection,
    m: &Movie,
    monitored: bool,
    now: OffsetDateTime,
) -> Result<Added> {
    let tx = c.transaction()?;
    let id = m.id.to_string();
    let now = now_s(now);
    let created = match is_managed(&tx, &id)? {
        Some(_) => {
            tx.execute(
                "UPDATE media SET managed = 1, monitored = ?2, updated_at = ?3, adopted_at = COALESCE(adopted_at, ?3) WHERE id = ?1",
                params![id, monitored, now],
            )?;
            false
        }
        None => {
            tx.execute(
                "INSERT INTO media (id, kind, data, monitored, managed, origin, added_at, updated_at, adopted_at) VALUES (?1, 'movie', ?2, ?3, 1, 'api', ?4, ?4, ?4)",
                params![id, serde_json::to_string(m)?, monitored, now],
            )?;
            true
        }
    };
    event(
        &tx,
        &now,
        "added",
        Some(&id),
        None,
        None,
        &format!(
            "{} {}",
            if created { "added" } else { "re-managed" },
            m.title
        ),
        None,
    )?;
    tx.commit()?;
    Ok(Added { created })
}

/// Adds (or re-manages) a series with its metadata episodes.
/// `monitor_new`: whether seasons that appear later (metadata refresh) are monitored.
pub fn add_series(
    c: &mut Connection,
    s: &Series,
    episodes: &[Episode],
    monitor: MonitorMode,
    monitor_new: bool,
    now: OffsetDateTime,
) -> Result<Added> {
    let tx = c.transaction()?;
    let id = s.id.to_string();
    let now_str = now_s(now);
    let created = match is_managed(&tx, &id)? {
        Some(_) => {
            // Re-managing (or re-activating) an existing series: the requested mode replaces
            // every dormant episode and season flag (for example native flags kept while the
            // series was unmonitored or observed), so exactly what was asked becomes effective.
            tx.execute(
                "UPDATE media SET managed = 1, monitored = ?2, monitor_new = ?4, updated_at = ?3, adopted_at = COALESCE(adopted_at, ?3) WHERE id = ?1",
                params![id, monitor != MonitorMode::None, now_str, monitor_new],
            )?;
            let all = monitor == MonitorMode::All;
            tx.execute(
                "UPDATE seasons SET monitored = ?2, by_follow = 0 WHERE series_id = ?1",
                params![id, monitor != MonitorMode::None],
            )?;
            match monitor {
                MonitorMode::All | MonitorMode::None => tx.execute(
                    "UPDATE episodes SET monitored = ?2 WHERE series_id = ?1",
                    params![id, all],
                )?,
                MonitorMode::Future => tx.execute(
                    "UPDATE episodes SET monitored = (air_date_utc IS NULL OR air_date_utc > ?2) WHERE series_id = ?1",
                    params![id, now_str],
                )?,
            };
            false
        }
        None => {
            let mut data = s.clone();
            data.episodes = None;
            tx.execute(
                "INSERT INTO media (id, kind, data, monitored, managed, origin, added_at, updated_at, monitor_new, adopted_at) VALUES (?1, 'series', ?2, ?3, 1, 'api', ?4, ?4, ?5, ?4)",
                params![id, serde_json::to_string(&data)?, monitor != MonitorMode::None, now_str, monitor_new],
            )?;
            true
        }
    };
    let mut seasons: Vec<u16> = episodes.iter().map(|e| e.season).collect();
    seasons.sort_unstable();
    seasons.dedup();
    for n in seasons {
        tx.execute(
            "INSERT OR IGNORE INTO seasons (series_id, season, monitored) VALUES (?1, ?2, ?3)",
            params![id, n, monitor != MonitorMode::None],
        )?;
    }
    for e in episodes.iter().filter(|e| e.season > 0) {
        let aired = e
            .air_date_utc
            .as_deref()
            .and_then(|d| {
                OffsetDateTime::parse(d, &time::format_description::well_known::Rfc3339).ok()
            })
            .is_some_and(|t| t <= now);
        let monitored = match monitor {
            MonitorMode::All => true,
            MonitorMode::Future => !aired,
            MonitorMode::None => false,
        };
        tx.execute(
            "INSERT OR IGNORE INTO episodes (series_id, season, number, title, air_date_utc, runtime, monitored, snapshot_owned, origin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 'api')",
            params![id, e.season, e.number, e.title, e.air_date_utc, e.runtime_minutes, monitored],
        )?;
    }
    event(
        &tx,
        &now_str,
        "added",
        Some(&id),
        None,
        None,
        &format!(
            "{} {}",
            if created { "added" } else { "re-managed" },
            s.title
        ),
        None,
    )?;
    tx.commit()?;
    Ok(Added { created })
}

/// Inserts a movie found in an existing library (Plex bootstrap) when it is not in the
/// catalog: owned by this server (adopted) but not monitored, so nothing is searched. An
/// existing row is never changed. Returns whether it was created.
pub fn bootstrap_movie(c: &Connection, m: &Movie, now: OffsetDateTime) -> Result<bool> {
    let id = m.id.to_string();
    if is_managed(c, &id)?.is_some() {
        return Ok(false);
    }
    let now = now_s(now);
    c.execute(
        "INSERT INTO media (id, kind, data, monitored, managed, origin, added_at, updated_at, adopted_at) VALUES (?1, 'movie', ?2, 0, 1, 'plex', ?3, ?3, ?3)",
        params![id, serde_json::to_string(m)?, now],
    )?;
    event(
        c,
        &now,
        "bootstrapped",
        Some(&id),
        None,
        None,
        &format!("{} found in the existing library (not monitored)", m.title),
        None,
    )?;
    Ok(true)
}

/// Inserts a series found in an existing library when it is not in the catalog: owned by this
/// server, not monitored, seasons appearing later not monitored. `episodes` must be a full
/// metadata episode list (they become the season lists) or empty (the lists stay unknown).
/// An existing series is never changed. Returns whether it was created.
pub fn bootstrap_series(
    c: &Connection,
    s: &Series,
    episodes: &[Episode],
    now: OffsetDateTime,
) -> Result<bool> {
    let id = s.id.to_string();
    if is_managed(c, &id)?.is_some() {
        return Ok(false);
    }
    let now = now_s(now);
    let mut data = s.clone();
    data.episodes = None;
    c.execute(
        "INSERT INTO media (id, kind, data, monitored, managed, origin, added_at, updated_at, monitor_new, adopted_at) VALUES (?1, 'series', ?2, 0, 1, 'plex', ?3, ?3, 0, ?3)",
        params![id, serde_json::to_string(&data)?, now],
    )?;
    for season in &s.seasons {
        c.execute(
            "INSERT OR IGNORE INTO seasons (series_id, season, monitored) VALUES (?1, ?2, 0)",
            params![id, season.number],
        )?;
    }
    for e in episodes.iter().filter(|e| e.season > 0) {
        c.execute(
            "INSERT OR IGNORE INTO episodes (series_id, season, number, title, air_date_utc, runtime, monitored, snapshot_owned, origin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, NULL, 'plex')",
            params![id, e.season, e.number, e.title, e.air_date_utc, e.runtime_minutes],
        )?;
    }
    event(
        c,
        &now,
        "bootstrapped",
        Some(&id),
        None,
        None,
        &format!("{} found in the existing library (not monitored)", s.title),
        None,
    )?;
    Ok(true)
}

/// Present file evidence for `target`, if any.
pub fn has_present_file(c: &Connection, target: &str) -> Result<bool> {
    Ok(c
        .query_row(
            "SELECT 1 FROM file_targets t JOIN files f ON f.id = t.file_id WHERE f.present = 1 AND t.target = ?1 LIMIT 1",
            [target],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Sets monitoring for a movie, series, season or episode. Returns false when unknown.
pub fn set_monitored(
    c: &mut Connection,
    id: MediaId,
    monitored: bool,
    now: OffsetDateTime,
) -> Result<bool> {
    let tx = c.transaction()?;
    let now = now_s(now);
    let changed = match id {
        MediaId::Movie { .. } | MediaId::Series(_) => tx.execute(
            "UPDATE media SET monitored = ?2, updated_at = ?3 WHERE id = ?1 AND managed = 1",
            params![id.to_string(), monitored, now],
        )?,
        MediaId::Season(k, s) => {
            let sid = MediaId::Series(k).to_string();
            let n = tx.execute(
                "INSERT INTO seasons (series_id, season, monitored) SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM media WHERE id = ?1 AND managed = 1)
                 ON CONFLICT (series_id, season) DO UPDATE SET monitored = excluded.monitored, by_follow = 0",
                params![sid, s, monitored],
            )?;
            tx.execute(
                "UPDATE episodes SET monitored = ?3 WHERE series_id = ?1 AND season = ?2",
                params![sid, s, monitored],
            )?;
            if monitored {
                tx.execute("UPDATE media SET monitored = 1 WHERE id = ?1", [&sid])?;
            }
            n
        }
        MediaId::Episode(k, s, e) => {
            let sid = MediaId::Series(k).to_string();
            let n = tx.execute(
                "UPDATE episodes SET monitored = ?4 WHERE series_id = ?1 AND season = ?2 AND number = ?3",
                params![sid, s, e, monitored],
            )?;
            if monitored && n > 0 {
                tx.execute("UPDATE media SET monitored = 1 WHERE id = ?1", [&sid])?;
            }
            n
        }
    };
    if changed > 0 {
        event(
            &tx,
            &now,
            if monitored {
                "monitored"
            } else {
                "unmonitored"
            },
            Some(&id.to_string()),
            Some(&id.to_string()),
            None,
            &format!(
                "{id} {}",
                if monitored {
                    "monitored"
                } else {
                    "unmonitored"
                }
            ),
            None,
        )?;
    }
    tx.commit()?;
    Ok(changed > 0)
}

/// Adopts one season or episode of a managed series: it becomes monitored here, and the
/// series is recorded as adopted. A series seeded by a legacy import and never adopted
/// before also stops monitoring seasons that appear later (that default was never a choice),
/// while keeping its current flags. Use after `add_series(MonitorMode::None)` for a series
/// that was not active, so that exactly this scope is effective.
pub fn adopt_scope(c: &mut Connection, scope: MediaId, now: OffsetDateTime) -> Result<bool> {
    let sid = match scope {
        MediaId::Season(k, _) | MediaId::Episode(k, _, _) => MediaId::Series(k).to_string(),
        other => bail!("{other} is not a season or episode"),
    };
    let now_str = now_s(now);
    c.execute(
        "UPDATE media SET monitor_new = 0 WHERE id = ?1 AND managed = 1 AND adopted_at IS NULL AND origin = 'snapshot'",
        [&sid],
    )?;
    c.execute(
        "UPDATE media SET adopted_at = ?2 WHERE id = ?1 AND managed = 1 AND adopted_at IS NULL",
        params![sid, now_str],
    )?;
    set_monitored(c, scope, true, now)
}

/// Whether Arr follows a managed series' new episodes: it acquires the series and a
/// season that appears later is monitored too.
pub fn following(c: &Connection, id: MediaId) -> Result<bool> {
    Ok(c.query_row(
        "SELECT monitored AND monitor_new FROM media WHERE id = ?1 AND managed = 1",
        [id.to_string()],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or(false))
}

/// Turns "get new episodes as they air" on or off for a managed series. Episodes that have
/// already aired, and every season someone chose, keep their monitoring; only what has not
/// aired yet (and any season that appears later) follows the switch. Returns false when the
/// series is not managed.
pub fn set_following(
    c: &mut Connection,
    id: MediaId,
    on: bool,
    now: OffsetDateTime,
) -> Result<bool> {
    let MediaId::Series(_) = id else {
        bail!("{id} is not a series");
    };
    let tx = c.transaction()?;
    let (sid, now_s) = (id.to_string(), now_s(now));
    let changed = if on {
        tx.execute(
            "UPDATE media SET monitored = 1, monitor_new = 1, updated_at = ?2, adopted_at = COALESCE(adopted_at, ?2) WHERE id = ?1 AND managed = 1",
            params![sid, now_s],
        )?
    } else {
        tx.execute(
            "UPDATE media SET monitor_new = 0, updated_at = ?2 WHERE id = ?1 AND managed = 1",
            params![sid, now_s],
        )?
    };
    if changed > 0 {
        // Turning on: a season with something still to air also takes episodes added to it
        // later; if that is what switched the season on, it is remembered (`by_follow`).
        // Turning off switches those seasons off again, and leaves every season someone
        // chose (Get season, an import) as it is, so it keeps taking late episodes.
        if on {
            tx.execute(
                "INSERT INTO seasons (series_id, season, monitored, by_follow) SELECT DISTINCT series_id, season, 1, 1 FROM episodes WHERE series_id = ?1 AND season > 0 AND (air_date_utc IS NULL OR air_date_utc > ?2)
                 ON CONFLICT (series_id, season) DO UPDATE SET by_follow = CASE WHEN seasons.monitored = 1 THEN seasons.by_follow ELSE 1 END, monitored = 1",
                params![sid, now_s],
            )?;
        } else {
            tx.execute(
                "UPDATE seasons SET monitored = 0, by_follow = 0 WHERE series_id = ?1 AND by_follow = 1",
                params![sid],
            )?;
        }
        tx.execute(
            "UPDATE episodes SET monitored = ?3 WHERE series_id = ?1 AND season > 0 AND (air_date_utc IS NULL OR air_date_utc > ?2)",
            params![sid, now_s, on],
        )?;
        let what = if on { "followed" } else { "unfollowed" };
        event(
            &tx,
            &now_s,
            if on { "monitored" } else { "unmonitored" },
            Some(&sid),
            Some(&sid),
            None,
            &format!("{id} {what} (new episodes)"),
            None,
        )?;
    }
    tx.commit()?;
    Ok(changed > 0)
}

fn handoff_scope(scope: MediaId) -> String {
    match scope {
        MediaId::Movie { .. } | MediaId::Series(_) => String::new(),
        MediaId::Season(_, s) => s.to_string(),
        MediaId::Episode(_, s, e) => format!("{s}:{e}"),
    }
}

/// Records that the operator confirmed the native handoff of `scope` of the catalog title
/// `title` (coexistence mode). It lasts until the title is released.
pub fn record_handoff(
    c: &Connection,
    title: MediaId,
    scope: MediaId,
    now: OffsetDateTime,
) -> Result<()> {
    c.execute(
        "INSERT OR IGNORE INTO handoffs (media_id, scope, confirmed_at) VALUES (?1, ?2, ?3)",
        params![title.to_string(), handoff_scope(scope), now_s(now)],
    )?;
    Ok(())
}

/// Whether a confirmed handoff covers `scope`: one for the whole title, its season or the
/// episode itself. Current monitoring flags play no part.
pub fn handoff_covers(c: &Connection, title: MediaId, scope: MediaId) -> Result<bool> {
    let mut keys = vec![String::new()];
    match scope {
        MediaId::Season(_, s) => keys.push(s.to_string()),
        MediaId::Episode(_, s, e) => keys.extend([s.to_string(), format!("{s}:{e}")]),
        _ => {}
    }
    let mut stmt = c.prepare("SELECT 1 FROM handoffs WHERE media_id = ?1 AND scope = ?2")?;
    for k in keys {
        if stmt.exists(params![title.to_string(), k])? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Coexistence mode, from the stored rows: why a grab of `covers` (the movie `title`, or
/// episodes of the series `title`) would not be adopted and monitored here now, or `None`.
/// Read in the transaction that records the grab, so a release, or a monitoring change
/// committed while the torrent was being fetched, is seen.
pub fn not_adopted_now(
    c: &Connection,
    title: MediaId,
    covers: &[MediaId],
) -> Result<Option<String>> {
    let key = title.to_string();
    let row: Option<(bool, bool, String, bool)> = c
        .query_row(
            "SELECT managed, monitored, origin, adopted_at IS NOT NULL FROM media WHERE id = ?1",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((managed, monitored, origin, adopted)) = row else {
        return Ok(Some(format!("{title} is not in the managed catalog")));
    };
    if !monitored || crate::catalog::acquisition_state(managed, &origin, adopted) != "adopted" {
        return Ok(Some(format!(
            "{title} is no longer adopted and monitored here (released or unmonitored while the release was being fetched)"
        )));
    }
    let mut stmt = c.prepare(
        "SELECT monitored FROM episodes WHERE series_id = ?1 AND season = ?2 AND number = ?3",
    )?;
    for t in covers {
        match *t {
            // As in the index check: a listed episode must be monitored; one the stored list
            // does not know yet is covered by its monitored series.
            MediaId::Episode(_, s, e) => {
                let m: Option<bool> = stmt
                    .query_row(params![key, s, e], |r| r.get(0))
                    .optional()?;
                if m == Some(false) {
                    return Ok(Some(format!("{t} is no longer monitored here")));
                }
            }
            other if other == title => {}
            other => return Ok(Some(format!("{other} is not a unit of {title}"))),
        }
    }
    Ok(None)
}

/// Outcome of `adopt_preserving`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preserved {
    /// Observed (or legacy-seeded) until now: adopted with its current flags untouched.
    Adopted,
    /// Already adopted: nothing changed (later choices are kept).
    AlreadyAdopted,
    /// Released by an earlier `DELETE /library`: its flags are no longer the native intent.
    Released,
    NotFound,
}

/// Adopts a whole imported movie or series *as it is*: the row becomes managed and adopted
/// in one transaction, and no monitoring flag (series, season, episode, "monitor new
/// seasons") is changed. The flags are those of the latest import (the native intent at that
/// snapshot), not a fresh native query. A replay changes nothing.
pub fn adopt_preserving(c: &mut Connection, id: MediaId, now: OffsetDateTime) -> Result<Preserved> {
    let tx = c.transaction()?;
    let key = id.to_string();
    let row: Option<(bool, String, bool)> = tx
        .query_row(
            "SELECT managed, origin, adopted_at IS NOT NULL FROM media WHERE id = ?1",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((managed, origin, adopted)) = row else {
        return Ok(Preserved::NotFound);
    };
    let outcome = match crate::catalog::acquisition_state(managed, &origin, adopted) {
        "adopted" => Preserved::AlreadyAdopted,
        "released" => Preserved::Released,
        _ => {
            let now_s = now_s(now);
            tx.execute(
                "UPDATE media SET managed = 1, adopted_at = ?2, updated_at = ?2 WHERE id = ?1",
                params![key, now_s],
            )?;
            event(
                &tx,
                &now_s,
                "adopted",
                Some(&key),
                None,
                None,
                &format!("{key} adopted with its imported monitoring preserved"),
                Some(&serde_json::json!({ "preserve_monitoring": true })),
            )?;
            Preserved::Adopted
        }
    };
    tx.commit()?;
    Ok(outcome)
}

/// Stops managing an item (unmonitored, hidden from library views). Files, history and
/// ownership evidence are kept so the item is never re-acquired by mistake. Refuses while
/// downloads for it are active.
pub fn remove(c: &mut Connection, id: MediaId, now: OffsetDateTime) -> Result<bool> {
    let tx = c.transaction()?;
    let key = id.to_string();
    let active: i64 = tx.query_row(
        "SELECT COUNT(*) FROM grabs WHERE media_id = ?1 AND active = 1",
        [&key],
        |r| r.get(0),
    )?;
    if active > 0 {
        bail!(
            "{active} active download(s) for {key}; cancel them before removing it from management"
        );
    }
    let n = tx.execute(
        "UPDATE media SET managed = 0, monitored = 0, updated_at = ?2 WHERE id = ?1",
        params![key, now_s(now)],
    )?;
    if n > 0 {
        // A release revokes every confirmed handoff of the title.
        tx.execute("DELETE FROM handoffs WHERE media_id = ?1", [&key])?;
        event(
            &tx,
            &now_s(now),
            "removed",
            Some(&key),
            None,
            None,
            &format!("{key} removed from management (files kept)"),
            None,
        )?;
    }
    tx.commit()?;
    Ok(n > 0)
}

/// Result of a metadata refresh for one series.
#[derive(Debug, Serialize)]
pub struct Upserted {
    pub added: usize,
    pub updated: usize,
    /// Metadata episodes not merged because they contradict the imported (native) numbering.
    pub skipped: usize,
    /// Why (bounded): unknown numbers inside imported seasons, imported air dates that differ.
    pub divergence: Vec<String>,
    /// New-season episodes not appended because of a numbering conflict in this refresh.
    pub held_back: usize,
    /// `none`, `set`, `kept` or `cleared`: the series' durable metadata hold.
    pub hold: &'static str,
}

/// How a metadata episode relates to the imported numbering.
enum Verdict {
    Existing,
    Append { new_season: bool },
    Conflict(String),
}

impl Upserted {
    fn diverge(&mut self, msg: String) {
        if self.divergence.len() < 20 {
            self.divergence.push(msg);
        }
    }
}

/// Upserts metadata episodes: new ones are added (monitored when the series and season are),
/// existing ones get title/air date/runtime only. Monitoring and ownership are never touched.
///
/// A series with imported per-episode evidence keeps the native (TVDB) numbering. Metadata
/// only appends the next episode of a season that has episodes, or the first episode of the
/// season right after the latest one. A gap, a number inside the imported range, an existing
/// episode renumbered (same title and air day) or an episode airing before the latest
/// imported one is a *conflict*: reported as divergence and never merged. While a refresh has
/// any conflict, no new season is appended and the series carries a durable `metadata_hold`
/// (shown in attention and status), cleared only by a refresh without conflicts whose list
/// includes every imported episode (an empty, truncated or partial response is no
/// evidence). Imported
/// episodes only have missing fields filled; a differing air date is reported, not rewritten.
pub fn upsert_episodes(
    c: &mut Connection,
    series: MediaId,
    eps: &[Episode],
    now: OffsetDateTime,
) -> Result<Upserted> {
    let tx = c.transaction()?;
    let sid = series.to_string();
    let (series_monitored, origin, data, monitor_new): (bool, String, String, bool) = tx
        .query_row(
            "SELECT monitored, origin, data, monitor_new FROM media WHERE id = ?1 AND managed = 1",
            [&sid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .context("series is not managed")?;
    let stats: Series = serde_json::from_str(&data)?;
    let mut out = Upserted {
        added: 0,
        updated: 0,
        skipped: 0,
        divergence: vec![],
        held_back: 0,
        hold: "none",
    };
    let mut per_season: BTreeMap<u16, usize> = BTreeMap::new();
    for e in eps.iter().filter(|e| e.season > 0) {
        *per_season.entry(e.season).or_default() += 1;
    }
    // Known numbers per season, and the seasons holding imported (native) episodes.
    let mut known: BTreeMap<u16, std::collections::BTreeSet<u16>> = BTreeMap::new();
    let mut imported_seasons: std::collections::BTreeSet<u16> = Default::default();
    let mut imported_episodes: std::collections::BTreeSet<(u16, u16)> = Default::default();
    // Existing episodes by (normalised title, air day), and the latest imported air date:
    // evidence that a "new" metadata episode is an existing one renumbered.
    let mut fingerprints: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut latest_imported: Option<String> = None;
    {
        let mut stmt = tx.prepare(
            "SELECT season, number, origin, title, air_date_utc FROM episodes WHERE series_id = ?1",
        )?;
        for row in stmt.query_map([&sid], |r| {
            Ok((
                r.get::<_, u16>(0)?,
                r.get::<_, u16>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })? {
            let (season, number, o, title, air) = row?;
            known.entry(season).or_default().insert(number);
            if let Some(fp) = fingerprint(title.as_deref(), air.as_deref()) {
                fingerprints.insert(fp, format!("S{season:02}E{number:02}"));
            }
            if o == "snapshot" {
                imported_seasons.insert(season);
                imported_episodes.insert((season, number));
                if let Some(a) = air.filter(|a| latest_imported.as_ref().is_none_or(|l| a > l)) {
                    latest_imported = Some(a);
                }
            }
        }
    }
    let last_imported_season = imported_seasons.iter().max().copied();
    let mut sorted: Vec<&Episode> = eps.iter().filter(|e| e.season > 0).collect();
    sorted.sort_by_key(|e| (e.season, e.number));
    // Pass 1: classify every metadata episode against the imported numbering (simulating
    // the appends), so a conflict anywhere in this refresh is known before anything is added.
    let mut sim = known.clone();
    let verdicts: Vec<Verdict> = sorted
        .iter()
        .map(|e| {
            if sim.get(&e.season).is_some_and(|k| k.contains(&e.number)) {
                return Verdict::Existing;
            }
            let label = format!("S{:02}E{:02}", e.season, e.number);
            let new_season = sim.get(&e.season).is_none_or(|k| k.is_empty());
            if let Some(last_season) = last_imported_season {
                let latest_season = sim
                    .iter()
                    .filter(|(n, k)| **n > 0 && !k.is_empty())
                    .map(|(n, _)| *n)
                    .max()
                    .unwrap_or(last_season);
                let justified = match sim.get(&e.season).filter(|k| !k.is_empty()) {
                    // A season with episodes: only its next episode.
                    Some(k) => k.iter().max().is_some_and(|m| e.number == m + 1),
                    // A new season: only the one right after the latest season with
                    // episodes, from its first episode (no gap in the season sequence).
                    None => e.season == latest_season + 1 && e.number == 1,
                };
                if !justified {
                    return Verdict::Conflict(format!("{label}: not in the imported episode list and not the next episode or season after it; not added (native numbering kept)"));
                }
                // A larger number alone does not make an episode new: one that is an
                // existing episode renumbered (same title and air day), or that aired before
                // the latest imported episode, is the provider disagreeing with the native
                // numbering.
                if let Some(other) = fingerprint(e.title.as_deref(), e.air_date_utc.as_deref())
                    .and_then(|fp| fingerprints.get(&fp))
                {
                    return Verdict::Conflict(format!("{label}: same title and air date as {other}; the metadata renumbers an existing episode, not added"));
                }
                if let (Some(a), Some(latest)) =
                    (e.air_date_utc.as_deref(), latest_imported.as_deref())
                    && days_apart(a, latest).is_some_and(|d| d > 1)
                    && a < latest
                {
                    return Verdict::Conflict(format!("{label}: airs {a}, before the latest imported episode ({latest}); not added as a new episode"));
                }
            }
            sim.entry(e.season).or_default().insert(e.number);
            Verdict::Append { new_season }
        })
        .collect();
    let conflicts: Vec<String> = verdicts
        .iter()
        .filter_map(|v| match v {
            Verdict::Conflict(why) => Some(why.clone()),
            _ => None,
        })
        .collect();
    // While the provider's numbering conflicts with the imported one, no new season is
    // appended (a later, seemingly contiguous season could be the same offset again).
    let hold = !conflicts.is_empty();
    // Only a response that lists every imported (native-numbered) episode can show that an
    // earlier conflict is resolved; an empty, truncated or partial one cannot.
    let covers_import = !imported_episodes.is_empty()
        && imported_episodes
            .iter()
            .all(|(n, x)| sorted.iter().any(|e| e.season == *n && e.number == *x));
    let mut held_seasons: std::collections::BTreeSet<u16> = Default::default();
    // Pass 2: apply.
    for (e, verdict) in sorted.into_iter().zip(verdicts) {
        let label = format!("S{:02}E{:02}", e.season, e.number);
        match verdict {
            Verdict::Conflict(why) => {
                out.skipped += 1;
                out.diverge(why);
                continue;
            }
            Verdict::Append { new_season }
                if hold && (new_season || held_seasons.contains(&e.season)) =>
            {
                held_seasons.insert(e.season);
                out.held_back += 1;
                out.diverge(format!("{label}: new season held back while the metadata numbering conflicts with the imported one"));
                continue;
            }
            Verdict::Existing => {
                let (o, air): (String, Option<String>) = tx.query_row(
                    "SELECT origin, air_date_utc FROM episodes WHERE series_id = ?1 AND season = ?2 AND number = ?3",
                    params![sid, e.season, e.number],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if o == "snapshot" {
                    if let (Some(a), Some(b)) = (air.as_deref(), e.air_date_utc.as_deref())
                        && days_apart(a, b).is_some_and(|d| d > 2)
                    {
                        out.diverge(format!(
                            "{label}: the imported air date {a} differs from the metadata's {b}; kept"
                        ));
                    }
                    out.updated += tx.execute(
                        "UPDATE episodes SET title = COALESCE(title, ?4), air_date_utc = COALESCE(air_date_utc, ?5), runtime = COALESCE(runtime, ?6)
                         WHERE series_id = ?1 AND season = ?2 AND number = ?3",
                        params![sid, e.season, e.number, e.title, e.air_date_utc, e.runtime_minutes],
                    )?;
                } else {
                    out.updated += tx.execute(
                        "UPDATE episodes SET title = COALESCE(?4, title), air_date_utc = COALESCE(?5, air_date_utc), runtime = COALESCE(?6, runtime)
                         WHERE series_id = ?1 AND season = ?2 AND number = ?3",
                        params![sid, e.season, e.number, e.title, e.air_date_utc, e.runtime_minutes],
                    )?;
                }
                continue;
            }
            Verdict::Append { .. } => {}
        }
        let season_monitored: Option<bool> = tx
            .query_row(
                "SELECT monitored FROM seasons WHERE series_id = ?1 AND season = ?2",
                params![sid, e.season],
                |r| r.get(0),
            )
            .optional()?;
        // A season that appears later follows the series' "monitor new seasons" choice (off
        // for a series added for one season only).
        let monitored = series_monitored && season_monitored.unwrap_or(monitor_new);
        // New episodes of a snapshot series without per-episode data: decide ownership from
        // the snapshot's season statistics only when they are conclusive.
        let snapshot_owned: Option<bool> = if origin == "snapshot" {
            stats
                .seasons
                .iter()
                .find(|x| x.number == e.season)
                .and_then(|x| match (x.episode_file_count, x.total_episode_count) {
                    (Some(0), _) => Some(false),
                    (Some(f), Some(t))
                        if t > 0 && f >= t && t as usize == per_season[&e.season] =>
                    {
                        Some(true)
                    }
                    _ => None,
                })
        } else {
            None
        };
        let n = tx.execute(
            "INSERT OR IGNORE INTO episodes (series_id, season, number, title, air_date_utc, runtime, monitored, snapshot_owned, origin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'metadata')",
            params![sid, e.season, e.number, e.title, e.air_date_utc, e.runtime_minutes, monitored, snapshot_owned],
        )?;
        if n > 0 {
            out.added += 1;
            known.entry(e.season).or_default().insert(e.number);
            if season_monitored.is_none() {
                tx.execute(
                    "INSERT OR IGNORE INTO seasons (series_id, season, monitored, by_follow) VALUES (?1, ?2, ?3, ?3)",
                    params![sid, e.season, series_monitored && monitor_new],
                )?;
            }
        }
    }
    // The hold is durable state: set while conflicts persist, cleared only by a refresh
    // that finds none.
    let now_str = now_s(now);
    let previous: Option<String> = tx.query_row(
        "SELECT metadata_hold FROM media WHERE id = ?1",
        [&sid],
        |r| r.get(0),
    )?;
    if hold {
        let since = previous
            .as_deref()
            .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
            .and_then(|v| v["since"].as_str().map(str::to_string))
            .unwrap_or_else(|| now_str.clone());
        let record = serde_json::json!({
            "since": since, "updated_at": now_str, "conflicts": conflicts.len(),
            "held_back": out.held_back,
            "reasons": conflicts.iter().take(10).collect::<Vec<_>>(),
        });
        tx.execute(
            "UPDATE media SET metadata_hold = ?2 WHERE id = ?1",
            params![sid, record.to_string()],
        )?;
        out.hold = if previous.is_some() { "kept" } else { "set" };
        if previous.is_none() {
            event(
                &tx,
                &now_str,
                "metadata_hold",
                Some(&sid),
                None,
                None,
                &format!(
                    "{} metadata episode(s) conflict with the imported numbering; new seasons are held",
                    conflicts.len()
                ),
                Some(&record),
            )?;
        }
    } else if let Some(prev) = previous.as_deref().filter(|_| !covers_import) {
        // No conflict seen, but not a response that could show the conflict resolved.
        let mut record: serde_json::Value = serde_json::from_str(prev).unwrap_or_default();
        record["updated_at"] = serde_json::json!(now_str);
        record["last_refresh"] = serde_json::json!(
            "the metadata response did not list every imported episode; not evidence that the conflict is resolved"
        );
        tx.execute(
            "UPDATE media SET metadata_hold = ?2 WHERE id = ?1",
            params![sid, record.to_string()],
        )?;
        out.hold = "kept";
    } else if previous.is_some() {
        tx.execute(
            "UPDATE media SET metadata_hold = NULL WHERE id = ?1",
            [&sid],
        )?;
        out.hold = "cleared";
        event(
            &tx,
            &now_str,
            "metadata_hold_cleared",
            Some(&sid),
            None,
            None,
            "the metadata numbering no longer conflicts with the imported one",
            None,
        )?;
    }
    let added = out.added;
    if added > 0 {
        event(
            &tx,
            &now_s(now),
            "metadata_refreshed",
            Some(&sid),
            None,
            None,
            &format!("{added} new episode(s)"),
            None,
        )?;
    }
    tx.commit()?;
    Ok(out)
}

/// (normalised title, air day) of an episode with a meaningful title and an air date.
fn fingerprint(title: Option<&str>, air: Option<&str>) -> Option<(String, String)> {
    let t: String = title?
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    let day = air?.get(..10)?.to_string();
    (t.len() >= 4).then_some((t, day))
}

/// Whole days between two RFC 3339 instants (`None` when either does not parse).
fn days_apart(a: &str, b: &str) -> Option<i64> {
    use time::format_description::well_known::Rfc3339;
    let a = OffsetDateTime::parse(a, &Rfc3339).ok()?;
    let b = OffsetDateTime::parse(b, &Rfc3339).ok()?;
    Some((a - b).whole_days().abs())
}

/// Appends a history event.
#[allow(clippy::too_many_arguments)]
pub fn event(
    c: &Connection,
    at: &str,
    kind: &str,
    media_id: Option<&str>,
    target: Option<&str>,
    grab_id: Option<&str>,
    message: &str,
    data: Option<&serde_json::Value>,
) -> Result<()> {
    c.execute(
        "INSERT INTO events (at, kind, media_id, target, grab_id, message, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![at, kind, media_id, target, grab_id, message, data.map(|d| d.to_string())],
    )?;
    Ok(())
}

/// A wanted (missing, monitored, aired) unit.
#[derive(Debug, Clone, Serialize)]
pub struct Wanted {
    pub target: MediaId,
    pub media_id: MediaId,
    pub title: String,
    pub air_date_utc: Option<String>,
    pub last_search_at: Option<String>,
    pub next_search_at: Option<String>,
    pub search_failures: i64,
}

fn aired(date: Option<&str>, now: OffsetDateTime) -> bool {
    date.and_then(|d| OffsetDateTime::parse(d, &time::format_description::well_known::Rfc3339).ok())
        .is_some_and(|t| t <= now)
}

/// Wanted units from the index plus schedule columns, oldest-searched first.
pub fn wanted(
    c: &Connection,
    index: &CatalogIndex,
    now: OffsetDateTime,
    availability_delay_days: i64,
) -> Result<Vec<Wanted>> {
    let mut sched: HashMap<String, (Option<String>, Option<String>, i64)> = HashMap::new();
    let mut stmt = c.prepare(
        "SELECT id, last_search_at, next_search_at, search_failures FROM media WHERE managed = 1",
    )?;
    for row in stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })? {
        let (id, l, n, f) = row?;
        sched.insert(id, (l, n, f));
    }
    let mut stmt = c.prepare("SELECT series_id, season, number, last_search_at, next_search_at, search_failures FROM episodes")?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, u16>(1)?,
            r.get::<_, u16>(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })? {
        let (sid, s, e, l, n, f) = row?;
        if let Ok(series) = sid.parse::<MediaId>() {
            sched.insert(episode_id(series, s, e).to_string(), (l, n, f));
        }
    }
    let mut out = vec![];
    let get = |k: &str| sched.get(k).cloned().unwrap_or_default();
    for m in &index.catalog.movies {
        if m.monitored
            && !m.has_file
            && !m.ownership_unknown
            && !m.queued
            && m.availability(now, availability_delay_days)
                == crate::catalog::Availability::Available
        {
            let (l, n, f) = get(&m.id.to_string());
            out.push(Wanted {
                target: m.id,
                media_id: m.id,
                title: match m.year {
                    Some(y) => format!("{} ({y})", m.title),
                    None => m.title.clone(),
                },
                air_date_utc: None,
                last_search_at: l,
                next_search_at: n,
                search_failures: f,
            });
        }
    }
    for s in index
        .catalog
        .series
        .iter()
        .filter(|s| s.monitored && s.series_type == "standard")
    {
        for e in s.episodes.iter().flatten() {
            if e.monitored
                && e.season > 0
                && !e.has_file
                && !e.ownership_unknown
                && !e.queued
                && aired(e.air_date_utc.as_deref(), now)
            {
                let target = episode_id(s.id, e.season, e.number);
                let (l, n, f) = get(&target.to_string());
                out.push(Wanted {
                    target,
                    media_id: s.id,
                    title: format!("{} S{:02}E{:02}", s.title, e.season, e.number),
                    air_date_utc: e.air_date_utc.clone(),
                    last_search_at: l,
                    next_search_at: n,
                    search_failures: f,
                });
            }
        }
    }
    out.sort_by(|a, b| {
        a.last_search_at
            .cmp(&b.last_search_at)
            .then(a.target.cmp(&b.target))
    });
    Ok(out)
}

/// Records a search attempt for targets: success (something grabbed) resets backoff;
/// otherwise the next attempt waits `backoff`.
pub fn record_search(
    c: &Connection,
    targets: &[MediaId],
    now: OffsetDateTime,
    next: Option<OffsetDateTime>,
    failed: bool,
) -> Result<()> {
    let (now, next) = (now_s(now), next.map(now_s));
    for t in targets {
        match *t {
            MediaId::Episode(k, s, e) => {
                c.execute(
                    "UPDATE episodes SET last_search_at = ?4, next_search_at = ?5, search_failures = CASE WHEN ?6 THEN search_failures + 1 ELSE 0 END
                     WHERE series_id = ?1 AND season = ?2 AND number = ?3",
                    params![MediaId::Series(k).to_string(), s, e, now, next, failed],
                )?;
            }
            other => {
                c.execute(
                    "UPDATE media SET last_search_at = ?2, next_search_at = ?3, search_failures = CASE WHEN ?4 THEN search_failures + 1 ELSE 0 END WHERE id = ?1",
                    params![other.to_string(), now, next, failed],
                )?;
            }
        }
    }
    Ok(())
}

/// Postpones automatic searches for targets (cancel cooldown) without counting a failure.
pub fn hold_search(c: &Connection, targets: &[MediaId], until: OffsetDateTime) -> Result<()> {
    let until = now_s(until);
    for t in targets {
        match *t {
            MediaId::Episode(k, s, e) => c.execute(
                "UPDATE episodes SET next_search_at = ?4 WHERE series_id = ?1 AND season = ?2 AND number = ?3",
                params![MediaId::Series(k).to_string(), s, e, until],
            )?,
            other => c.execute("UPDATE media SET next_search_at = ?2 WHERE id = ?1", params![other.to_string(), until])?,
        };
    }
    Ok(())
}

/// Calendar entries: episodes with an air date in `[start, end)`, with their status.
#[derive(Debug, Clone, Serialize)]
pub struct CalendarEntry {
    pub target: MediaId,
    pub series: MediaId,
    pub title: String,
    pub episode_title: Option<String>,
    pub air_date_utc: String,
    pub monitored: bool,
    /// `owned`, `queued`, `missing`, `unknown` or `upcoming`.
    pub status: &'static str,
}

pub fn calendar(
    index: &CatalogIndex,
    start: OffsetDateTime,
    end: OffsetDateTime,
    now: OffsetDateTime,
) -> Vec<CalendarEntry> {
    let mut out = vec![];
    for s in &index.catalog.series {
        for e in s.episodes.iter().flatten() {
            let Some(d) = e.air_date_utc.as_deref() else {
                continue;
            };
            let Ok(t) = OffsetDateTime::parse(d, &time::format_description::well_known::Rfc3339)
            else {
                continue;
            };
            if t < start || t >= end {
                continue;
            }
            let status = if e.has_file {
                "owned"
            } else if e.queued {
                "queued"
            } else if e.ownership_unknown {
                "unknown"
            } else if t > now {
                "upcoming"
            } else {
                "missing"
            };
            out.push(CalendarEntry {
                target: episode_id(s.id, e.season, e.number),
                series: s.id,
                title: format!("{} S{:02}E{:02}", s.title, e.season, e.number),
                episode_title: e.title.clone(),
                air_date_utc: d.to_string(),
                monitored: s.monitored && e.monitored,
                status,
            });
        }
    }
    out.sort_by(|a, b| {
        a.air_date_utc
            .cmp(&b.air_date_utc)
            .then(a.target.cmp(&b.target))
    });
    out
}

/// Builds catalog records for items added from metadata (TMDB/TVmaze/operator-supplied).
#[allow(clippy::too_many_arguments)]
pub fn new_movie(
    tmdb: u32,
    title: String,
    year: Option<u16>,
    runtime: Option<u32>,
    original_language: Option<String>,
    alternate_titles: Vec<String>,
    imdb_id: Option<String>,
    profile: Option<String>,
    root: Option<String>,
    folder: Option<String>,
    release: crate::catalog::ReleaseDates,
    source: &str,
    now: OffsetDateTime,
) -> Movie {
    Movie {
        id: MediaId::Movie { tmdb },
        tmdb_id: tmdb,
        imdb_id,
        title,
        original_title: None,
        alternate_titles,
        year,
        secondary_year: None,
        runtime_minutes: runtime,
        original_language,
        status: None,
        is_available: None,
        monitored: true,
        quality_profile: profile,
        root_folder: root,
        has_file: false,
        file_quality: None,
        identity_conflict: false,
        provenance: crate::catalog::Provenance {
            source_app: source.into(),
            native_id: 0,
            import_id: String::new(),
            observed_at: now_s(now),
        },
        folder,
        file: None,
        ownership_unknown: false,
        queued: false,
        minimum_availability: Some("released".into()),
        in_cinemas: release.in_cinemas,
        physical_release: release.physical_release,
        digital_release: release.digital_release,
        acquisition: None,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn new_series(
    key: SeriesKey,
    title: String,
    year: Option<u16>,
    runtime: Option<u32>,
    original_language: Option<String>,
    tvmaze_id: Option<u32>,
    profile: Option<String>,
    root: Option<String>,
    folder: Option<String>,
    source: &str,
    now: OffsetDateTime,
) -> Series {
    Series {
        id: MediaId::Series(key),
        tvdb_id: match key {
            SeriesKey::Tvdb(t) => t,
            SeriesKey::Tvmaze(_) => 0,
        },
        tvmaze_id,
        tmdb_id: None,
        imdb_id: None,
        title,
        alternate_titles: Vec::<AltTitle>::new(),
        year,
        series_type: "standard".into(),
        scene_numbering: false,
        runtime_minutes: runtime,
        original_language,
        status: None,
        monitored: true,
        quality_profile: profile,
        root_folder: root,
        seasons: vec![],
        episodes: None,
        identity_conflict: false,
        provenance: crate::catalog::Provenance {
            source_app: source.into(),
            native_id: 0,
            import_id: String::new(),
            observed_at: now_s(now),
        },
        folder,
        monitor_new_items: None,
        acquisition: None,
        native_specials_monitored: None,
        metadata_hold: None,
    }
}

/// Generated folder name with native semantics (rename off): `{Movie Title} ({Release Year})`
/// or `{Series Title}`, smart colon replacement, illegal characters replaced (or removed).
pub fn folder_name(title: &str, year: Option<u16>, replace_illegal: bool) -> String {
    let base = match year {
        Some(y) => format!("{title} ({y})"),
        None => title.to_string(),
    };
    clean_name(&base, replace_illegal)
}

/// A usable item folder: non-empty plain relative components, none hidden.
pub fn valid_folder(folder: &str) -> bool {
    !folder.is_empty()
        && folder
            .split('/')
            .all(|c| !c.is_empty() && !c.starts_with('.') && c.trim() == c)
        && crate::paths::safe_rel(folder).is_some()
}

/// Native `CleanFileName` with smart colon replacement.
pub fn clean_name(name: &str, replace_illegal: bool) -> String {
    let mut s = name.replace(": ", " - ").replace(':', "-");
    for (bad, good) in [
        ("\\", "+"),
        ("/", "+"),
        ("<", ""),
        (">", ""),
        ("?", "!"),
        ("*", "-"),
        ("|", ""),
        ("\"", ""),
    ] {
        s = s.replace(bad, if replace_illegal { good } else { "" });
    }
    s.trim().trim_end_matches(['.', ' ']).to_string()
}

/// Managed-catalog rows per acquisition state (`adopted`, `legacy_managed`, `observed`,
/// `released`), counted per kind.
pub fn ownership_counts(c: &Connection) -> Result<BTreeMap<String, usize>> {
    let mut stmt = c.prepare(
        "SELECT kind, managed, origin, adopted_at IS NOT NULL, COUNT(*) FROM media GROUP BY kind, managed, origin, adopted_at IS NOT NULL",
    )?;
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, bool>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, bool>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })? {
        let (kind, managed, origin, adopted, n) = row?;
        let state = crate::catalog::acquisition_state(managed, &origin, adopted);
        *out.entry(format!("{kind}s_{state}")).or_default() += n as usize;
        *out.entry(state.to_string()).or_default() += n as usize;
    }
    let with_specials: i64 = c.query_row(
        "SELECT COUNT(*) FROM media WHERE kind = 'series' AND managed = 1 AND (adopted_at IS NOT NULL OR origin = 'api') AND native_specials > 0",
        [],
        |r| r.get(0),
    )?;
    if with_specials > 0 {
        out.insert(
            "adopted_series_with_native_specials".into(),
            with_specials as usize,
        );
    }
    Ok(out)
}

/// Ownership rows (newest adoption first), optionally one acquisition state, with each
/// series' effective scope (monitored seasons and episodes) and the native flags recorded by
/// the latest import. Returns `(items, total)`.
pub fn ownership(
    c: &Connection,
    state: Option<&str>,
    only: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<(Vec<serde_json::Value>, usize)> {
    use serde_json::json;
    let mut stmt = c.prepare(
        "SELECT id, kind, data, monitored, managed, origin, adopted_at, monitor_new, native_specials FROM media WHERE (?1 IS NULL OR id = ?1) ORDER BY adopted_at IS NULL, adopted_at DESC, id",
    )?;
    type Row = (
        String,
        String,
        String,
        bool,
        bool,
        String,
        Option<String>,
        bool,
        i64,
    );
    let rows: Vec<Row> = stmt
        .query_map([only], |r| {
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
        .collect::<std::result::Result<_, _>>()?;
    let rows: Vec<(Row, &'static str)> = rows
        .into_iter()
        .map(|r| {
            let s = crate::catalog::acquisition_state(r.4, &r.5, r.6.is_some());
            (r, s)
        })
        .filter(|(_, s)| state.is_none_or(|want| want == *s))
        .collect();
    let total = rows.len();
    let mut out = vec![];
    for ((id, kind, data, monitored, _managed, origin, adopted_at, monitor_new, specials), acq) in
        rows.into_iter().skip(offset).take(limit)
    {
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap_or_default();
        let native = (origin == "snapshot").then(|| {
            json!({
                "app": parsed["provenance"]["source_app"],
                "monitored": parsed["monitored"],
                "observed_at": parsed["provenance"]["observed_at"],
            })
        });
        let mut h_stmt = c.prepare(
            "SELECT scope, confirmed_at FROM handoffs WHERE media_id = ?1 ORDER BY confirmed_at, scope",
        )?;
        let handoffs = h_stmt
            .query_map([&id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (scope, at) = row?;
                let scope = match scope.as_str() {
                    "" => id.clone(),
                    s if s.contains(':') => {
                        format!("{}:{s}", id.replacen("series:", "episode:", 1))
                    }
                    s => format!("{}:{s}", id.replacen("series:", "season:", 1)),
                };
                Ok(json!({ "scope": scope, "confirmed_at": at }))
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut v = json!({
            "media_id": id, "kind": kind, "title": parsed["title"], "year": parsed["year"],
            "acquisition": acq, "origin": origin, "adopted_at": adopted_at,
            "monitored": monitored, "native": native, "handoffs": handoffs,
        });
        if kind == "series" {
            let mut seasons: BTreeMap<u16, (Option<bool>, Vec<u16>, usize, usize)> =
                BTreeMap::new();
            let mut s_stmt =
                c.prepare("SELECT season, monitored FROM seasons WHERE series_id = ?1")?;
            for row in
                s_stmt.query_map([&id], |r| Ok((r.get::<_, u16>(0)?, r.get::<_, bool>(1)?)))?
            {
                let (n, m) = row?;
                seasons.entry(n).or_default().0 = Some(m);
            }
            let mut e_stmt = c.prepare(
                "SELECT season, number, monitored, COALESCE(native_monitored, 0) FROM episodes WHERE series_id = ?1 AND season > 0 ORDER BY season, number",
            )?;
            for row in e_stmt.query_map([&id], |r| {
                Ok((
                    r.get::<_, u16>(0)?,
                    r.get::<_, u16>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, bool>(3)?,
                ))
            })? {
                let (season, number, m, native_m) = row?;
                let e = seasons.entry(season).or_default();
                e.2 += 1;
                e.3 += usize::from(native_m);
                if m {
                    e.1.push(number);
                }
            }
            v["monitor_new_seasons"] = json!(monitor_new);
            // Natively monitored specials stay native: season 0 is never acquired here.
            v["native_specials_monitored"] = json!(specials);
            v["scope"] = json!(
                seasons
                    .into_iter()
                    .filter(|(n, _)| *n > 0)
                    .map(|(n, (flag, eps, total, native_m))| json!({
                        "season": n, "season_monitored": flag, "monitored_episodes": eps,
                        "episodes_known": total, "native_monitored_episodes": native_m,
                    }))
                    .collect::<Vec<_>>()
            );
        }
        out.push(v);
    }
    Ok((out, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_follow_native_cleaning() {
        assert_eq!(
            folder_name("The Matrix", Some(1999), true),
            "The Matrix (1999)"
        );
        assert_eq!(
            folder_name("Star Wars: Andor", None, true),
            "Star Wars - Andor"
        );
        assert_eq!(folder_name("What If...?", None, true), "What If...!");
        assert_eq!(folder_name("AC/DC: Live", None, false), "ACDC - Live");
        assert_eq!(clean_name("Name.", true), "Name");
        assert!(valid_folder("The Matrix (1999)") && valid_folder("Movies/The Matrix (1999)"));
        for bad in ["", "..", ".hidden", "a//b", " padded", "a/../b"] {
            assert!(!valid_folder(bad), "{bad:?}");
        }
        assert!(!valid_folder(&folder_name("..", None, true)));
    }
}
