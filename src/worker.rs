//! Background worker: one loop per process, woken by a notification or every
//! `poll_seconds`. Each pass runs queued commands first (user requests are never starved by
//! scheduled work), then the download pipeline (submit, poll, import, cleanup), then due
//! scheduled tasks (RSS sync, explicit request retries, metadata refresh, rescan, pruning).
//!
//! Commands are leased (`lease_owner`/`lease_until`); on restart, running commands whose
//! lease belongs to a dead instance are requeued (at most 3 attempts), interrupted imports
//! are rolled back from the journal and `importing` grabs return to `completed`.
//!
//! Normal monitoring is RSS-based, like native: nothing searches the whole missing backlog on
//! its own. Backlog searches run only when requested (`search_missing`), in bounded batches
//! interleaved with other work; explicit requests that found nothing are retried with backoff
//! until they succeed or expire.

use crate::api::AppState;
use crate::catalog::Episode;
use crate::ids::{MediaId, SeriesKey};
use crate::import::rfc3339;
use crate::library;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Notify;

pub const COMMANDS: &[&str] = &[
    "search",
    "search_missing",
    "rss_sync",
    "refresh_metadata",
    "rescan",
    "plex_refresh",
    "poll_downloads",
    "cleanup_downloads",
];
pub const TASKS: &[&str] = &[
    "downloads",
    "rss_sync",
    "requests",
    "refresh_metadata",
    "rescan",
    "prune",
    "bin",
    "sports",
];
const COMMAND_LEASE: Duration = Duration::from_secs(15 * 60);
const MAX_COMMAND_ATTEMPTS: i64 = 3;
const COMMANDS_PER_PASS: usize = 4;
const METADATA_PER_RUN: usize = 25;

pub struct Worker {
    pub notify: Notify,
    /// Random id of this process (lease owner).
    pub instance: String,
    /// Serialises grab mutations between the worker and API actions.
    pub grabs: tokio::sync::Mutex<()>,
    /// Set by `stop`: the loop finishes its current pass and exits.
    stopping: std::sync::atomic::AtomicBool,
    stop: Notify,
    /// Set while the download client's answers are missing: until its first answered poll
    /// in this process, and again after any unanswered one.
    unanswered: std::sync::atomic::AtomicBool,
    /// Smoothed download rate of each grab that is moving, kept in memory only (never stored).
    rates: std::sync::Mutex<std::collections::HashMap<String, Rate>>,
}

/// A grab's smoothed download rate and the bytes it still had left when last measured.
struct Rate {
    bytes_per_sec: f64,
    left: f64,
    at: std::time::Instant,
}

/// Each new reading counts for this much of the smoothed rate, so the estimate does not jump.
const RATE_WEIGHT: f64 = 0.3;
/// A reading older than this is not used: the grab is no longer being polled as moving.
const RATE_FRESH: Duration = Duration::from_secs(120);

impl Worker {
    pub fn new() -> Self {
        Worker {
            notify: Notify::new(),
            instance: format!(
                "w_{}",
                crate::fsutil::random_hex(6).unwrap_or_else(|_| "000000000000".into())
            ),
            grabs: tokio::sync::Mutex::new(()),
            stopping: std::sync::atomic::AtomicBool::new(false),
            stop: Notify::new(),
            unanswered: std::sync::atomic::AtomicBool::new(true),
            rates: Default::default(),
        }
    }

    fn rates(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Rate>> {
        self.rates.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records a poll's reading for a grab that is downloading: bytes per second and bytes left.
    pub fn note_rate(&self, grab: &str, bytes_per_sec: i64, left: i64) {
        let sample = bytes_per_sec.max(0) as f64;
        let mut rates = self.rates();
        let smoothed = match rates.get(grab) {
            Some(prev) if prev.at.elapsed() < RATE_FRESH => {
                prev.bytes_per_sec * (1.0 - RATE_WEIGHT) + sample * RATE_WEIGHT
            }
            _ => sample,
        };
        rates.insert(
            grab.to_string(),
            Rate {
                bytes_per_sec: smoothed,
                left: left.max(0) as f64,
                at: std::time::Instant::now(),
            },
        );
    }

    /// Forgets a grab's rate (paused, stalled, finished) so no time left is shown for it.
    pub fn clear_rate(&self, grab: &str) {
        self.rates().remove(grab);
    }

    /// Forgets the rates of grabs that are no longer being polled.
    pub fn keep_rates(&self, polled: &[String]) {
        self.rates().retain(|id, _| polled.contains(id));
    }

    /// Seconds a moving download has left at its smoothed rate; none when it is not moving.
    pub fn eta_seconds(&self, grab: &str) -> Option<u64> {
        let rates = self.rates();
        let r = rates.get(grab)?;
        let age = r.at.elapsed();
        if age >= RATE_FRESH || r.bytes_per_sec < 1.0 || r.left <= 0.0 {
            return None;
        }
        let left = (r.left / r.bytes_per_sec - age.as_secs_f64()).max(0.0);
        Some(left.round() as u64)
    }

    /// Asks the spawned loop to exit after its current pass.
    pub fn stop(&self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.stop.notify_one();
    }

    fn stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Records a poll the download client did not answer.
    pub fn client_unanswered(&self) {
        self.unanswered
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Records an answered poll. Returns whether answers were missing since the previous
    /// answered poll (an unanswered poll since, or none yet in this process).
    pub fn client_answered(&self) -> bool {
        self.unanswered
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }
}

impl Default for Worker {
    fn default() -> Self {
        Self::new()
    }
}

fn ts(t: OffsetDateTime) -> String {
    rfc3339(t)
}

fn parse_ts(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

/// Queues a command unless the same command (and media) is already pending. Returns the id
/// of the queued or already pending command.
pub fn queue_command(
    c: &Connection,
    name: &str,
    media_id: Option<MediaId>,
    now: &str,
) -> Result<String> {
    let media = media_id.map(|m| m.to_string());
    let existing: Option<String> = c
        .query_row(
            "SELECT id FROM commands WHERE name = ?1 AND COALESCE(media_id, '') = COALESCE(?2, '') AND state IN ('queued', 'running')",
            params![name, media],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let id = format!("cmd_{}", crate::fsutil::random_hex(12)?);
    c.execute(
        "INSERT INTO commands (id, name, media_id, state, created_at) VALUES (?1, ?2, ?3, 'queued', ?4)",
        params![id, name, media, now],
    )?;
    Ok(id)
}

#[derive(Debug, Clone, Serialize)]
pub struct Command {
    pub id: String,
    pub name: String,
    pub media_id: Option<String>,
    pub state: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub result: Option<Value>,
    pub attempts: i64,
}

fn command_row(r: &rusqlite::Row) -> rusqlite::Result<Command> {
    Ok(Command {
        id: r.get(0)?,
        name: r.get(1)?,
        media_id: r.get(2)?,
        state: r.get(3)?,
        created_at: r.get(4)?,
        started_at: r.get(5)?,
        finished_at: r.get(6)?,
        result: r
            .get::<_, Option<String>>(7)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        attempts: r.get(8)?,
    })
}

const COMMAND_COLUMNS: &str =
    "id, name, media_id, state, created_at, started_at, finished_at, result, attempts";

pub fn get_command(c: &Connection, id: &str) -> Result<Option<Command>> {
    Ok(c.query_row(
        &format!("SELECT {COMMAND_COLUMNS} FROM commands WHERE id = ?1"),
        [id],
        command_row,
    )
    .optional()?)
}

pub fn list_commands(c: &Connection, limit: u32, offset: u32) -> Result<(Vec<Command>, u64)> {
    let total: i64 = c.query_row("SELECT COUNT(*) FROM commands", [], |r| r.get(0))?;
    let mut stmt = c.prepare(&format!("SELECT {COMMAND_COLUMNS} FROM commands ORDER BY created_at DESC, id DESC LIMIT ?1 OFFSET ?2"))?;
    let rows = stmt
        .query_map(params![limit, offset], command_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((rows, total as u64))
}

/// Task status rows with their schedule.
pub fn tasks(c: &Connection) -> Result<Vec<Value>> {
    let mut out = vec![];
    for name in TASKS {
        type Row = (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let row: Option<Row> = c
            .query_row(
                "SELECT last_started, last_finished, last_status, last_message, next_run FROM tasks WHERE name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let (started, finished, status, message, next) = row.unwrap_or_default();
        out.push(json!({
            "name": name,
            "last_started": started,
            "last_finished": finished,
            "last_status": status,
            "last_result": message.and_then(|m| serde_json::from_str::<Value>(&m).ok()),
            "next_run": next,
        }));
    }
    Ok(out)
}

/// Startup recovery (blocking, before the first pass).
pub fn recover(st: &AppState, now: OffsetDateTime) -> Result<Value> {
    let instance = st.worker.instance.clone();
    st.store.blocking(|c| {
        let journal = crate::ingest::recover(c)?;
        let grabs = crate::downloads::recover(c, now)?;
        let n = ts(now);
        let requeued = c.execute(
            "UPDATE commands SET state = 'queued', lease_owner = NULL, lease_until = NULL, attempts = attempts + 1 WHERE state = 'running' AND (lease_owner IS NULL OR lease_owner != ?1) AND attempts < ?2",
            params![instance, MAX_COMMAND_ATTEMPTS],
        )?;
        let abandoned = c.execute(
            "UPDATE commands SET state = 'failed', finished_at = ?2, result = '{\"error\":\"abandoned after repeated interruptions\"}' WHERE state = 'running' AND (lease_owner IS NULL OR lease_owner != ?1)",
            params![instance, n],
        )?;
        if !journal.is_empty() || grabs > 0 || requeued > 0 {
            library::event(c, &n, "recovered", None, None, None, &format!("startup recovery: {} journal action(s), {grabs} import(s) to retry, {requeued} command(s) requeued", journal.len()), Some(&json!({ "journal": journal })))?;
        }
        Ok(json!({ "journal": journal, "imports_to_retry": grabs, "commands_requeued": requeued, "commands_abandoned": abandoned }))
    })
}

/// Runs the loop until the process ends.
pub fn spawn(st: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match recover(&st, OffsetDateTime::now_utc()) {
            Ok(v) => tracing::info!(recovery = %v, "worker started"),
            Err(e) => tracing::error!(error = %format!("{e:#}"), "worker recovery failed"),
        }
        while !st.worker.stopping() {
            if let Err(e) = tick(&st, OffsetDateTime::now_utc()).await {
                tracing::error!(error = %format!("{e:#}"), "worker pass failed");
            }
            tokio::select! {
                _ = st.worker.notify.notified() => {}
                _ = st.worker.stop.notified() => {}
                _ = tokio::time::sleep(st.cfg.automation.poll) => {}
            }
        }
    })
}

#[derive(Debug, Default, Serialize)]
pub struct TickReport {
    pub commands: Vec<String>,
    pub tasks: Vec<String>,
}

/// One worker pass. Public so tests and the sandbox harness can drive time explicitly.
pub async fn tick(st: &AppState, now: OffsetDateTime) -> Result<TickReport> {
    let mut report = TickReport::default();
    if !st.cfg.execution_enabled {
        return Ok(report);
    }
    let mut seen: Vec<String> = vec![];
    for _ in 0..COMMANDS_PER_PASS {
        let Some(cmd) = claim(st, now, seen.clone()).await? else {
            break;
        };
        seen.push(cmd.id.clone());
        report.commands.push(cmd.name.clone());
        let outcome = run_command(st, &cmd, now).await;
        finish(st, &cmd, outcome, now).await?;
    }
    run_task(st, "downloads", now).await?;
    report.tasks.push("downloads".into());
    for name in [
        "rss_sync",
        "requests",
        "refresh_metadata",
        "rescan",
        "prune",
        "bin",
        "sports",
    ] {
        if due(st, name, now).await? {
            run_task(st, name, now).await?;
            report.tasks.push(name.into());
        }
    }
    Ok(report)
}

fn interval(st: &AppState, name: &str) -> Option<Duration> {
    let a = &st.cfg.automation;
    match name {
        "rss_sync" => st
            .current()
            .and_then(|l| crate::rss::interval(&l.settings, a.rss_interval_minutes)),
        "requests" => Some(Duration::from_secs(60)),
        "refresh_metadata" => Some(a.metadata_refresh),
        "rescan" => Some(a.rescan),
        "prune" => Some(Duration::from_secs(86_400)),
        "bin" => Some(Duration::from_secs(3_600)),
        // Cheap when nothing is due: it looks at the follows and which leagues were read when.
        "sports" => st.sports.as_ref().map(|_| Duration::from_secs(15 * 60)),
        _ => None,
    }
}

async fn due(st: &AppState, name: &'static str, now: OffsetDateTime) -> Result<bool> {
    if interval(st, name).is_none() {
        return Ok(false);
    }
    let next: Option<String> = st
        .store
        .with(move |c| {
            Ok(
                c.query_row("SELECT next_run FROM tasks WHERE name = ?1", [name], |r| {
                    r.get(0)
                })
                .optional()?
                .flatten(),
            )
        })
        .await?;
    Ok(next.as_deref().and_then(parse_ts).is_none_or(|n| n <= now))
}

/// Runs a task now and records its status and next run.
pub async fn run_task(st: &AppState, name: &str, now: OffsetDateTime) -> Result<Value> {
    let name_s = name.to_string();
    let started = ts(now);
    let s2 = started.clone();
    let n2 = name_s.clone();
    st.store
        .with(move |c| {
            c.execute(
                "INSERT INTO tasks (name, last_started) VALUES (?1, ?2) ON CONFLICT (name) DO UPDATE SET last_started = ?2",
                params![n2, s2],
            )?;
            Ok(())
        })
        .await?;
    let result: Result<Value> = match name {
        "downloads" => downloads_pass(st, now).await,
        "rss_sync" => crate::rss::sync(st, now, false).await.map(|r| json!(r)),
        "requests" => requests_pass(st, now).await,
        "refresh_metadata" => refresh_metadata(st, None, now).await,
        "rescan" => crate::scan::scan(st, now, false).await.map(|r| json!(r)),
        "prune" => prune(st, now).await,
        "bin" => crate::trash::purge_due(st, now).await,
        "sports" => match &st.sports {
            Some(s) => Ok(crate::sport::refresh::run(st.store.clone(), s.clone(), now).await),
            None => Ok(json!({ "note": "sport is turned off" })),
        },
        other => Err(anyhow::anyhow!("unknown task {other}")),
    };
    let (status, value) = match &result {
        Ok(v) => ("ok", v.clone()),
        Err(e) => ("failed", json!({ "error": format!("{e:#}") })),
    };
    let next = interval(st, name).map(|i| ts(now + i));
    let finished = ts(OffsetDateTime::now_utc().max(now));
    let msg = value.to_string();
    st.store
        .with(move |c| {
            c.execute(
                "UPDATE tasks SET last_finished = ?2, last_status = ?3, last_message = ?4, next_run = ?5 WHERE name = ?1",
                params![name_s, finished, status, msg, next],
            )?;
            Ok(())
        })
        .await?;
    result
}

async fn downloads_pass(st: &AppState, now: OffsetDateTime) -> Result<Value> {
    let submitted = crate::downloads::submit_due(st, now).await?;
    let polled = crate::downloads::poll(st, now, st.cfg.automation.stall_timeout).await?;
    let imported = crate::ingest::import_due(st, now).await?;
    let cleanup = crate::downloads::cleanup(st, now).await?;
    let plans = sync_plans(st, now).await?;
    Ok(
        json!({ "submitted": submitted, "poll": polled, "imported": imported, "cleanup": cleanup, "plans_imported": plans }),
    )
}

/// Executed plans follow their grabs: `download_queued` until the download client confirms one
/// (`download_started`, `submitted: true`), `imported` once all are imported, and
/// `download_failed` if every grab ended before any confirmation.
async fn sync_plans(st: &AppState, now: OffsetDateTime) -> Result<usize> {
    use crate::plan::PlanState;
    let mut plans = vec![];
    for state in [PlanState::DownloadQueued, PlanState::DownloadStarted] {
        plans.extend(st.store.list(Some(state), None, 100, 0).await?.0);
    }
    let mut n = 0;
    for mut p in plans {
        let ids = p.execution.grabs.clone();
        // (state, active, submitted_at) per grab
        let grabs: Vec<(String, bool, Option<String>)> = st
            .store
            .with(move |c| {
                let mut out = vec![];
                for id in ids {
                    if let Some(g) = crate::downloads::get(c, &id)? {
                        out.push((g.state, g.active, g.submitted_at));
                    }
                }
                Ok(out)
            })
            .await?;
        if grabs.is_empty() {
            continue;
        }
        let pending = grabs.iter().any(|(_, active, at)| *active && at.is_none());
        let confirmed: Vec<&String> = grabs.iter().filter_map(|(_, _, at)| at.as_ref()).collect();
        let next = if grabs.iter().all(|(s, _, _)| s == "imported") {
            PlanState::Imported
        } else if pending && confirmed.is_empty() {
            PlanState::DownloadQueued
        } else if !confirmed.is_empty() {
            PlanState::DownloadStarted
        } else {
            PlanState::DownloadFailed
        };
        let first = confirmed.iter().min().map(|s| s.to_string());
        if next == p.state && p.execution.confirmed_at == first {
            continue;
        }
        p.state = next;
        p.execution.submitted = first.is_some();
        p.execution.confirmed_at = first;
        p.execution.note = match next {
            PlanState::DownloadQueued => "queued; waiting for the download client to confirm".into(),
            PlanState::DownloadStarted => "the download client confirmed the download(s)".into(),
            PlanState::DownloadFailed => "every grab failed or was cancelled before the download client confirmed it (see /api/v1/queue?all=true)".into(),
            _ => "all grabs imported".into(),
        };
        p.updated_at = ts(now);
        st.store.update_plan(p).await?;
        n += 1;
    }
    Ok(n)
}

async fn prune(st: &AppState, now: OffsetDateTime) -> Result<Value> {
    let week = ts(now - Duration::from_secs(7 * 86_400));
    let quarter = ts(now - Duration::from_secs(90 * 86_400));
    st.store
        .with(move |c| {
            let links = c.execute("DELETE FROM release_links WHERE seen_at < ?1", [&week])?;
            let commands = c.execute(
                "DELETE FROM commands WHERE state IN ('completed', 'failed') AND created_at < ?1",
                [&quarter],
            )?;
            let requests = c.execute(
                "DELETE FROM requests WHERE state != 'active' AND updated_at < ?1",
                [&quarter],
            )?;
            Ok(json!({ "release_links": links, "commands": commands, "requests": requests }))
        })
        .await
}

/// Claims the oldest queued command not already run in this pass (batched commands
/// continue in later passes, so other work is never starved).
async fn claim(
    st: &AppState,
    now: OffsetDateTime,
    exclude: Vec<String>,
) -> Result<Option<Command>> {
    let (owner, until, now_s) = (st.worker.instance.clone(), ts(now + COMMAND_LEASE), ts(now));
    st.store
        .with(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let id: Option<String> = {
                let mut stmt = tx.prepare("SELECT id FROM commands WHERE state = 'queued' ORDER BY created_at, id")?;
                let ids = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                ids.into_iter().find(|i| !exclude.contains(i))
            };
            let Some(id) = id else { return Ok(None) };
            tx.execute(
                "UPDATE commands SET state = 'running', started_at = COALESCE(started_at, ?2), lease_owner = ?3, lease_until = ?4 WHERE id = ?1",
                params![id, now_s, owner, until],
            )?;
            let cmd = get_command(&tx, &id)?;
            tx.commit()?;
            Ok(cmd)
        })
        .await
}

enum CommandOutcome {
    Done(Value),
    /// More work remains (batched commands): back to the queue.
    Continue(Value),
    Retry(String),
    Failed(String),
}

async fn finish(
    st: &AppState,
    cmd: &Command,
    outcome: CommandOutcome,
    now: OffsetDateTime,
) -> Result<()> {
    let id = cmd.id.clone();
    let now_s = ts(now);
    let attempts = cmd.attempts;
    st.store
        .with(move |c| {
            match outcome {
                CommandOutcome::Done(v) => c.execute(
                    "UPDATE commands SET state = 'completed', finished_at = ?2, result = ?3, lease_owner = NULL, lease_until = NULL WHERE id = ?1",
                    params![id, now_s, v.to_string()],
                )?,
                CommandOutcome::Continue(v) => c.execute(
                    "UPDATE commands SET state = 'queued', result = ?2, lease_owner = NULL, lease_until = NULL WHERE id = ?1",
                    params![id, v.to_string()],
                )?,
                CommandOutcome::Retry(e) if attempts + 1 < MAX_COMMAND_ATTEMPTS => c.execute(
                    "UPDATE commands SET state = 'queued', attempts = attempts + 1, result = ?2, lease_owner = NULL, lease_until = NULL WHERE id = ?1",
                    params![id, json!({ "error": e, "retrying": true }).to_string()],
                )?,
                CommandOutcome::Retry(e) | CommandOutcome::Failed(e) => c.execute(
                    "UPDATE commands SET state = 'failed', finished_at = ?2, result = ?3, lease_owner = NULL, lease_until = NULL WHERE id = ?1",
                    params![id, now_s, json!({ "error": e }).to_string()],
                )?,
            };
            Ok(())
        })
        .await
}

async fn run_command(st: &AppState, cmd: &Command, now: OffsetDateTime) -> CommandOutcome {
    let media: Option<MediaId> = cmd.media_id.as_deref().and_then(|m| m.parse().ok());
    let r: Result<CommandOutcome> = async {
        Ok(match cmd.name.as_str() {
            "search" => {
                let Some(id) = media else { return Ok(CommandOutcome::Failed("search needs a media_id".into())) };
                let targets = search_targets(st, id, now);
                if targets.is_empty() {
                    return Ok(CommandOutcome::Done(json!({ "searched": 0, "note": "nothing wanted for this item" })));
                }
                let mut results = vec![];
                for t in targets {
                    match st.acquire(t, false, true, "automatic", now).await {
                        Ok(a) => results.push(json!({ "target": t, "plan_id": a.plan.id, "state": a.plan.state, "grabs": a.plan.execution.grabs })),
                        Err(e) if is_outage(&e) => return Ok(CommandOutcome::Retry(e.message)),
                        Err(e) => results.push(json!({ "target": t, "error": { "code": e.code, "message": e.message } })),
                    }
                }
                CommandOutcome::Done(json!({ "results": results }))
            }
            "search_missing" => search_missing(st, cmd, media, now).await?,
            "rss_sync" => CommandOutcome::Done(json!(crate::rss::sync(st, now, true).await?)),
            "refresh_metadata" => CommandOutcome::Done(refresh_metadata(st, media, now).await?),
            "rescan" => CommandOutcome::Done(json!(crate::scan::scan(st, now, true).await?)),
            "poll_downloads" => CommandOutcome::Done(downloads_pass(st, now).await?),
            "cleanup_downloads" => CommandOutcome::Done(json!(crate::downloads::cleanup(st, now).await?)),
            "plex_refresh" => {
                let (Some(plex), Some(id)) = (&st.plex, media) else {
                    return Ok(CommandOutcome::Done(json!({ "skipped": "Plex is not configured" })));
                };
                let kind = crate::downloads::media_kind(id);
                let Some(section) = plex.section_for(kind) else {
                    return Ok(CommandOutcome::Done(json!({ "skipped": format!("no Plex section configured for {}", kind.label()) })));
                };
                // With the root's Plex path configured, only the item's folder is scanned (the
                // whole section while it is empty: Plex ignores folder scans there).
                let index = st.index();
                let folder = match id {
                    MediaId::Movie { tmdb } => index.movie(tmdb).and_then(|m| m.folder.clone()),
                    other => other
                        .series_key()
                        .and_then(|k| index.series(k))
                        .and_then(|s| s.folder.clone()),
                };
                let path = folder.as_deref().and_then(|f| plex.plex_path(kind, f));
                match plex.refresh_import(section, path.as_deref()).await {
                    Ok(folder) => CommandOutcome::Done(json!({ "section": section, "refreshed": true, "scope": if folder { "folder" } else { "section" } })),
                    Err(e) => CommandOutcome::Retry(format!("plex {}", e.describe())),
                }
            }
            other => CommandOutcome::Failed(format!("unknown command {other}")),
        })
    }
    .await;
    r.unwrap_or_else(|e| CommandOutcome::Retry(format!("{e:#}")))
}

fn is_outage(e: &crate::error::ApiError) -> bool {
    use crate::error::ErrorCode::*;
    matches!(
        e.code,
        UpstreamTimeout | UpstreamUnavailable | UpstreamRateLimited | Overloaded
    )
}

/// Units an explicit search of `id` covers: a movie, an episode, a season, or every
/// monitored season of a series with wanted episodes.
fn search_targets(st: &AppState, id: MediaId, now: OffsetDateTime) -> Vec<MediaId> {
    match id {
        MediaId::Series(k) => {
            let index = st.index();
            let Some(s) = index.series(k) else {
                return vec![];
            };
            let mut seasons: Vec<u16> = s
                .episodes
                .iter()
                .flatten()
                .filter(|e| {
                    e.season > 0
                        && e.monitored
                        && !e.has_file
                        && !e.queued
                        && aired(e.air_date_utc.as_deref(), now)
                })
                .map(|e| e.season)
                .collect();
            if s.episodes.is_none() {
                seasons = s
                    .seasons
                    .iter()
                    .filter(|x| x.number > 0 && x.monitored)
                    .map(|x| x.number)
                    .collect();
            }
            seasons.sort_unstable();
            seasons.dedup();
            if !s.monitored {
                // Not acquiring here (unmonitored, observed or not adopted).
                return vec![];
            }
            if let Some(eps) = &s.episodes {
                // Only adopted (monitored) units: a whole season only when every episode of
                // it is adopted, else its adopted missing episodes one by one, so neither a
                // season pack nor a plan covers an episode that is not monitored here.
                let mut out = vec![];
                for n in seasons {
                    let all = eps.iter().filter(|e| e.season == n).all(|e| e.monitored);
                    if all {
                        out.push(MediaId::Season(k, n));
                    } else {
                        out.extend(
                            eps.iter()
                                .filter(|e| {
                                    e.season == n
                                        && e.monitored
                                        && !e.has_file
                                        && !e.queued
                                        && aired(e.air_date_utc.as_deref(), now)
                                })
                                .map(|e| MediaId::Episode(k, n, e.number)),
                        );
                    }
                }
                return out;
            }
            seasons.into_iter().map(|n| MediaId::Season(k, n)).collect()
        }
        other => vec![other],
    }
}

pub(crate) fn aired(date: Option<&str>, now: OffsetDateTime) -> bool {
    date.and_then(parse_ts).is_some_and(|t| t <= now)
}

/// Explicit backlog search: wanted units grouped by season, searched in batches of
/// `searches_per_run` per pass (oldest-searched first), until every unit has been searched
/// once since the command was created.
async fn search_missing(
    st: &AppState,
    cmd: &Command,
    scope: Option<MediaId>,
    now: OffsetDateTime,
) -> Result<CommandOutcome> {
    let index = st.index();
    let wanted = st
        .store
        .with({
            let index = index.clone();
            let delay = st.availability_delay_days();
            move |c| library::wanted(c, &index, now, delay)
        })
        .await?;
    let since = cmd.created_at.clone();
    let held = |w: &library::Wanted| {
        w.next_search_at
            .as_deref()
            .and_then(parse_ts)
            .is_some_and(|n| n > now)
    };
    // Seasons with a held episode (e.g. the 24 h cooldown after a cancel) are searched per
    // unheld episode instead, so the held one is not searched with its season.
    let held_seasons: std::collections::BTreeSet<(MediaId, u16)> = wanted
        .iter()
        .filter(|w| held(w))
        .filter_map(|w| match w.target {
            MediaId::Episode(_, s, _) => Some((w.media_id, s)),
            _ => None,
        })
        .collect();
    let mut groups: BTreeMap<String, (MediaId, Option<String>)> = BTreeMap::new();
    for w in wanted {
        if let Some(s) = scope
            && w.media_id != s
            && w.target != s
        {
            continue;
        }
        if w.last_search_at
            .as_deref()
            .is_some_and(|l| l >= since.as_str())
        {
            continue; // already searched by this command (or later)
        }
        if held(&w) {
            continue;
        }
        let unit = match w.target {
            MediaId::Episode(k, s, _)
                if !held_seasons.contains(&(w.media_id, s))
                    // Coexistence: a season is searched as a whole only when all of it is
                    // adopted here; otherwise its adopted episodes are searched one by one.
                    && crate::downloads::not_adopted(st, &[MediaId::Season(k, s)]).is_none() =>
            {
                MediaId::Season(k, s)
            }
            other => other,
        };
        let e = groups
            .entry(unit.to_string())
            .or_insert((unit, w.last_search_at.clone()));
        if w.last_search_at < e.1 {
            e.1 = w.last_search_at.clone();
        }
    }
    let mut order: Vec<(MediaId, Option<String>)> = groups.into_values().collect();
    order.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    let total = order.len();
    let batch: Vec<MediaId> = order
        .into_iter()
        .take(st.cfg.automation.searches_per_run as usize)
        .map(|x| x.0)
        .collect();
    let mut grabbed = 0;
    let mut searched = 0;
    for t in &batch {
        match st.acquire(*t, false, true, "automatic", now).await {
            Ok(a) => {
                searched += 1;
                grabbed += a.plan.execution.grabs.len();
            }
            Err(e) if is_outage(&e) => return Ok(CommandOutcome::Retry(e.message)),
            Err(_) => searched += 1,
        }
        // Mark every wanted unit of the group as searched by this command.
        let index = st.index();
        let targets: Vec<MediaId> = match *t {
            MediaId::Season(k, s) => index
                .series(k)
                .map(|x| {
                    x.episodes
                        .iter()
                        .flatten()
                        .filter(|e| e.season == s)
                        .map(|e| library::episode_id(x.id, s, e.number))
                        .collect()
                })
                .unwrap_or_default(),
            other => vec![other],
        };
        st.store
            .with(move |c| library::record_search(c, &targets, now, None, false))
            .await?;
    }
    let left = total.saturating_sub(batch.len());
    let v = json!({ "searched_this_pass": searched, "grabbed_this_pass": grabbed, "remaining_units": left });
    Ok(if left > 0 {
        CommandOutcome::Continue(v)
    } else {
        CommandOutcome::Done(v)
    })
}

// ------------------------------------------------------------------ explicit requests

/// Records an explicit request that found nothing, to be retried with backoff.
pub fn remember_request(
    c: &Connection,
    target: MediaId,
    media: MediaId,
    result: &str,
    now: OffsetDateTime,
    retry: Duration,
    expiry: Duration,
) -> Result<()> {
    let n = ts(now);
    let attempts: i64 = c
        .query_row(
            "SELECT attempts FROM requests WHERE target = ?1 AND state = 'active'",
            [target.to_string()],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let delay = (retry * (1u32 << attempts.clamp(0, 5) as u32)).min(Duration::from_secs(86_400));
    c.execute(
        "INSERT INTO requests (target, media_id, state, created_at, updated_at, attempts, next_attempt_at, last_result, expires_at)
         VALUES (?1, ?2, 'active', ?3, ?3, 1, ?4, ?5, ?6)
         ON CONFLICT (target) DO UPDATE SET state = 'active', updated_at = ?3, attempts = CASE WHEN requests.state = 'active' THEN requests.attempts + 1 ELSE 1 END,
           next_attempt_at = ?4, last_result = ?5, expires_at = CASE WHEN requests.state = 'active' THEN requests.expires_at ELSE ?6 END",
        params![target.to_string(), media.to_string(), n, ts(now + delay), result, ts(now + expiry)],
    )?;
    Ok(())
}

pub fn settle_request(
    c: &Connection,
    target: MediaId,
    state: &str,
    result: &str,
    now: OffsetDateTime,
) -> Result<()> {
    c.execute(
        "UPDATE requests SET state = ?2, last_result = ?3, updated_at = ?4 WHERE target = ?1 AND state = 'active'",
        params![target.to_string(), state, result, ts(now)],
    )?;
    Ok(())
}

pub fn list_requests(c: &Connection, active_only: bool) -> Result<Vec<Value>> {
    let mut stmt = c.prepare(
        "SELECT target, media_id, state, created_at, updated_at, attempts, next_attempt_at, last_result, expires_at FROM requests WHERE (?1 = 0 OR state = 'active') ORDER BY next_attempt_at LIMIT 500",
    )?;
    let rows = stmt
        .query_map([active_only as i64], |r| {
            Ok(json!({
                "target": r.get::<_, String>(0)?, "media_id": r.get::<_, String>(1)?, "state": r.get::<_, String>(2)?,
                "created_at": r.get::<_, String>(3)?, "updated_at": r.get::<_, String>(4)?, "attempts": r.get::<_, i64>(5)?,
                "next_attempt_at": r.get::<_, String>(6)?, "last_result": r.get::<_, Option<String>>(7)?, "expires_at": r.get::<_, String>(8)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

async fn requests_pass(st: &AppState, now: OffsetDateTime) -> Result<Value> {
    let (now_s, limit) = (ts(now), st.cfg.automation.searches_per_run);
    let due: Vec<(String, String)> = st
        .store
        .with(move |c| {
            let expired = c.execute("UPDATE requests SET state = 'expired', updated_at = ?1 WHERE state = 'active' AND expires_at <= ?1", [&now_s])?;
            let _ = expired;
            let mut stmt = c.prepare("SELECT target, expires_at FROM requests WHERE state = 'active' AND next_attempt_at <= ?1 ORDER BY next_attempt_at, target LIMIT ?2")?;
            let rows = stmt.query_map(params![now_s, limit], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;
    let mut done = vec![];
    for (t, _) in due {
        let Ok(target) = t.parse::<MediaId>() else {
            continue;
        };
        match st.acquire(target, false, true, "automatic", now).await {
            Ok(a) => done.push(json!({ "target": target, "state": a.plan.state, "grabs": a.plan.execution.grabs.len() })),
            Err(e) if is_outage(&e) => {
                // Outage: retry later without counting an attempt.
                let next = ts(now + st.cfg.automation.request_retry);
                st.store
                    .with(move |c| Ok(c.execute("UPDATE requests SET next_attempt_at = ?2 WHERE target = ?1", params![t, next]).map(|_| ())?))
                    .await?;
                done.push(json!({ "target": target, "deferred": e.message }));
            }
            Err(e) if e.code == crate::error::ErrorCode::NotAdopted => {
                // The scope stopped being acquired here (released or unmonitored): the request
                // is over, not retried.
                let (t2, msg) = (t.clone(), e.message.clone());
                st.store
                    .with(move |c| settle_request(c, t2.parse()?, "cancelled", &msg, now))
                    .await?;
                done.push(json!({ "target": target, "cancelled": e.message }));
            }
            Err(e) => done.push(json!({ "target": target, "error": e.message })),
        }
    }
    Ok(json!({ "retried": done }))
}

// ------------------------------------------------------------------ metadata refresh

/// Refreshes episode lists of managed series from TVmaze (bounded per run, least recently
/// refreshed first). New episodes become wanted when they air; monitoring and ownership are
/// never changed.
pub async fn refresh_metadata(
    st: &AppState,
    only: Option<MediaId>,
    now: OffsetDateTime,
) -> Result<Value> {
    let movies = match only {
        Some(MediaId::Movie { .. }) | None => refresh_movies(st, only, now).await?,
        _ => json!({ "skipped": "not requested" }),
    };
    let series = match only {
        Some(MediaId::Movie { .. }) => json!({ "skipped": "not requested" }),
        _ => refresh_series(st, only, now).await?,
    };
    Ok(json!({ "series": series, "movies": movies }))
}

/// Movie release evidence, status and runtime from Plex or TMDB (fresh), when configured.
/// Without either, availability still advances with the clock from the recorded release dates.
async fn refresh_movies(
    st: &AppState,
    only: Option<MediaId>,
    now: OffsetDateTime,
) -> Result<Value> {
    let Some(tmdb) = st.movies.as_ref() else {
        return Ok(
            json!({ "skipped": "no movie metadata source (Plex or TMDB) is configured; availability follows the recorded release dates" }),
        );
    };
    let only_s = only.map(|m| m.to_string());
    let rows: Vec<(String, String)> = st
        .store
        .with(move |c| {
            let mut stmt = c.prepare("SELECT id, data FROM media WHERE kind = 'movie' AND managed = 1 AND (?1 IS NULL OR id = ?1) ORDER BY COALESCE(refreshed_at, ''), id LIMIT ?2")?;
            let rows = stmt.query_map(params![only_s, METADATA_PER_RUN as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;
    let mut results = vec![];
    for (id, data) in rows {
        let Ok(MediaId::Movie { tmdb: tmdb_id }) = id.parse::<MediaId>() else {
            continue;
        };
        let fetched = match tmdb.movie_with(tmdb_id, true).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                results.push(json!({ "movie": id, "error": "no movie metadata found" }));
                continue;
            }
            Err(e) => {
                results.push(
                    json!({ "movie": id, "error": format!("movie metadata {}", e.describe()) }),
                );
                if matches!(
                    e,
                    crate::upstream::UpstreamError::Timeout
                        | crate::upstream::UpstreamError::Unavailable { .. }
                        | crate::upstream::UpstreamError::RateLimited
                ) {
                    break;
                }
                continue;
            }
        };
        let Ok(mut movie) = serde_json::from_str::<crate::catalog::Movie>(&data) else {
            continue;
        };
        let release = fetched.release();
        // Only evidence the provider actually states replaces what is recorded. "On sale today"
        // (Plex) is an observation, so it never moves an earlier recorded date forward.
        movie.in_cinemas = release.in_cinemas.or(movie.in_cinemas);
        movie.physical_release = release.physical_release.or(movie.physical_release);
        movie.digital_release = match (release.digital_release, movie.digital_release) {
            (Some(new), Some(old)) if fetched.observed_digital => Some(new.min(old)),
            (new, old) => new.or(old),
        };
        movie.status = fetched.status.clone().or(movie.status);
        movie.runtime_minutes = fetched.runtime.filter(|r| *r > 0).or(movie.runtime_minutes);
        let (id2, now_s, json_s) = (id.clone(), ts(now), serde_json::to_string(&movie)?);
        st.store
            .with(move |c| {
                c.execute(
                    "UPDATE media SET data = ?2, refreshed_at = ?3, updated_at = ?3 WHERE id = ?1",
                    params![id2, json_s, now_s],
                )?;
                Ok(())
            })
            .await?;
        results.push(json!({ "movie": id, "digital_release": movie.digital_release, "physical_release": movie.physical_release, "in_cinemas": movie.in_cinemas }));
    }
    st.reload_index().await?;
    Ok(json!(results))
}

async fn refresh_series(
    st: &AppState,
    only: Option<MediaId>,
    now: OffsetDateTime,
) -> Result<Value> {
    let Some(tv) = st.tvmaze.as_ref() else {
        return Ok(
            json!({ "skipped": "TVmaze is disabled; episode lists come only from imports" }),
        );
    };
    // A season or episode refresh means its series.
    let only = only.map(|m| match m {
        MediaId::Season(k, _) | MediaId::Episode(k, _, _) => MediaId::Series(k),
        other => other,
    });
    let only_s = only.map(|m| m.to_string());
    let ids: Vec<String> = st
        .store
        .with(move |c| {
            let mut stmt = c.prepare("SELECT id FROM media WHERE kind = 'series' AND managed = 1 AND (?1 IS NULL OR id = ?1) ORDER BY COALESCE(refreshed_at, '') , id LIMIT ?2")?;
            let rows = stmt.query_map(params![only_s, METADATA_PER_RUN as i64], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;
    let index = st.index();
    let mut results = vec![];
    for id in ids {
        let Ok(mid) = id.parse::<MediaId>() else {
            continue;
        };
        let Some(key) = mid.series_key() else {
            continue;
        };
        let tvmaze_id = index.series(key).and_then(|s| s.tvmaze_id);
        let show = match (tvmaze_id, key) {
            // Always fresh: a refresh must not be answered from the lookup cache.
            (Some(t), _) | (None, SeriesKey::Tvmaze(t)) => tv.by_id_with(t, true).await,
            (None, SeriesKey::Tvdb(t)) => tv.by_tvdb_with(t, true).await,
        };
        let show = match show {
            Ok(Some(s)) => s,
            Ok(None) => {
                results.push(json!({ "series": id, "error": "not found on TVmaze" }));
                continue;
            }
            Err(e) => {
                results.push(json!({ "series": id, "error": format!("tvmaze {}", e.describe()) }));
                if matches!(
                    e,
                    crate::upstream::UpstreamError::Timeout
                        | crate::upstream::UpstreamError::Unavailable { .. }
                        | crate::upstream::UpstreamError::RateLimited
                ) {
                    break;
                }
                continue;
            }
        };
        let eps = match tv.episodes_with(show.id, true).await {
            Ok(e) => e,
            Err(e) => {
                results.push(json!({ "series": id, "error": format!("tvmaze {}", e.describe()) }));
                continue;
            }
        };
        let list: Vec<Episode> = eps
            .iter()
            .filter_map(|e| {
                let season = u16::try_from(e.season?).ok().filter(|s| *s > 0)?;
                let number = u16::try_from(e.number?).ok().filter(|n| *n > 0)?;
                Some(Episode {
                    season,
                    number,
                    title: e.name.clone(),
                    air_date_utc: e.airstamp.as_deref().and_then(parse_ts).map(ts),
                    runtime_minutes: e.runtime.filter(|r| *r > 0),
                    has_file: false,
                    monitored: true,
                    file_quality: None,
                    file: None,
                    ownership_unknown: false,
                    queued: false,
                    replaceable: false,
                })
            })
            .collect();
        let (id2, now_s) = (id.clone(), ts(now));
        let r = st
            .store
            .with(move |c| {
                let r = library::upsert_episodes(c, mid, &list, now)?;
                c.execute(
                    "UPDATE media SET refreshed_at = ?2 WHERE id = ?1",
                    params![id2, now_s],
                )?;
                Ok(r)
            })
            .await
            .context("episode upsert")?;
        results.push(json!({ "series": id, "added": r.added, "updated": r.updated, "skipped": r.skipped, "held_back": r.held_back, "hold": r.hold, "divergence": r.divergence }));
    }
    st.reload_index().await?;
    Ok(json!(results))
}
