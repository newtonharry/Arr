//! Durable shadow-plan storage (SQLite, WAL). One active plan per target is enforced by a
//! partial unique index. Idempotency keys live in their own table, bound to a request
//! fingerprint and the plan they returned, whether that plan was created or already active.

use crate::plan::{Plan, PlanState};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS plans (
    id TEXT PRIMARY KEY,
    target TEXT NOT NULL,
    state TEXT NOT NULL,
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    idempotency_key TEXT UNIQUE,
    request_fingerprint TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    plan_json TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS plans_one_active_per_target ON plans(target) WHERE active = 1;
CREATE INDEX IF NOT EXISTS plans_created ON plans(created_at, id);
"#;
/// Schema 2 adds key bindings. `plans.idempotency_key`/`request_fingerprint` are schema-1
/// legacy columns: migrated into `idempotency_keys`, no longer written.
const BINDINGS: &str = r#"
CREATE TABLE IF NOT EXISTS idempotency_keys (
    key TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    plan_id TEXT NOT NULL REFERENCES plans(id),
    bound_at TEXT NOT NULL
);
"#;
/// Schema 3: managed catalog, file evidence, downloads outbox, import journal, blocklist,
/// history, scheduled tasks, commands and root status. All timestamps are RFC 3339 UTC with
/// whole seconds, so string comparison orders them.
const EXECUTION: &str = r#"
CREATE TABLE IF NOT EXISTS media (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('movie', 'series')),
    data TEXT NOT NULL,
    monitored INTEGER NOT NULL,
    managed INTEGER NOT NULL DEFAULT 1,
    origin TEXT NOT NULL,
    added_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_search_at TEXT,
    next_search_at TEXT,
    search_failures INTEGER NOT NULL DEFAULT 0,
    scanned_at TEXT,
    refreshed_at TEXT,
    monitor_new INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS episodes (
    series_id TEXT NOT NULL REFERENCES media(id),
    season INTEGER NOT NULL,
    number INTEGER NOT NULL,
    title TEXT,
    air_date_utc TEXT,
    runtime INTEGER,
    monitored INTEGER NOT NULL,
    snapshot_owned INTEGER,
    origin TEXT NOT NULL,
    last_search_at TEXT,
    next_search_at TEXT,
    search_failures INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (series_id, season, number)
);
CREATE TABLE IF NOT EXISTS seasons (
    series_id TEXT NOT NULL REFERENCES media(id),
    season INTEGER NOT NULL,
    monitored INTEGER NOT NULL,
    PRIMARY KEY (series_id, season)
);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    media_id TEXT NOT NULL,
    path TEXT,
    size INTEGER,
    quality TEXT,
    revision_version INTEGER,
    revision_real INTEGER,
    source TEXT NOT NULL,
    present INTEGER NOT NULL,
    observed_at TEXT NOT NULL,
    probe TEXT,
    grab_id TEXT,
    mtime INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS files_present_path ON files(media_id, path) WHERE present = 1 AND path IS NOT NULL;
CREATE TABLE IF NOT EXISTS file_targets (
    file_id INTEGER NOT NULL REFERENCES files(id),
    target TEXT NOT NULL,
    PRIMARY KEY (file_id, target)
);
CREATE INDEX IF NOT EXISTS file_targets_target ON file_targets(target);
CREATE TABLE IF NOT EXISTS seeded_imports (import_id TEXT PRIMARY KEY, seeded_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS release_links (
    release_ref TEXT NOT NULL,
    target TEXT NOT NULL,
    media_id TEXT NOT NULL,
    info_hash TEXT,
    magnet TEXT,
    download_url TEXT,
    title TEXT NOT NULL,
    size INTEGER,
    indexer_id INTEGER,
    quality TEXT,
    revision_version INTEGER,
    revision_real INTEGER,
    full_season INTEGER NOT NULL,
    covers TEXT NOT NULL,
    replaces TEXT NOT NULL,
    interactive INTEGER NOT NULL,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (release_ref, target)
);
CREATE INDEX IF NOT EXISTS release_links_seen ON release_links(seen_at);
CREATE TABLE IF NOT EXISTS grabs (
    id TEXT PRIMARY KEY,
    media_id TEXT NOT NULL,
    target TEXT NOT NULL,
    plan_id TEXT,
    release_ref TEXT NOT NULL,
    info_hash TEXT NOT NULL,
    title TEXT NOT NULL,
    indexer_id INTEGER,
    size INTEGER,
    quality TEXT,
    revision_version INTEGER,
    revision_real INTEGER,
    full_season INTEGER NOT NULL DEFAULT 0,
    category TEXT NOT NULL,
    save_path TEXT NOT NULL,
    seed_ratio REAL,
    seed_time_minutes INTEGER,
    remove_completed INTEGER NOT NULL,
    remove_failed INTEGER NOT NULL,
    origin TEXT NOT NULL,
    payload_kind TEXT NOT NULL,
    payload BLOB NOT NULL,
    state TEXT NOT NULL,
    active INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT NOT NULL,
    lease_owner TEXT,
    lease_until TEXT,
    progress REAL,
    client_state TEXT,
    client_name TEXT,
    last_error TEXT,
    user_paused INTEGER NOT NULL DEFAULT 0,
    stalled_since TEXT,
    problem_since TEXT,
    absent_since TEXT,
    last_progress REAL,
    failure TEXT,
    retry TEXT,
    cleanup TEXT NOT NULL DEFAULT 'none',
    cleanup_detail TEXT,
    cleanup_files TEXT,
    cleanup_at TEXT,
    imported_files INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    submitted_at TEXT,
    completed_at TEXT,
    imported_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS grabs_active_hash ON grabs(info_hash) WHERE active = 1;
CREATE INDEX IF NOT EXISTS grabs_active ON grabs(active, state);
CREATE INDEX IF NOT EXISTS grabs_cleanup ON grabs(cleanup);
CREATE TABLE IF NOT EXISTS grab_targets (
    grab_id TEXT NOT NULL REFERENCES grabs(id),
    target TEXT NOT NULL,
    replaces INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (grab_id, target)
);
CREATE INDEX IF NOT EXISTS grab_targets_target ON grab_targets(target);
CREATE TABLE IF NOT EXISTS import_journal (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    grab_id TEXT NOT NULL REFERENCES grabs(id),
    source TEXT NOT NULL,
    root TEXT NOT NULL,
    dest TEXT NOT NULL,
    targets TEXT NOT NULL,
    size INTEGER NOT NULL,
    method TEXT,
    state TEXT NOT NULL,
    staged TEXT,
    staged_ino INTEGER,
    replaced_file_id INTEGER,
    replaced_path TEXT,
    backup TEXT,
    backup_ino INTEGER,
    quality TEXT,
    revision_version INTEGER,
    revision_real INTEGER,
    probe TEXT,
    error TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS import_journal_grab ON import_journal(grab_id, state);
CREATE TABLE IF NOT EXISTS blocklist (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    media_id TEXT NOT NULL,
    release_ref TEXT NOT NULL,
    info_hash TEXT,
    title TEXT NOT NULL,
    reason TEXT NOT NULL,
    grab_id TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (media_id, release_ref)
);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    kind TEXT NOT NULL,
    media_id TEXT,
    target TEXT,
    grab_id TEXT,
    message TEXT NOT NULL,
    data TEXT
);
CREATE INDEX IF NOT EXISTS events_media ON events(media_id, id);
CREATE TABLE IF NOT EXISTS tasks (
    name TEXT PRIMARY KEY,
    last_started TEXT,
    last_finished TEXT,
    last_status TEXT,
    last_message TEXT,
    next_run TEXT
);
CREATE TABLE IF NOT EXISTS commands (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    media_id TEXT,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    result TEXT,
    lease_owner TEXT,
    lease_until TEXT,
    attempts INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS commands_one_pending ON commands(name, COALESCE(media_id, '')) WHERE state IN ('queued', 'running');
CREATE TABLE IF NOT EXISTS rss_state (
    indexer_id INTEGER PRIMARY KEY,
    last_attempt_at TEXT,
    last_success_at TEXT,
    newest_pub TEXT,
    last_status TEXT,
    last_error TEXT,
    items INTEGER,
    gap_possible INTEGER NOT NULL DEFAULT 0,
    failures INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT
);
CREATE TABLE IF NOT EXISTS rss_seen (
    key TEXT PRIMARY KEY,
    indexer_id INTEGER NOT NULL,
    title TEXT NOT NULL,
    published_at TEXT,
    first_seen TEXT NOT NULL,
    last_seen TEXT NOT NULL,
    status TEXT NOT NULL,
    detail TEXT,
    grab_id TEXT
);
CREATE INDEX IF NOT EXISTS rss_seen_last ON rss_seen(last_seen);
CREATE TABLE IF NOT EXISTS requests (
    target TEXT PRIMARY KEY,
    media_id TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT NOT NULL,
    last_result TEXT,
    expires_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS requests_due ON requests(state, next_attempt_at);
CREATE TABLE IF NOT EXISTS roots (
    name TEXT PRIMARY KEY,
    local TEXT NOT NULL,
    status TEXT NOT NULL,
    scanned_at TEXT,
    files INTEGER,
    unmatched TEXT,
    detail TEXT
);
"#;
const SCHEMA_VERSION: &str = "3";

fn ensure_column(c: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let exists: bool = c
        .prepare(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
        ))?
        .exists([column])?;
    if !exists {
        c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

pub enum CreateOutcome {
    Created(Plan),
    Existing(Plan),
    IdempotencyConflict,
}

fn decode(json: String) -> Result<Plan> {
    serde_json::from_str(&json).context("stored plan is not valid JSON")
}

/// The plan `key` is bound to, with the fingerprint of the request that bound it.
fn bound(c: &Connection, key: &str) -> Result<Option<(Plan, String)>> {
    let row: Option<(String, String)> = c
        .query_row(
            "SELECT p.plan_json, k.fingerprint FROM idempotency_keys k JOIN plans p ON p.id = k.plan_id WHERE k.key = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(j, f)| Ok((decode(j)?, f))).transpose()
}

/// `Some` when `key` is already bound: its plan for the same request, a conflict otherwise.
fn check_key(c: &Connection, key: &str, fingerprint: &str) -> Result<Option<CreateOutcome>> {
    Ok(bound(c, key)?.map(|(plan, fp)| {
        if fp == fingerprint {
            CreateOutcome::Existing(plan)
        } else {
            CreateOutcome::IdempotencyConflict
        }
    }))
}

fn bind(c: &Connection, key: &str, fingerprint: &str, plan_id: &str) -> Result<()> {
    c.execute(
        "INSERT INTO idempotency_keys (key, fingerprint, plan_id, bound_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            key,
            fingerprint,
            plan_id,
            crate::import::rfc3339(time::OffsetDateTime::now_utc())
        ],
    )?;
    Ok(())
}

fn active_plan(c: &Connection, target: &str) -> Result<Option<Plan>> {
    c.query_row(
        "SELECT plan_json FROM plans WHERE target = ?1 AND active = 1",
        [target],
        |r| r.get(0),
    )
    .optional()?
    .map(decode)
    .transpose()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            crate::fsutil::write_new_private_file(path, b"")?;
        }
        crate::fsutil::check_private_file(path)?;
        let conn =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let mut conn = conn;
        match version.as_deref() {
            None => {
                let tx = conn.transaction()?;
                tx.execute_batch(BINDINGS)?;
                tx.execute_batch(EXECUTION)?;
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION],
                )?;
                tx.commit()?;
            }
            Some(v @ ("1" | "2")) => {
                // Migrate in place, in one transaction: keep every plan; schema 1 key
                // bindings move into the bindings table; schema 3 adds execution tables.
                let tx = conn.transaction()?;
                if v == "1" {
                    tx.execute_batch(BINDINGS)?;
                    tx.execute(
                        "INSERT INTO idempotency_keys (key, fingerprint, plan_id, bound_at)
                         SELECT idempotency_key, COALESCE(request_fingerprint, ''), id, created_at
                         FROM plans WHERE idempotency_key IS NOT NULL",
                        [],
                    )?;
                }
                tx.execute_batch(EXECUTION)?;
                tx.execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                    [SCHEMA_VERSION],
                )?;
                tx.commit()?;
            }
            Some(SCHEMA_VERSION) => {}
            Some(other) => bail!("state database schema {other} is not supported by this build"),
        }
        // Columns added within schema 3 after its first build (idempotent).
        ensure_column(&conn, "media", "monitor_new", "INTEGER NOT NULL DEFAULT 1")?;
        // A season whose flag was set only because new episodes are followed (not chosen).
        ensure_column(&conn, "seasons", "by_follow", "INTEGER NOT NULL DEFAULT 0")?;
        // When this server took over acquisition of an item (POST /library, or creation
        // through the API). NULL for items an import seeded.
        ensure_column(&conn, "media", "adopted_at", "TEXT")?;
        // The native monitored flag from the latest import, kept even after an adoption
        // clears dormant flags.
        ensure_column(&conn, "episodes", "native_monitored", "INTEGER")?;
        // Specials (season 0) the native app effectively monitors (latest import). They are
        // not acquired here and stay with the native app.
        ensure_column(
            &conn,
            "media",
            "native_specials",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        // Latest metadata numbering hold (JSON) while a refresh finds conflicts with the
        // imported numbering; cleared by a refresh without conflicts.
        ensure_column(&conn, "media", "metadata_hold", "TEXT")?;
        // Native handoffs an older release recorded, per catalog title (scope "" is the whole
        // title, "S" a season, "S:E" an episode). Kept so a rollback finds them; nothing
        // writes them now, and a release still removes them.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS handoffs (
                media_id TEXT NOT NULL,
                scope TEXT NOT NULL,
                confirmed_at TEXT NOT NULL,
                PRIMARY KEY (media_id, scope)
            );",
        )?;
        // The embedded download engine's own records, per torrent: identity and ownership as
        // the app assigned them, share limits, and lifetime transfer counters persisted here
        // (the engine's counters restart with every session and pause).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS embedded_torrents (
                info_hash TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                category TEXT NOT NULL,
                tags TEXT NOT NULL,
                save_path TEXT NOT NULL,
                output_folder TEXT NOT NULL,
                multi_file INTEGER NOT NULL,
                ratio_limit REAL NOT NULL,
                seeding_time_limit INTEGER NOT NULL,
                uploaded INTEGER NOT NULL DEFAULT 0 CHECK (uploaded >= 0),
                downloaded INTEGER NOT NULL DEFAULT 0 CHECK (downloaded >= 0),
                seeding_secs INTEGER NOT NULL DEFAULT 0 CHECK (seeding_secs >= 0),
                counters_valid INTEGER NOT NULL DEFAULT 1,
                completed_at TEXT,
                stopped_at_limit TEXT,
                verified_at TEXT,
                verify_error TEXT,
                verify_token TEXT,
                admitted_at TEXT,
                removed_at TEXT,
                added_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )?;
        // Plex bootstrap runs (resume cursor, counts, bounded issues), per media kind.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bootstrap_runs (
                media TEXT PRIMARY KEY,
                state TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )?;
        // Native indexer queries, counted for configured query limits (pruned after 2 days).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS indexer_usage (
                indexer_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS indexer_usage_at ON indexer_usage(indexer_id, kind, at);",
        )?;
        // Files a delete moved into the library's bin, kept so they can be put back until
        // `purge_at`.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bin_entries (
                delete_id TEXT PRIMARY KEY,
                media_id TEXT NOT NULL,
                title TEXT NOT NULL,
                media TEXT NOT NULL,
                files TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                deleted_at TEXT NOT NULL,
                purge_at TEXT NOT NULL
            );",
        )?;
        // Set when Arr's own delete moved the imported file to the bin; its download may then
        // be removed although the file is no longer in the library. Restoring clears it.
        ensure_column(&conn, "import_journal", "deleted_at", "TEXT")?;
        // Added within schema 3 after the table's first build (idempotent).
        // A full piece check binds to the run that started it.
        ensure_column(&conn, "embedded_torrents", "verify_token", "TEXT")?;
        // When the engine first accepted this torrent without overwriting anything (or
        // restored it from its own persistence). Only an admitted record may resume over
        // existing files; a record alone proves nothing.
        ensure_column(&conn, "embedded_torrents", "admitted_at", "TEXT")?;
        // Episodes whose idle or unstarted single-episode downloads a season pack takes over
        // when it is grabbed (JSON list of ids; older rows have none).
        ensure_column(
            &conn,
            "release_links",
            "supersedes",
            "TEXT NOT NULL DEFAULT '[]'",
        )?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs `f` on the single connection on a blocking thread (serialised by the mutex).
    pub async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut guard)
        })
        .await
        .context("database task failed")?
    }

    /// Runs `f` on the connection on the current thread. Only for blocking contexts
    /// (startup, `spawn_blocking` file operations).
    pub fn blocking<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    pub async fn ping(&self) -> Result<()> {
        self.with(|c| {
            Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                .map(|_| ())?)
        })
        .await
    }

    pub async fn get(&self, id: String) -> Result<Option<Plan>> {
        self.with(move |c| {
            c.query_row("SELECT plan_json FROM plans WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .map(decode)
            .transpose()
        })
        .await
    }

    /// Plan bound to `key`, with the fingerprint of the request that bound it. Bindings are
    /// durable: they survive restarts, cancellation and superseding.
    pub async fn by_idempotency_key(&self, key: String) -> Result<Option<(Plan, String)>> {
        self.with(move |c| bound(c, &key)).await
    }

    /// The active plan for `target`, binding a new idempotency key to it in the same
    /// transaction. `None` when no plan is active (nothing is bound then).
    pub async fn existing_active(
        &self,
        target: String,
        idempotency: Option<(String, String)>,
    ) -> Result<Option<CreateOutcome>> {
        self.with(move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some((key, fp)) = &idempotency
                && let Some(outcome) = check_key(&tx, key, fp)?
            {
                return Ok(Some(outcome));
            }
            let Some(plan) = active_plan(&tx, &target)? else {
                return Ok(None);
            };
            if let Some((key, fp)) = &idempotency {
                bind(&tx, key, fp, &plan.id)?;
            }
            tx.commit()?;
            Ok(Some(CreateOutcome::Existing(plan)))
        })
        .await
    }

    /// Atomically inserts `plan`. Returns the existing active plan instead unless `replace`,
    /// in which case the existing one is marked superseded in the same transaction. A new
    /// idempotency key is bound to whichever plan is returned.
    pub async fn create(
        &self,
        plan: Plan,
        idempotency: Option<(String, String)>,
        replace: bool,
    ) -> Result<CreateOutcome> {
        self.with(move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some((key, fp)) = &idempotency
                && let Some(outcome) = check_key(&tx, key, fp)?
            {
                return Ok(outcome);
            }
            let target = plan.target.to_string();
            if let Some(mut old) = active_plan(&tx, &target)? {
                if !replace {
                    // Lost a race with another request for the same target.
                    if let Some((key, fp)) = &idempotency {
                        bind(&tx, key, fp, &old.id)?;
                    }
                    tx.commit()?;
                    return Ok(CreateOutcome::Existing(old));
                }
                old.state = PlanState::Superseded;
                old.superseded_by = Some(plan.id.clone());
                old.updated_at = plan.created_at.clone();
                tx.execute(
                    "UPDATE plans SET active = 0, state = ?2, updated_at = ?3, plan_json = ?4 WHERE id = ?1",
                    params![old.id, old.state.as_str(), old.updated_at, serde_json::to_string(&old)?],
                )?;
            }
            tx.execute(
                "INSERT INTO plans (id, target, state, active, created_at, updated_at, plan_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    plan.id,
                    target,
                    plan.state.as_str(),
                    i64::from(plan.state.is_active()),
                    plan.created_at,
                    plan.updated_at,
                    serde_json::to_string(&plan)?
                ],
            )?;
            if let Some((key, fp)) = &idempotency {
                bind(&tx, key, fp, &plan.id)?;
            }
            tx.commit()?;
            Ok(CreateOutcome::Created(plan))
        })
        .await
    }

    pub async fn list(
        &self,
        state: Option<PlanState>,
        target: Option<String>,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<Plan>, u64)> {
        self.with(move |c| {
            let state = state.map(|s| s.as_str().to_string());
            let filter = "(?1 IS NULL OR state = ?1) AND (?2 IS NULL OR target = ?2)";
            let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM plans WHERE {filter}"), params![state, target], |r| r.get(0))?;
            let mut stmt = c.prepare(&format!(
                "SELECT plan_json FROM plans WHERE {filter} ORDER BY created_at DESC, id DESC LIMIT ?3 OFFSET ?4"
            ))?;
            let plans = stmt
                .query_map(params![state, target, limit, offset], |r| r.get::<_, String>(0))?
                .map(|j| decode(j?))
                .collect::<Result<Vec<_>>>()?;
            Ok((plans, total as u64))
        })
        .await
    }

    /// Rewrites a plan (execution results, imported state). Active only while unexecuted.
    pub async fn update_plan(&self, plan: Plan) -> Result<()> {
        self.with(move |c| {
            c.execute(
                "UPDATE plans SET state = ?2, active = ?3, updated_at = ?4, plan_json = ?5 WHERE id = ?1",
                params![plan.id, plan.state.as_str(), i64::from(plan.state.is_active()), plan.updated_at, serde_json::to_string(&plan)?],
            )?;
            Ok(())
        })
        .await
    }

    /// Cancels an active plan. `Ok(None)` = not found, `Err(state)` inside = not active.
    pub async fn cancel(
        &self,
        id: String,
        now: String,
    ) -> Result<Option<std::result::Result<Plan, PlanState>>> {
        self.with(move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let json: Option<String> = tx
                .query_row("SELECT plan_json FROM plans WHERE id = ?1", [&id], |r| r.get(0))
                .optional()?;
            let Some(json) = json else { return Ok(None) };
            let mut plan = decode(json)?;
            if !plan.state.is_active() {
                return Ok(Some(Err(plan.state)));
            }
            plan.state = PlanState::Cancelled;
            plan.updated_at = now;
            tx.execute(
                "UPDATE plans SET active = 0, state = ?2, updated_at = ?3, plan_json = ?4 WHERE id = ?1",
                params![plan.id, plan.state.as_str(), plan.updated_at, serde_json::to_string(&plan)?],
            )?;
            tx.commit()?;
            Ok(Some(Ok(plan)))
        })
        .await
    }
}
