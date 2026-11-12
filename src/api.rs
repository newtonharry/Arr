//! HTTP API (`/api/v1`). JSON only, bearer-token auth (except health and schema),
//! bounded bodies, bounded concurrency and a per-request deadline.

use crate::catalog::CatalogIndex;
use crate::client::Client;
use crate::config::RuntimeConfig;
use crate::decision::{Rejection, Scope};
use crate::downloads::{self, ActionError, CancelOptions, EnqueueError, EnqueueRequest, Removal};
use crate::embedded::Embedded;
use crate::error::{ApiError, ErrorCode};
use crate::ids::{MediaId, SeriesKey};
use crate::import::{self, rfc3339};
use crate::indexer::{Backend, Registry};
use crate::library::{self, MonitorMode};
use crate::metadata::{self, Movies, PlexMovies, Tmdb, Tvmaze};
use crate::plan::{self, Execution, ExecutionOutcome, Plan, PlanInputs, PlanState, UnitStatus};
use crate::plex::Plex;
use crate::prowlarr::ProwlarrClient;
use crate::qbit::Qbit;
use crate::search::{self, SearchInputs};
use crate::settings::{MediaKind, Settings};
use crate::store::{CreateOutcome, Store};
use crate::target::{Resolver, Target};
use crate::worker::{self, Worker};
use crate::{fsutil, openapi};
use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Semaphore;

mod options;
mod system;
mod titles;

/// Every route, for the OpenAPI coverage test: (method, path).
pub const ROUTES: &[(&str, &str)] = &[
    ("get", "/api/v1/health"),
    ("get", "/api/v1/openapi.json"),
    ("get", "/api/v1/readiness"),
    ("get", "/api/v1/capabilities"),
    ("get", "/api/v1/diagnostics"),
    ("get", "/api/v1/settings"),
    ("get", "/api/v1/migration-report"),
    ("get", "/api/v1/library/movies"),
    ("get", "/api/v1/library/series"),
    ("post", "/api/v1/library"),
    ("patch", "/api/v1/library/{media_id}"),
    ("delete", "/api/v1/library/{media_id}"),
    ("get", "/api/v1/library/roots"),
    ("get", "/api/v1/storage"),
    ("get", "/api/v1/wanted"),
    ("get", "/api/v1/calendar"),
    ("get", "/api/v1/discover"),
    ("get", "/api/v1/people"),
    ("get", "/api/v1/people/{person_id}"),
    ("get", "/api/v1/media/{media_id}"),
    ("get", "/api/v1/media/{media_id}/releases"),
    ("post", "/api/v1/plans"),
    ("get", "/api/v1/plans"),
    ("get", "/api/v1/plans/{plan_id}"),
    ("post", "/api/v1/plans/{plan_id}/cancel"),
    ("post", "/api/v1/plans/{plan_id}/execute"),
    ("post", "/api/v1/grabs"),
    ("get", "/api/v1/queue"),
    ("get", "/api/v1/queue/{grab_id}"),
    ("post", "/api/v1/queue/{grab_id}/pause"),
    ("post", "/api/v1/queue/{grab_id}/resume"),
    ("post", "/api/v1/queue/{grab_id}/cancel"),
    ("post", "/api/v1/queue/{grab_id}/retry"),
    ("get", "/api/v1/history"),
    ("get", "/api/v1/blocklist"),
    ("delete", "/api/v1/blocklist/{entry_id}"),
    ("post", "/api/v1/commands"),
    ("get", "/api/v1/commands"),
    ("get", "/api/v1/commands/{command_id}"),
    ("get", "/api/v1/system/tasks"),
    ("get", "/api/v1/requests"),
    ("get", "/api/v1/ownership"),
    ("get", "/api/v1/status/{media_id}"),
    ("get", "/api/v1/attention"),
    ("get", "/api/v1/feed"),
    ("get", "/api/v1/ideas"),
    ("post", "/api/v1/ideas/hide"),
    ("delete", "/api/v1/ideas/hide/{media_id}"),
    ("post", "/api/v1/quick-add"),
    ("post", "/api/v1/follow"),
    ("post", "/api/v1/stop"),
    ("post", "/api/v1/remove"),
    ("post", "/api/v1/quality"),
    ("get", "/api/v1/copies/{media_id}"),
    ("post", "/api/v1/copies/{media_id}/get"),
    ("get", "/api/v1/show/{media_id}"),
    ("get", "/api/v1/show/{media_id}/season/{n}"),
    ("get", "/api/v1/movie/{media_id}"),
    ("get", "/api/v1/rss"),
    ("get", "/api/v1/update"),
    ("post", "/api/v1/update"),
    ("get", "/api/v1/settings/app"),
    ("post", "/api/v1/settings/app"),
    ("get", "/api/v1/system/health"),
    ("get", "/api/v1/setup"),
    ("post", "/api/v1/setup"),
    ("get", "/api/v1/setup/plex"),
    ("get", "/api/v1/setup/plex/bootstrap"),
    ("post", "/api/v1/setup/plex/bootstrap"),
];

pub struct Loaded {
    pub import_id: String,
    pub observed_at: String,
    pub imported_at: String,
    pub settings: Settings,
    pub report: Value,
    /// Always `managed_setup` (state made by the removed Sonarr/Radarr importer is refused at
    /// startup); kept so API clients that read `origin` keep working.
    pub origin: &'static str,
}

impl From<import::LoadedImport> for Loaded {
    fn from(l: import::LoadedImport) -> Self {
        Loaded {
            origin: "managed_setup",
            import_id: l.manifest.import_id,
            observed_at: l.manifest.observed_at,
            imported_at: l.manifest.imported_at,
            settings: {
                let mut settings = l.settings;
                crate::setup::with_levels(&mut settings);
                settings
            },
            report: l.report,
        }
    }
}

pub struct AppState {
    pub cfg: RuntimeConfig,
    /// The active settings import (made by managed setup). Replaced only
    /// by `activate`, under `setup_lock`; readers take an `Arc` snapshot per operation.
    active: RwLock<Option<Arc<Loaded>>>,
    pub store: Store,
    /// The release search backend: native indexers or Prowlarr (never both, no fallback).
    pub search: Option<Backend>,
    pub tvmaze: Option<Arc<Tvmaze>>,
    /// Movie metadata: Plex first, TMDB (when it has a key) as the backup.
    pub movies: Option<Movies>,
    /// The download client ([qbittorrent] or [embedded]); none in shadow mode.
    pub client: Option<Arc<Client>>,
    pub plex: Option<Plex>,
    pub worker: Worker,
    index: RwLock<Arc<CatalogIndex>>,
    plans_lock: tokio::sync::Mutex<()>,
    /// Serialises library mutations (add, PATCH, DELETE): each one checks and changes
    /// monitoring against the state the previous one left.
    library_lock: tokio::sync::Mutex<()>,
    /// Serialises managed setup (validation against the active settings and activation).
    pub(crate) setup_lock: tokio::sync::Mutex<()>,
    slots: Arc<Semaphore>,
    started_at: OffsetDateTime,
    /// Release check and "Update now" (see `selfupdate.rs`).
    pub updates: crate::selfupdate::Updates,
    /// Ends the process so the service manager starts it again (a changed setting).
    pub restart: crate::lifecycle::Restart,
    /// One setting is changed at a time (each edits the config file).
    settings_lock: tokio::sync::Mutex<()>,
}

type St = State<Arc<AppState>>;
type ApiResult = Result<Response, ApiError>;

/// Outcome of a search-and-grab for one target.
pub struct Acquired {
    pub plan: Plan,
    pub created: bool,
}

/// Binds the state to the download backend it first runs with. Grabs record torrents of one
/// client; switching the backend over the same state would reinterpret them, so it is refused
/// (a different backend needs its own state directory). A state with grabs from before the
/// binding existed was qBittorrent's.
fn bind_backend(c: &mut rusqlite::Connection, backend: &str) -> anyhow::Result<()> {
    use rusqlite::OptionalExtension;
    let bound: Option<String> = c
        .query_row(
            "SELECT value FROM meta WHERE key = 'download_backend'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let bound = match bound {
        Some(b) => b,
        None => {
            let grabs: i64 = c.query_row("SELECT COUNT(*) FROM grabs", [], |r| r.get(0))?;
            let b = if grabs > 0 { "qbittorrent" } else { backend };
            c.execute(
                "INSERT INTO meta (key, value) VALUES ('download_backend', ?1)",
                [b],
            )?;
            b.to_string()
        }
    };
    if bound != backend {
        anyhow::bail!(
            "this state directory is bound to the {bound} download backend, but [{backend}] is configured; its downloads are never reinterpreted by another backend. Use a fresh state directory for {backend}, or restore the [{bound}] configuration"
        );
    }
    Ok(())
}

/// The embedded engine; with the `test-client` feature and `[test_client]` configured, the
/// in-memory stand-in (tests only).
#[cfg_attr(not(feature = "test-client"), allow(unused_variables))]
fn embedded_client(
    cfg: &RuntimeConfig,
    e: &crate::config::EmbeddedConfig,
    store: &Store,
) -> Client {
    #[cfg(feature = "test-client")]
    if let Some(id) = &cfg.test_client {
        return Client::Fake(crate::client::fake::Fake::attach(id, e));
    }
    Client::Embedded(Embedded::new(e.clone(), store.clone()))
}

impl AppState {
    pub fn new(cfg: RuntimeConfig) -> anyhow::Result<Self> {
        let active = import::load_active(&cfg.state_dir)?;
        if let Some(l) = &active
            && !l.manifest.files.contains_key(crate::setup::SOURCE_FILE)
        {
            anyhow::bail!(
                "the state folder {} holds a Sonarr/Radarr snapshot import (made by `arr import`), which Arr no longer supports; nothing in it was changed. Set up a new state folder with `arr setup` and `arr bootstrap` (see docs/STANDALONE.md)",
                cfg.state_dir.display()
            );
        }
        let store = Store::open(&cfg.state_dir.join("state.sqlite"))?;
        if let Some(backend) = cfg.download_backend() {
            store.blocking(|c| bind_backend(c, backend))?;
        }
        let now = OffsetDateTime::now_utc();
        let index = store.blocking(|c| library::load_index(c))?;
        let loaded = active.map(|l| Arc::new(Loaded::from(l)));
        let search = if !cfg.indexers.is_empty() {
            Some(Backend::Native(Arc::new(Registry::new(
                &cfg.indexers,
                store.clone(),
            ))))
        } else {
            cfg.prowlarr
                .clone()
                .map(|p| Backend::Prowlarr(Arc::new(ProwlarrClient::new(p))))
        };
        Ok(AppState {
            search,
            tvmaze: cfg.tvmaze.as_ref().map(|c| Arc::new(Tvmaze::new(c))),
            movies: Movies::new(
                cfg.plex.as_ref().and_then(|p| {
                    crate::plexsignin::client_id(&cfg.state_dir)
                        .ok()
                        .map(|id| PlexMovies::new(p, id))
                }),
                cfg.tmdb.as_ref().and_then(Tmdb::new),
            ),
            client: match (&cfg.qbittorrent, &cfg.embedded) {
                (Some(q), _) => Some(Arc::new(Client::Qbit(Qbit::new(q.clone())))),
                // The engine runs only with execution enabled: in shadow mode nothing may open
                // its listener or resume saved transfers, so there is no client at all.
                (None, Some(e)) if cfg.execution_enabled => {
                    Some(Arc::new(embedded_client(&cfg, e, &store)))
                }
                (None, _) => None,
            },
            plex: cfg.plex.clone().map(Plex::new),
            worker: Worker::new(),
            index: RwLock::new(Arc::new(index)),
            plans_lock: tokio::sync::Mutex::new(()),
            library_lock: tokio::sync::Mutex::new(()),
            setup_lock: tokio::sync::Mutex::new(()),
            slots: Arc::new(Semaphore::new(cfg.max_concurrent_requests)),
            started_at: now,
            updates: crate::selfupdate::Updates::new(&cfg.state_dir),
            restart: crate::lifecycle::Restart::default(),
            settings_lock: tokio::sync::Mutex::new(()),
            active: RwLock::new(loaded),
            store,
            cfg,
        })
    }

    /// The active settings import, if any (a snapshot for one operation).
    pub fn current(&self) -> Option<Arc<Loaded>> {
        self.active
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Makes a verified import (written by managed setup) the active one in this process by
    /// swapping the settings. The caller holds `setup_lock` and has activated it on disk.
    pub(crate) async fn activate(&self, l: import::LoadedImport) -> anyhow::Result<()> {
        *self.active.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(Loaded::from(l)));
        self.reload_index().await?;
        self.worker.notify.notify_one();
        Ok(())
    }

    /// The lock serialising library mutations.
    pub(crate) fn library_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.library_lock
    }

    /// The provider label for errors about a release from `indexer_id`.
    pub fn provider_label(&self, indexer_id: Option<i64>) -> String {
        self.search
            .as_ref()
            .map_or_else(|| "indexer".into(), |b| b.label(indexer_id))
    }

    /// Radarr `availabilityDelay` from the active import (days).
    pub fn availability_delay_days(&self) -> i64 {
        self.current()
            .and_then(|l| {
                l.settings
                    .policy(MediaKind::Movie)
                    .map(|p| p.availability_delay_days)
            })
            .unwrap_or(0)
    }

    /// The managed catalog index (current as of the last mutation).
    pub fn index(&self) -> Arc<CatalogIndex> {
        self.index.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Rebuilds the index from the managed catalog tables.
    pub async fn reload_index(&self) -> anyhow::Result<()> {
        let idx = self.store.with(|c| library::load_index(c)).await?;
        *self.index.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(idx);
        Ok(())
    }

    fn loaded(&self) -> Result<Arc<Loaded>, ApiError> {
        self.current().ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::CatalogNotImported,
                "no settings are active: create managed settings (`arr setup`, or POST /api/v1/setup)",
            )
        })
    }

    fn resolver<'a>(&'a self, index: &'a CatalogIndex, now: OffsetDateTime) -> Resolver<'a> {
        Resolver {
            catalog: Some(index),
            tvmaze: self.tvmaze.as_deref(),
            movies: self.movies.as_ref(),
            now,
            availability_delay_days: self.availability_delay_days(),
        }
    }

    fn search_inputs<'a>(
        &'a self,
        loaded: &'a Loaded,
        index: &'a CatalogIndex,
        now: OffsetDateTime,
        automatic: bool,
        refresh: bool,
        blocked: Option<&'a HashSet<String>>,
    ) -> SearchInputs<'a> {
        SearchInputs {
            backend: self.search.as_ref(),
            settings: &loaded.settings,
            catalog: Some(index),
            catalog_observed_at: Some(loaded.observed_at.clone()),
            now,
            automatic,
            refresh,
            blocked,
            partial_packs: self.can_select_files(),
        }
    }

    /// Whether the download client can fetch some files of a torrent: the embedded engine
    /// can. qBittorrent is not driven that way, and with no client nothing is grabbed, so a
    /// season pack must cover every episode there (the behaviour before file selection).
    pub fn can_select_files(&self) -> bool {
        self.client
            .as_deref()
            .is_some_and(|c| c.embedded_semantics())
    }

    /// Search-and-grab for one target: a fresh plan (superseding an active one), executed
    /// when `execute` and execution is enabled. Explicit requests that find nothing are
    /// remembered and retried with backoff.
    pub async fn acquire(
        &self,
        requested: MediaId,
        revisions: bool,
        execute: bool,
        origin: &'static str,
        now: OffsetDateTime,
    ) -> Result<Acquired, ApiError> {
        let mut last = self
            .acquire_once(requested, revisions, execute, origin, now)
            .await?;
        // A candidate whose payload contradicted the item was blocklisted at submission:
        // search again for a valid alternative (bounded).
        for _ in 0..3 {
            if !(execute && self.cfg.execution_enabled && refused_wrong_payload(&last.plan)) {
                break;
            }
            last = self
                .acquire_once(requested, revisions, execute, origin, now)
                .await?;
        }
        Ok(last)
    }

    async fn acquire_once(
        &self,
        requested: MediaId,
        revisions: bool,
        execute: bool,
        origin: &'static str,
        now: OffsetDateTime,
    ) -> Result<Acquired, ApiError> {
        let loaded = self.loaded()?;
        let index = self.index();
        let target = self.resolver(&index, now).target(requested).await?;
        if execute
            && self.cfg.execution_enabled
            && let Some(why) = downloads::not_adopted(self, &[plan::canonical_id(&target)])
        {
            return Err(ApiError::conflict(ErrorCode::NotAdopted, why));
        }
        let profile = search::profile_for(&loaded.settings, &target, None)?;
        let media = target_media(&target);
        let blocked = self
            .store
            .with(move |c| downloads::blocked_for(c, media))
            .await
            .map_err(internal("blocklist"))?;
        // Automatic searches always query the indexers (no cached result).
        let inputs = PlanInputs {
            search: self.search_inputs(&loaded, &index, now, true, true, Some(&blocked)),
            import_id: Some(loaded.import_id.clone()),
            revisions,
            execution_enabled: self.cfg.execution_enabled,
            episode_deadline: tokio::time::Instant::now() + self.cfg.request_timeout.mul_f64(0.75),
        };
        let built = plan::build(&inputs, &target, requested, profile).await?;
        let links = built.links;
        self.store
            .with(move |c| downloads::save_links(c, &links, now))
            .await
            .map_err(internal("links"))?;
        let plan = match self
            .store
            .create(built.plan, None, true)
            .await
            .map_err(internal("plan insert"))?
        {
            CreateOutcome::Created(p) | CreateOutcome::Existing(p) => p,
            CreateOutcome::IdempotencyConflict => return Err(key_reused()),
        };
        let plan = if execute
            && self.cfg.execution_enabled
            && matches!(
                plan.state,
                PlanState::ShadowPlanned | PlanState::ShadowPartial
            ) {
            self.execute_plan(&plan.id, origin).await?
        } else {
            plan
        };
        if execute && self.cfg.execution_enabled {
            self.track_request(&plan, now)
                .await
                .map_err(internal("request"))?;
        }
        Ok(Acquired {
            plan,
            created: true,
        })
    }

    async fn track_request(&self, plan: &Plan, now: OffsetDateTime) -> anyhow::Result<()> {
        // A movie that is not available yet stays requested (retried with backoff), and so
        // does a request whose execution refused a candidate for a planned unit that no
        // active download covers (whether some or none of its candidates were queued).
        let waiting = plan.blockers.iter().any(|b| b.code == "not_available");
        let refused_uncovered = if plan.execution.refused.is_empty() {
            false
        } else {
            let planned: Vec<String> = plan
                .units
                .iter()
                .filter(|u| u.status == UnitStatus::Planned)
                .map(|u| u.target.to_string())
                .collect();
            self.store
                .with(move |c| downloads::any_uncovered(c, &planned))
                .await?
        };
        let open = waiting
            || refused_uncovered
            || plan.units.iter().any(|u| {
                matches!(
                    u.status,
                    UnitStatus::NoApprovedRelease
                        | UnitStatus::NoReleaseObserved
                        | UnitStatus::SearchFailed
                        | UnitStatus::NotSearched
                )
            });
        let (target, media) = (plan.target, plan_media(plan.target));
        let summary = if refused_uncovered {
            format!(
                "{}: {} ({})",
                plan.state.as_str(),
                if plan.execution.nothing_queued() {
                    "nothing queued"
                } else {
                    "partially queued"
                },
                plan.execution
                    .refused
                    .iter()
                    .map(|r| r.code.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "{}: {}",
                plan.state.as_str(),
                plan.blockers
                    .iter()
                    .map(|b| b.code.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let (retry, expiry) = (
            self.cfg.automation.request_retry,
            self.cfg.automation.request_expiry,
        );
        self.store
            .with(move |c| {
                if open {
                    worker::remember_request(c, target, media, &summary, now, retry, expiry)
                } else {
                    worker::settle_request(c, target, "satisfied", &summary, now)
                }
            })
            .await
    }

    /// Executes a stored shadow plan: each planned candidate goes through the outbox.
    pub async fn execute_plan(
        &self,
        plan_id: &str,
        origin: &'static str,
    ) -> Result<Plan, ApiError> {
        let _guard = self.plans_lock.lock().await;
        let mut plan = self
            .store
            .get(plan_id.to_string())
            .await
            .map_err(internal("plan get"))?
            .ok_or_else(|| ApiError::not_found("no such plan"))?;
        if let Some((code, msg)) =
            downloads::execution_blocker(self, plan_media(plan.target).kind())
        {
            return Err(ApiError::conflict(refusal_code(code), msg));
        }
        if let Some(why) = downloads::not_adopted(self, &[plan.target]) {
            return Err(ApiError::conflict(ErrorCode::NotAdopted, why));
        }
        if !matches!(
            plan.state,
            PlanState::ShadowPlanned | PlanState::ShadowPartial
        ) {
            return Err(ApiError::conflict(
                ErrorCode::PlanNotActive,
                format!(
                    "plan is {}; only shadow_planned or shadow_partial plans can be executed",
                    plan.state.as_str()
                ),
            ));
        }
        let now = OffsetDateTime::now_utc();
        let mut grabs = vec![];
        let mut replaced: Vec<String> = vec![];
        let mut refused = vec![];
        for c in plan.candidates.clone() {
            let (r, t) = (c.release_ref.clone(), plan.target);
            let link = self
                .store
                .with(move |conn| downloads::load_link(conn, &r, Some(t)))
                .await
                .map_err(internal("link"))?;
            let provider = self.provider_label(link.as_ref().and_then(|l| l.indexer_id));
            let Some(link) = link else {
                refused.push(Rejection {
                    code: "release_not_found".into(),
                    message: format!(
                        "{}: link material expired; create a new plan",
                        c.release_ref
                    ),
                });
                continue;
            };
            match downloads::enqueue(
                self,
                EnqueueRequest {
                    link,
                    plan_id: Some(plan.id.clone()),
                    origin,
                },
            )
            .await
            {
                Ok(g) => {
                    replaced.extend(g.replaced.iter().cloned());
                    grabs.push(g.id);
                }
                Err(EnqueueError::Refused(code, msg)) => refused.push(Rejection {
                    code: code.into(),
                    message: format!("{}: {msg}", c.title),
                }),
                Err(EnqueueError::Upstream(e)) => refused.push(Rejection {
                    code: "payload_fetch_failed".into(),
                    message: format!("{}: {provider} {}", c.title, e.describe()),
                }),
                Err(EnqueueError::Internal(e)) => {
                    return Err(ApiError::internal("enqueue", format!("{e:#}")));
                }
            }
        }
        let queued = !grabs.is_empty();
        let outcome = match (queued, refused.is_empty()) {
            (true, true) => ExecutionOutcome::Queued,
            (true, false) => ExecutionOutcome::PartiallyQueued,
            (false, _) => ExecutionOutcome::NothingQueued,
        };
        plan.execution = Execution {
            mode: "executed".into(),
            download_submission: "enabled".into(),
            // Entering the outbox is not a client action: `submitted` turns true only when
            // qBittorrent confirms a grab (worker plan sync).
            submitted: false,
            note: if queued {
                format!(
                    "executed: {} grab(s) queued; waiting for {} to confirm them",
                    grabs.len(),
                    if self
                        .client
                        .as_deref()
                        .is_some_and(|c| c.embedded().is_some())
                    {
                        "the embedded engine"
                    } else {
                        "qBittorrent"
                    }
                )
            } else {
                "executed: nothing was queued (see refused)".into()
            },
            grabs,
            refused,
            executed_at: Some(rfc3339(now)),
            confirmed_at: None,
            outcome: Some(outcome),
            superseded_grabs: replaced,
        };
        if queued {
            plan.state = PlanState::DownloadQueued;
        }
        plan.updated_at = rfc3339(now);
        self.store
            .update_plan(plan.clone())
            .await
            .map_err(internal("plan update"))?;
        Ok(plan)
    }
}

fn refused_wrong_payload(plan: &Plan) -> bool {
    plan.execution
        .refused
        .iter()
        .any(|r| r.code == "wrong_payload")
}

fn target_media(t: &Target) -> MediaId {
    match t {
        Target::Movie(m) => m.id,
        Target::Season(s) | Target::Episode { season: s, .. } => s.series.id,
    }
}

fn plan_media(target: MediaId) -> MediaId {
    match target {
        MediaId::Season(k, _) | MediaId::Episode(k, _, _) => MediaId::Series(k),
        other => other,
    }
}

trait Kind {
    fn kind(self) -> MediaKind;
}

impl Kind for MediaId {
    fn kind(self) -> MediaKind {
        downloads::media_kind(self)
    }
}

fn refusal_code(code: &str) -> ErrorCode {
    match code {
        "execution_disabled" => ErrorCode::ExecutionDisabled,
        "catalog_not_imported" => ErrorCode::CatalogNotImported,
        "client_not_configured" => ErrorCode::ClientNotConfigured,
        "import_unsupported" => ErrorCode::ImportUnsupported,
        "covers_queued" => ErrorCode::AlreadyQueued,
        "blocklisted" => ErrorCode::Blocklisted,
        "duplicate_torrent" => ErrorCode::DuplicateTorrent,
        "invalid_payload" => ErrorCode::InvalidPayload,
        "no_download_link" => ErrorCode::NoDownloadLink,
        "provider_not_configured" => ErrorCode::ProviderNotConfigured,
        "grab_state" => ErrorCode::GrabState,
        "seed_criteria_unmet" => ErrorCode::SeedCriteriaUnmet,
        "cleanup_blocked" => ErrorCode::CleanupBlocked,
        "foreign_torrent" => ErrorCode::ForeignTorrent,
        "wrong_payload" => ErrorCode::WrongPayload,
        "not_adopted" => ErrorCode::NotAdopted,
        "indexer_limit_reached" => ErrorCode::IndexerLimitReached,
        "no_peer_discovery" => ErrorCode::NoPeerDiscovery,
        "quality_changed" => ErrorCode::QualityChanged,
        _ => ErrorCode::InvalidRequest,
    }
}

fn action_error(e: ActionError, backend: &str) -> ApiError {
    match e {
        ActionError::NotFound => ApiError::not_found("no such download"),
        ActionError::Conflict(code, msg) => ApiError::conflict(refusal_code(code), msg),
        ActionError::Upstream(e) => e.into_api(backend),
        ActionError::Internal(e) => ApiError::internal("download action", format!("{e:#}")),
    }
}

fn enqueue_error(e: EnqueueError, provider: &str) -> ApiError {
    match e {
        EnqueueError::Refused(code, msg) => ApiError::conflict(refusal_code(code), msg),
        EnqueueError::Upstream(e) => e.into_api(provider),
        EnqueueError::Internal(e) => ApiError::internal("enqueue", format!("{e:#}")),
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/api/v1/readiness", get(readiness))
        .route("/api/v1/capabilities", get(capabilities))
        .route("/api/v1/diagnostics", get(diagnostics))
        .route("/api/v1/settings", get(settings))
        .route("/api/v1/migration-report", get(migration_report))
        .route("/api/v1/library", post(library_add))
        .route("/api/v1/library/movies", get(library_movies))
        .route("/api/v1/library/series", get(library_series))
        .route("/api/v1/library/roots", get(library_roots))
        .route("/api/v1/storage", get(storage))
        .route(
            "/api/v1/library/{media_id}",
            axum::routing::patch(library_patch).delete(library_remove),
        )
        .route("/api/v1/wanted", get(wanted))
        .route("/api/v1/calendar", get(calendar))
        .route("/api/v1/discover", get(discover))
        .route("/api/v1/people", get(people))
        .route("/api/v1/people/{person_id}", get(person))
        .route("/api/v1/media/{media_id}", get(media))
        .route("/api/v1/media/{media_id}/releases", get(releases))
        .route("/api/v1/plans", post(create_plan).get(list_plans))
        .route("/api/v1/plans/{plan_id}", get(get_plan))
        .route("/api/v1/plans/{plan_id}/cancel", post(cancel_plan))
        .route("/api/v1/plans/{plan_id}/execute", post(execute_plan))
        .route("/api/v1/grabs", post(grab))
        .route("/api/v1/queue", get(queue))
        .route("/api/v1/queue/{grab_id}", get(queue_item))
        .route("/api/v1/queue/{grab_id}/pause", post(queue_pause))
        .route("/api/v1/queue/{grab_id}/resume", post(queue_resume))
        .route("/api/v1/queue/{grab_id}/cancel", post(queue_cancel))
        .route("/api/v1/queue/{grab_id}/retry", post(queue_retry))
        .route("/api/v1/history", get(history))
        .route("/api/v1/blocklist", get(blocklist))
        .route("/api/v1/blocklist/{entry_id}", delete(blocklist_remove))
        .route("/api/v1/commands", post(command_create).get(command_list))
        .route("/api/v1/commands/{command_id}", get(command_get))
        .route("/api/v1/system/tasks", get(system_tasks))
        .route("/api/v1/requests", get(requests))
        .route("/api/v1/ownership", get(ownership))
        .route("/api/v1/status/{media_id}", get(media_status))
        .route("/api/v1/attention", get(attention))
        .route("/api/v1/feed", get(feed))
        .route("/api/v1/ideas", get(ideas))
        .route("/api/v1/ideas/hide", post(ideas_hide))
        .route("/api/v1/ideas/hide/{media_id}", delete(ideas_unhide))
        .route("/api/v1/quick-add", post(quick_add))
        .route("/api/v1/follow", post(titles::follow))
        .route("/api/v1/stop", post(titles::stop))
        .route("/api/v1/remove", post(titles::remove))
        .route("/api/v1/quality", post(options::quality))
        .route("/api/v1/copies/{media_id}", get(options::copies))
        .route("/api/v1/copies/{media_id}/get", post(options::copies_get))
        .route("/api/v1/show/{media_id}", get(titles::show))
        .route("/api/v1/show/{media_id}/season/{n}", get(titles::season))
        .route("/api/v1/movie/{media_id}", get(titles::movie))
        .route("/api/v1/rss", get(rss_status))
        .route(
            "/api/v1/update",
            get(system::update_get).post(system::update_post),
        )
        .route(
            "/api/v1/settings/app",
            get(system::app_settings_get).post(system::app_settings_post),
        )
        .route("/api/v1/system/health", get(system::system_health))
        .route("/api/v1/setup", get(setup_status).post(setup_apply))
        .route("/api/v1/setup/plex", get(setup_plex))
        .route(
            "/api/v1/setup/plex/bootstrap",
            get(bootstrap_status).post(bootstrap_batch),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route("/api/v1/health", get(health))
        .route(
            "/api/v1/openapi.json",
            get(|| async { Json(openapi::document()) }),
        )
        .merge(protected)
        .fallback(|| async { ApiError::not_found("no such route") })
        .method_not_allowed_fallback(|| async {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                ErrorCode::MethodNotAllowed,
                "method not allowed for this route",
            )
        })
        .layer(DefaultBodyLimit::max(state.cfg.max_body_bytes))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Load shedding, request deadline, request id, no-store headers, access log (path only).
/// Liveness (`/api/v1/health`) is exempt from shedding so a busy server is not seen as dead.
async fn guard(State(st): St, req: Request, next: Next) -> Response {
    let started = Instant::now();
    let request_id = fsutil::random_hex(8).unwrap_or_else(|_| "unknown".into());
    let method = req.method().clone();
    let path = req.uri().path().chars().take(200).collect::<String>();
    let permit = if path == "/api/v1/health" {
        Ok(None)
    } else {
        st.slots.clone().try_acquire_owned().map(Some)
    };
    let mut resp = match permit {
        Err(_) => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Overloaded,
            "too many concurrent requests; retry shortly",
        )
        .into_response(),
        Ok(_permit) => match tokio::time::timeout(st.cfg.request_timeout, next.run(req)).await {
            Ok(r) => r,
            Err(_) => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::RequestTimeout,
                "request exceeded the server deadline",
            )
            .into_response(),
        },
    };
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        h.insert("x-request-id", v);
    }
    tracing::info!(request_id, %method, path, status = resp.status().as_u16(), elapsed_ms = started.elapsed().as_millis() as u64, "request");
    resp
}

async fn auth(State(st): St, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let ok = presented
        .is_some_and(|t| bool::from(t.as_bytes().ct_eq(st.cfg.api_token.expose().as_bytes())));
    if !ok {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            "missing or invalid bearer token",
        )
        .into_response();
    }
    next.run(req).await
}

// ---------------------------------------------------------------- extractors

pub struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(rej) => Err(json_rejection(rej)),
        }
    }
}

fn json_rejection(rej: JsonRejection) -> ApiError {
    let status = rej.status();
    let code = match status {
        StatusCode::PAYLOAD_TOO_LARGE => ErrorCode::PayloadTooLarge,
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ErrorCode::UnsupportedMediaType,
        StatusCode::UNPROCESSABLE_ENTITY => ErrorCode::InvalidBody,
        _ => ErrorCode::InvalidJson,
    };
    let message = match code {
        ErrorCode::PayloadTooLarge => "request body exceeds the configured limit".to_string(),
        ErrorCode::UnsupportedMediaType => "expected Content-Type: application/json".to_string(),
        _ => rej.body_text(),
    };
    ApiError::new(status, code, message)
}

/// Optional JSON body: empty = defaults.
fn optional_body<T: DeserializeOwned + Default>(b: &Bytes) -> Result<T, ApiError> {
    if b.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(b).map_err(|e| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidBody,
            format!("invalid body: {e}"),
        )
    })
}

pub struct ApiQuery<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for ApiQuery<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(v)| ApiQuery(v))
            .map_err(|e: QueryRejection| {
                ApiError::bad_request(ErrorCode::InvalidQuery, e.body_text())
            })
    }
}

fn parse_media_id(raw: &str) -> Result<MediaId, ApiError> {
    raw.parse().map_err(|e: crate::ids::MediaIdError| {
        ApiError::bad_request(ErrorCode::InvalidMediaId, e.0)
    })
}

fn media_id(p: Result<Path<String>, PathRejection>) -> Result<MediaId, ApiError> {
    let Path(raw) =
        p.map_err(|e| ApiError::bad_request(ErrorCode::InvalidMediaId, e.body_text()))?;
    parse_media_id(&raw)
}

fn prefixed_id(
    p: Result<Path<String>, PathRejection>,
    prefix: &str,
    what: &str,
) -> Result<String, ApiError> {
    let Path(raw) =
        p.map_err(|e| ApiError::bad_request(ErrorCode::InvalidRequest, e.body_text()))?;
    let ok = raw.len() == prefix.len() + 24
        && raw.starts_with(prefix)
        && raw[prefix.len()..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if !ok {
        return Err(ApiError::not_found(format!("no such {what}")));
    }
    Ok(raw)
}

fn plan_id(p: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    prefixed_id(p, "pln_", "plan")
}

fn ok(v: Value) -> ApiResult {
    Ok(Json(v).into_response())
}

fn created(v: Value) -> ApiResult {
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

fn internal(context: &str) -> impl FnOnce(anyhow::Error) -> ApiError + '_ {
    move |e| ApiError::internal(context, format!("{e:#}"))
}

// ---------------------------------------------------------------- status

fn mode(st: &AppState) -> &'static str {
    if st.cfg.execution_enabled {
        "execution"
    } else {
        "shadow"
    }
}

async fn health(State(st): St) -> Json<Value> {
    Json(
        json!({ "status": "ok", "service": "arr", "version": env!("CARGO_PKG_VERSION"), "mode": mode(&st) }),
    )
}

fn age_hours(observed_at: &str, now: OffsetDateTime) -> Option<f64> {
    OffsetDateTime::parse(observed_at, &Rfc3339)
        .ok()
        .map(|t| ((now - t).whole_minutes() as f64 / 60.0 * 10.0).round() / 10.0)
}

/// One row of the capability matrix.
struct Cap {
    name: &'static str,
    implemented: bool,
    configured: bool,
    /// Built-in verification of this build: `unit_tests` or `loopback_mocks`.
    tested: &'static str,
    detail: String,
}

fn capability_rows(st: &AppState) -> Vec<Cap> {
    let imported = st.current().is_some();
    let exec = st.cfg.execution_enabled;
    let qb = st.client.is_some();
    let embedded = st.client.as_deref().is_some_and(Client::embedded_semantics);
    // Client-dependent capabilities carry the backend in their name, so evidence recorded
    // for one backend never counts for the other.
    let name = |q: &'static str, e: &'static str| if embedded { e } else { q };
    let tested = if embedded {
        "loopback_peers"
    } else {
        "loopback_mocks"
    };
    let lib = st.cfg.library.is_some();
    let searchable = st.search.is_some();
    let rss_interval = st
        .current()
        .and_then(|l| crate::rss::interval(&l.settings, st.cfg.automation.rss_interval_minutes));
    let cleanup_policy = st.current().map(|l| {
        [MediaKind::Movie, MediaKind::Series]
            .iter()
            .map(|m| match l.settings.download_client(*m) {
                Some(dc) => format!(
                    "{}: removeCompleted={:?} removeFailed={:?}",
                    m.label(),
                    dc.remove_completed_downloads,
                    dc.remove_failed_downloads
                ),
                None => format!("{}: no torrent client", m.label()),
            })
            .collect::<Vec<_>>()
            .join("; ")
    });
    vec![
        Cap { name: "http_api", implemented: true, configured: true, tested: "loopback_mocks", detail: "authenticated JSON API under /api/v1".into() },
        Cap { name: "managed_catalog", implemented: true, configured: imported, tested: "loopback_mocks", detail: "durable catalog seeded insert-only from imports; add/monitor/unmonitor/remove; live file evidence".into() },
        Cap { name: "release_search", implemented: true, configured: searchable && imported, tested: "loopback_mocks", detail: match &st.search {
            Some(Backend::Native(r)) => format!("native indexers queried directly ({}); per-indexer status, explicit empties and failures reported; bounded fan-out, pacing, Retry-After backoff, caching", r.all().map(|i| format!("{} {}", i.label(), i.cfg.kind.as_str())).collect::<Vec<_>>().join(", ")),
            Some(Backend::Prowlarr(_)) => "Prowlarr GET /api/v1/search with imported indexer ids and typed categories".into(),
            None => "no release search backend: add [[indexer]] entries (native) or [prowlarr]".into(),
        } },
        Cap { name: "release_decisions", implemented: true, configured: imported, tested: "unit_tests", detail: "independent evaluation of imported quality/size/seeder/language/scope/delay/revision rules".into() },
        Cap { name: "movie_discovery", implemented: true, configured: st.movies.is_some(), tested: "loopback_mocks", detail: match &st.movies { Some(m) if m.has_tmdb() => "Plex search and details, with TMDB as the backup".into(), Some(_) => "Plex search and details".into(), None => crate::tmdbkey::MISSING.into() } },
        Cap { name: "tv_discovery", implemented: true, configured: st.tvmaze.is_some(), tested: "loopback_mocks", detail: "TVmaze search and episode lists (enable [tvmaze])".into() },
        Cap { name: "plans", implemented: true, configured: imported, tested: "loopback_mocks", detail: "durable idempotent plans; executable when execution is enabled".into() },
        Cap { name: name("download_submission", "embedded_download_submission"), implemented: true, configured: exec && qb, tested, detail: if embedded { "embedded engine (no qBittorrent API): .torrent bytes or magnet metadata fetched from peers, names validated by the engine's parser before anything is written, durable record before the add, reconciliation by hash, never overwrites existing files, outbox queue limit".into() } else { "qBittorrent WebAPI 2.8.2 (4.3.9) to 2.11 (5.x): hash identity persisted before add, ambiguous-add reconciliation, category + unified-media tag ownership".into() } },
        Cap { name: name("download_monitoring", "embedded_download_monitoring"), implemented: true, configured: exec && qb, tested, detail: "queue status mapping, stall/error/missing/removed classification, pause/resume/cancel/retry".into() },
        Cap { name: name("completed_download_import", "embedded_completed_download_import"), implemented: true, configured: exec && qb && lib, tested, detail: "rename off, native scene names, payload identity checks (torrent, folder and file names vs title/aliases/year), ffprobe validation, effective quality from measured resolution against the profile, sample durations, hardlink/copy, no overwrite, journal and crash recovery".into() },
        Cap { name: "revision_upgrades", implemented: true, configured: exec && st.current().is_some_and(|l| [MediaKind::Movie, MediaKind::Series].iter().any(|m| l.settings.policy(*m).is_some_and(|p| p.revision_upgrades))), tested: "loopback_mocks", detail: "exact quality, higher known revision (PROPER/REPACK); journaled reversible replacement; quality upgrades are not implemented".into() },
        Cap { name: name("download_cleanup", "embedded_download_cleanup"), implemented: true, configured: exec && qb && st.cfg.download_cleanup, tested, detail: format!("removeCompleted/removeFailedDownloads after a verified import and a met share limit; journaled; library files kept ({})", cleanup_policy.unwrap_or_else(|| "no import".into())) },
        Cap { name: name("failed_download_handling", "embedded_failed_download_handling"), implemented: true, configured: exec && qb, tested, detail: "blocklist, autoRedownloadFailed re-search, cancel cooldown".into() },
        Cap { name: "embedded_download_engine", implemented: true, configured: exec && embedded, tested: "loopback_peers", detail: format!("{} in this process: persistent session with fast-resume under the state directory, lifetime upload/download/seeding counters persisted in SQLite (the engine's reset on every restart), share limits enforced from them, explicit listen address, DHT/LSD only when configured, UPnP never", crate::embedded::ENGINE) },
        Cap { name: "rss_sync", implemented: true, configured: exec && searchable && rss_interval.is_some(), tested: "loopback_mocks", detail: match rss_interval { Some(i) => format!("feeds of RSS-enabled indexers {} every {} min; feed windows can miss releases (gap_possible)", match &st.search { Some(Backend::Native(_)) => "read directly (TPB top-100 recent, Torznab t=search)", _ => "via Prowlarr's Torznab proxy" }, i.as_secs() / 60), None => "RSS disabled by the settings' rssSyncInterval or no settings".into() } },
        Cap { name: "explicit_requests", implemented: true, configured: exec, tested: "loopback_mocks", detail: "one-call add-and-search for movies, series, seasons and episodes (scan first), manual searches, backlog search on demand, bounded retries of unmet requests (including movies not yet released)".into() },
        Cap { name: "metadata_refresh", implemented: true, configured: exec && st.tvmaze.is_some(), tested: "loopback_mocks", detail: "fresh TVmaze episode lists for managed series and movie release evidence (Plex, or TMDB) (availability); never changes monitoring or ownership".into() },
        Cap { name: "library_scan", implemented: true, configured: exec && lib, tested: "loopback_mocks", detail: "per-root reconciliation; unavailable/incomplete/suspect-empty roots change nothing; lazy bounded ffprobe".into() },
        Cap { name: "plex", implemented: true, configured: exec && st.plex.is_some(), tested: "loopback_mocks", detail: "section refresh after imports (optional)".into() },
        Cap { name: "coexistence_guard", implemented: true, configured: exec, tested: "loopback_mocks", detail: "one acquisition gate for plans, grabs, RSS, searches and retries: only titles adopted and monitored here are acquired".into() },
        Cap { name: "quality_upgrades", implemented: false, configured: false, tested: "none", detail: {
            let upgrading: Vec<String> = st.current().map(|l| l.settings.profiles.iter().filter(|p| p.upgrade_allowed).map(|p| p.id.clone()).collect()).unwrap_or_default();
            if upgrading.is_empty() {
                "cutoff/quality upgrades of owned files are not implemented; no imported profile allows upgrades".into()
            } else {
                format!("cutoff/quality upgrades of owned files are not implemented; imported profiles allowing them never upgrade here (only exact-quality revisions): {}", upgrading.join(", "))
            }
        } },
        Cap { name: "rename", implemented: false, configured: false, tested: "none", detail: "renaming is not implemented; rename=true blocks file import".into() },
    ]
}

fn verified_level(c: &Cap) -> String {
    c.tested.to_string()
}

/// The `migration` object of `GET /api/v1/readiness`. The staged Sonarr/Radarr migration
/// readiness is gone; the keys stay (`stages` is empty) for clients that read them.
fn migration_readiness(st: &AppState, counts: &BTreeMap<String, usize>) -> Value {
    json!({
        "mode": st.cfg.migration.as_str(),
        "ownership": counts,
        "stages": [],
        "accepted_limitations": [
            "Torznab RSS sees only each feed's latest window; missed windows are flagged gap_possible",
            "specials (season 0) are never acquired",
            "quality/cutoff upgrades, renaming, custom formats and release-profile terms are not implemented",
        ],
        "evidence": {},
        "note": "migration from Sonarr/Radarr is no longer supported; service readiness is the top-level ready",
    })
}

/// Whether the embedded engine can find peers for what the native indexers produce.
fn peer_discovery_readiness(st: &AppState) -> Value {
    let Some(e) = st
        .cfg
        .embedded
        .as_ref()
        .filter(|_| st.cfg.qbittorrent.is_none())
    else {
        return json!({ "name": "peer_discovery", "status": "not_applicable", "note": "the embedded engine is not the download backend" });
    };
    let modes: Vec<&str> = [
        (e.dht, "dht"),
        (e.trackers, "payload trackers"),
        (!e.initial_peers.is_empty(), "initial peers"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, n)| *n)
    .collect();
    let trackerless: Vec<String> = st
        .search
        .as_ref()
        .and_then(Backend::native)
        .map(|r| {
            r.all()
                .filter(|i| i.cfg.kind.builds_magnets() && i.cfg.magnet_trackers.is_empty())
                .map(|i| i.label())
                .collect()
        })
        .unwrap_or_default();
    let blocked = !trackerless.is_empty() && !e.dht && e.initial_peers.is_empty();
    json!({
        "name": "peer_discovery",
        "status": if blocked || modes.is_empty() { "blocked" } else { "ok" },
        "modes": modes,
        "dht": e.dht,
        "trackers": e.trackers,
        "initial_peers": e.initial_peers.len(),
        "trackerless_magnet_sources": trackerless,
        "message": if blocked {
            Some("magnets from these indexers name no tracker and DHT is off with no initial peers: such grabs are refused (no_peer_discovery). Enable [embedded] dht = true for public torrents, or list trusted magnet_trackers")
        } else if modes.is_empty() {
            Some("trackers, DHT and initial peers are all off: no payload can find a peer")
        } else {
            None
        },
        "note": "torrents flagged private never use the DHT (the engine honours the flag); they need their trackers or initial peers",
    })
}

/// The search backend's readiness: which settings indexers it can serve (not contacted).
fn search_readiness(st: &AppState) -> Value {
    let Some(backend) = &st.search else {
        return json!({ "name": "search_backend", "status": "not_configured", "message": "add [[indexer]] entries (native indexers) or a [prowlarr] section" });
    };
    let Backend::Native(r) = backend else {
        return json!({ "name": "search_backend", "status": "configured", "backend": "prowlarr", "note": "Prowlarr reports no per-indexer status" });
    };
    let loaded = st.current();
    let mapping = loaded
        .as_deref()
        .map(|l| r.mapping(&l.settings))
        .unwrap_or_default();
    let usable = mapping.iter().filter(|m| m.status == "usable").count();
    let mut warnings: Vec<String> = vec![];
    let tpb_without_trackers: Vec<String> = r
        .all()
        .filter(|i| i.cfg.kind.builds_magnets() && i.cfg.magnet_trackers.is_empty())
        .map(|i| i.label())
        .collect();
    if let Some(e) = &st.cfg.embedded
        && !tpb_without_trackers.is_empty()
        && !e.dht
        && e.initial_peers.is_empty()
    {
        warnings.push(format!(
            "{} build magnets without trackers (magnet_trackers is empty) and the embedded engine has DHT off and no initial peers: such downloads cannot find peers. Enable [embedded] dht, or list trusted trackers in magnet_trackers",
            tpb_without_trackers.join(", ")
        ));
    }
    let failing: Vec<String> = r
        .all()
        .filter(|i| matches!(i.health().status, "failing" | "backoff"))
        .map(|i| format!("{} is {}", i.label(), i.health().status))
        .collect();
    let status = if loaded.is_none() {
        "no_settings"
    } else if mapping.is_empty() || usable == 0 {
        "blocked"
    } else if usable < mapping.len() || !failing.is_empty() {
        "degraded"
    } else {
        "ok"
    };
    json!({
        "name": "search_backend", "status": status, "backend": "native",
        "indexers": r.all().map(|i| json!({ "id": i.cfg.id, "name": i.cfg.name, "kind": i.cfg.kind, "health": i.health().status })).collect::<Vec<_>>(),
        "settings_mapping": mapping,
        "failing": failing,
        "warnings": warnings,
        "note": "not contacted by readiness: health is what this process observed; see /api/v1/diagnostics",
    })
}

async fn readiness(State(st): St) -> Response {
    let now = OffsetDateTime::now_utc();
    let db_ok = st.store.ping().await.is_ok();
    let mut checks =
        vec![json!({ "name": "state_db", "status": if db_ok { "ok" } else { "failed" } })];
    let index = st.index();
    match st.current() {
        Some(l) => checks.push(json!({
            "name": "import", "status": "ok", "import_id": l.import_id, "observed_at": l.observed_at,
            "imported_at": l.imported_at, "catalog_age_hours": age_hours(&l.observed_at, now),
            "movies": index.catalog.movies.len(), "series": index.catalog.series.len(),
            "blocking_issues": l.settings.blocking_issues.len(),
        })),
        None => checks.push(json!({ "name": "import", "status": "missing", "message": "run `arr setup`" })),
    }
    checks.push(json!({ "name": "prowlarr", "status": if st.cfg.prowlarr.is_some() { "configured" } else { "not_configured" }, "note": "not contacted by readiness; see /api/v1/diagnostics" }));
    if st.movies.is_none() {
        checks.push(json!({ "name": "movie_search", "status": "not_configured", "message": crate::tmdbkey::MISSING }));
    }
    checks.push(search_readiness(&st));
    checks.push(peer_discovery_readiness(&st));
    checks.push(json!({ "name": "tvmaze", "status": if st.tvmaze.is_some() { "enabled" } else { "disabled" } }));
    checks.push(json!({ "name": "plex_metadata", "status": if st.movies.as_ref().is_some_and(Movies::has_plex) { "configured" } else { "not_configured" } }));
    checks.push(json!({ "name": "tmdb", "status": if st.movies.as_ref().is_some_and(Movies::has_tmdb) { "configured" } else { "not_configured" } }));
    checks.push(json!({ "name": "execution", "status": if st.cfg.execution_enabled { "enabled" } else { "disabled" } }));
    if st.cfg.execution_enabled {
        for m in [MediaKind::Movie, MediaKind::Series] {
            let b = downloads::execution_blocker(&st, m);
            checks.push(json!({ "name": format!("execution_{}", if m == MediaKind::Movie { "radarr" } else { "sonarr" }), "status": if b.is_none() { "ok" } else { "blocked" }, "reason": b.map(|x| json!({ "code": x.0, "message": x.1 })) }));
        }
    }
    let ready = db_ok && st.current().is_some();
    let counts = st
        .store
        .with(|c| library::ownership_counts(c))
        .await
        .unwrap_or_default();
    let body = json!({
        "ready": ready,
        "ready_note": "service readiness (state and import loaded)",
        "mode": mode(&st),
        "started_at": rfc3339(st.started_at),
        "checks": checks,
        "migration": migration_readiness(&st, &counts),
    });
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

async fn capabilities(State(st): St) -> Json<Value> {
    let caps: Vec<Value> = capability_rows(&st)
        .iter()
        .map(|c| {
            let state = match (c.implemented, c.configured) {
                (false, _) => "not_implemented",
                (true, true) => "available",
                (true, false) if !st.cfg.execution_enabled && !matches!(c.name, "http_api" | "managed_catalog" | "release_search" | "release_decisions" | "movie_discovery" | "tv_discovery" | "plans") => "disabled",
                (true, false) => "not_configured",
            };
            json!({ "name": c.name, "state": state, "implemented": c.implemented, "configured": c.configured, "verified": verified_level(c), "detail": c.detail })
        })
        .collect();
    Json(json!({
        "mode": mode(&st),
        "execution_enabled": st.cfg.execution_enabled,
        "verification_levels": ["none", "unit_tests", "loopback_mocks", "sandbox", "production"],
        "verification_note": "the level this build's test suite reaches for the capability (unit_tests, loopback_mocks or loopback_peers)",
        "capabilities": caps,
        "policy_blocking_issues": st.current().map(|l| l.settings.blocking_issues.clone()).unwrap_or_default(),
    }))
}

async fn diagnostics(State(st): St) -> ApiResult {
    let mut out = serde_json::Map::new();
    out.insert("mode".into(), json!(mode(&st)));
    out.insert(
        "download_client".into(),
        json!({ "backend": st.cfg.download_backend(), "running": st.client.is_some() }),
    );
    if let Some(e) = st.client.as_deref().and_then(Client::embedded) {
        out.insert("embedded".into(), e.status());
    } else if st.cfg.embedded.is_some() {
        out.insert(
            "embedded".into(),
            json!({ "backend": "embedded", "engine": crate::embedded::ENGINE, "status": "not_running", "reason": "execution is disabled ([execution] enabled = false)" }),
        );
    }
    let qb = match st.client.as_deref().and_then(Client::qbit) {
        None => json!({ "status": "not_configured" }),
        Some(q) => match async {
            Ok::<_, crate::upstream::UpstreamError>((q.categories().await?, q.version().await?))
        }
        .await
        {
            Err(e) => {
                json!({ "status": "failed", "error": { "code": e.code(), "message": format!("qbittorrent {}", e.describe()) } })
            }
            Ok((cats, v)) => {
                let cats = Some(cats.keys().cloned().collect::<Vec<_>>());
                let prefs = q.preferences().await.ok();
                let mut warnings = vec![];
                if let (Some(l), Some(cats)) = (st.current(), &cats) {
                    for m in [MediaKind::Movie, MediaKind::Series] {
                        if let Ok(p) = downloads::client_policy(&st, &l.settings, m) {
                            if !cats.contains(&p.category) {
                                warnings.push(format!("category {} does not exist yet (qBittorrent creates it on the first add)", p.category));
                            }
                            if l.settings
                                .download_client(m)
                                .and_then(|d| d.category.clone())
                                .as_deref()
                                == Some(p.category.as_str())
                            {
                                warnings.push(format!("category {} is shared with native {}; ownership also requires the unified-media tag", p.category, m.label()));
                            }
                        }
                    }
                }
                json!({ "status": "ok", "webapi": v.to_string(), "naming": if v.stop_start() { "5.x stop/start" } else { "4.x pause/resume" }, "categories": cats, "share_limits": prefs, "warnings": warnings })
            }
        },
    };
    out.insert("qbittorrent".into(), qb);
    out.insert(
        "prowlarr".into(),
        json!({ "status": if st.cfg.prowlarr.is_some() { "configured" } else { "not_configured" } }),
    );
    out.insert(
        "search_backend".into(),
        match &st.search {
            Some(Backend::Native(r)) => r.status(st.current().as_deref().map(|l| &l.settings)),
            Some(Backend::Prowlarr(_)) => {
                json!({ "backend": "prowlarr", "note": "Prowlarr reports no per-indexer status" })
            }
            None => json!({ "backend": null, "status": "not_configured" }),
        },
    );
    let ffprobe = match &st.cfg.library {
        None => json!({ "status": "not_configured" }),
        Some(l) => match crate::probe::preflight(&l.ffprobe).await {
            Ok(v) => json!({ "status": "ok", "version": v }),
            Err(e) => json!({ "status": "failed", "message": e }),
        },
    };
    out.insert("ffprobe".into(), ffprobe);
    let mut roots = vec![];
    if let Some(l) = &st.cfg.library {
        for (name, m) in [("movies", MediaKind::Movie), ("tv", MediaKind::Series)] {
            if let Some(p) = crate::ingest::root_for(l, m) {
                roots.push(json!({ "name": name, "local": p.display().to_string(), "exists": p.is_dir(), "free_bytes": crate::paths::free_bytes(p), "native_root": crate::ingest::native_root(&st, m) }));
            }
        }
    }
    out.insert("library_roots".into(), json!(roots));
    let mut exec = serde_json::Map::new();
    for m in [MediaKind::Movie, MediaKind::Series] {
        exec.insert(
            (if m == MediaKind::Movie {
                "radarr"
            } else {
                "sonarr"
            })
            .into(),
            json!(
                downloads::execution_blocker(&st, m)
                    .map(|(c, msg)| json!({ "code": c, "message": msg }))
            ),
        );
    }
    out.insert("execution_blockers".into(), Value::Object(exec));
    let counts = st
        .store
        .with(|c| downloads::counts(c))
        .await
        .map_err(internal("counts"))?;
    out.insert("downloads".into(), json!(counts));
    let rss = st
        .store
        .with(|c| crate::rss::state(c))
        .await
        .map_err(internal("rss"))?;
    out.insert("rss".into(), json!(rss));
    out.insert(
        "plex".into(),
        json!({ "status": if st.plex.is_some() { "configured" } else { "not_configured" } }),
    );
    ok(Value::Object(out))
}

async fn settings(State(st): St) -> ApiResult {
    let l = st.loaded()?;
    ok(
        json!({ "import_id": l.import_id, "observed_at": l.observed_at, "imported_at": l.imported_at, "settings": l.settings }),
    )
}

async fn migration_report(State(st): St) -> ApiResult {
    Ok(Json(st.loaded()?.report.clone()).into_response())
}

// ---------------------------------------------------------------- library

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    q: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
    /// `title` (A to Z), `added` (newest first) or `size` (largest first).
    sort: Option<String>,
}

/// How a library list is ordered when the caller asks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LibrarySort {
    Title,
    Added,
    Size,
}

fn library_sort(raw: Option<&str>) -> Result<Option<LibrarySort>, ApiError> {
    match raw {
        None => Ok(None),
        Some("title") => Ok(Some(LibrarySort::Title)),
        Some("added") => Ok(Some(LibrarySort::Added)),
        Some("size") => Ok(Some(LibrarySort::Size)),
        Some(_) => Err(ApiError::bad_request(
            ErrorCode::InvalidQuery,
            "sort must be title, added or size",
        )),
    }
}

/// When each title was added to the catalog (RFC 3339), by media id.
async fn added_times(st: &AppState) -> Result<std::collections::HashMap<String, String>, ApiError> {
    st.store
        .with(|c| {
            let mut stmt = c.prepare("SELECT id, added_at FROM media")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(internal("added"))
}

fn series_size(s: &crate::catalog::Series) -> u64 {
    // A file that holds several episodes counts once.
    let mut seen = HashSet::new();
    s.episodes
        .iter()
        .flatten()
        .filter_map(|e| e.file.as_ref())
        .filter(|f| f.path.as_ref().is_none_or(|p| seen.insert(p.clone())))
        .filter_map(|f| f.size)
        .sum()
}

fn movie_size(m: &crate::catalog::Movie) -> u64 {
    m.file.as_ref().and_then(|f| f.size).unwrap_or(0)
}

/// Orders `items` as asked: `added` and `size` put the newest and the largest first.
fn sort_library<T>(
    items: &mut [T],
    sort: LibrarySort,
    title: impl Fn(&T) -> &str,
    added: impl Fn(&T) -> String,
    size: impl Fn(&T) -> u64,
) {
    items.sort_by(|a, b| {
        let by_title = title(a).cmp(title(b));
        match sort {
            LibrarySort::Title => by_title,
            LibrarySort::Added => added(b).cmp(&added(a)).then(by_title),
            LibrarySort::Size => size(b).cmp(&size(a)).then(by_title),
        }
    });
}

fn page_bounds(limit: Option<u32>, offset: Option<u32>) -> Result<(usize, usize), ApiError> {
    let limit = limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidQuery,
            "limit must be between 1 and 100",
        ));
    }
    let offset = offset.unwrap_or(0);
    if offset > 1_000_000 {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidQuery,
            "offset is too large",
        ));
    }
    Ok((limit as usize, offset as usize))
}

fn text_query(q: Option<&str>, required: bool) -> Result<Option<String>, ApiError> {
    match q.map(str::trim).filter(|s| !s.is_empty()) {
        None if required => Err(ApiError::bad_request(
            ErrorCode::InvalidQuery,
            "q is required",
        )),
        None => Ok(None),
        Some(s) if s.chars().count() > 100 || s.chars().any(char::is_control) => {
            Err(ApiError::bad_request(
                ErrorCode::InvalidQuery,
                "q must be at most 100 printable characters",
            ))
        }
        Some(s) => Ok(Some(s.to_string())),
    }
}

fn observation(l: &Loaded) -> Value {
    json!({
        "import_id": l.import_id,
        "observed_at": l.observed_at,
        "note": "managed catalog: seeded insert-only from the imported snapshot, then updated by library changes, scans and imports made by this server",
    })
}

fn managed(app: &str) -> bool {
    !app.ends_with("(unmanaged)")
}

async fn library_movies(State(st): St, ApiQuery(q): ApiQuery<ListQuery>) -> ApiResult {
    let l = st.loaded()?;
    let index = st.index();
    let (limit, offset) = page_bounds(q.limit, q.offset)?;
    let items: Vec<&crate::catalog::Movie> = match text_query(q.q.as_deref(), false)? {
        Some(text) => index.search_movies(&text),
        None => {
            let mut all: Vec<_> = index.catalog.movies.iter().collect();
            all.sort_by(|a, b| a.title.cmp(&b.title).then(a.id.cmp(&b.id)));
            all
        }
    };
    let mut items: Vec<_> = items
        .into_iter()
        .filter(|m| managed(&m.provenance.source_app))
        .collect();
    let added = added_times(&st).await?;
    let when = |id: &MediaId| added.get(&id.to_string()).cloned().unwrap_or_default();
    if let Some(sort) = library_sort(q.sort.as_deref())? {
        sort_library(
            &mut items,
            sort,
            |m| m.title.as_str(),
            |m| when(&m.id),
            |m| movie_size(m),
        );
    }
    let views: Vec<Value> = items
        .iter()
        .skip(offset)
        .take(limit)
        .map(|m| {
            let mut v = serde_json::to_value(m).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.insert("added_at".into(), json!(added.get(&m.id.to_string())));
                o.insert("size_bytes".into(), json!(movie_size(m)));
            }
            v
        })
        .collect();
    ok(json!({ "total": items.len(), "items": views, "observation": observation(&l) }))
}

async fn library_series(State(st): St, ApiQuery(q): ApiQuery<ListQuery>) -> ApiResult {
    let l = st.loaded()?;
    let index = st.index();
    let (limit, offset) = page_bounds(q.limit, q.offset)?;
    let items: Vec<&crate::catalog::Series> = match text_query(q.q.as_deref(), false)? {
        Some(text) => index.search_series(&text),
        None => {
            let mut all: Vec<_> = index.catalog.series.iter().collect();
            all.sort_by(|a, b| a.title.cmp(&b.title).then(a.id.cmp(&b.id)));
            all
        }
    };
    let mut items: Vec<_> = items
        .into_iter()
        .filter(|s| managed(&s.provenance.source_app))
        .collect();
    let added = added_times(&st).await?;
    if let Some(sort) = library_sort(q.sort.as_deref())? {
        sort_library(
            &mut items,
            sort,
            |s| s.title.as_str(),
            |s| added.get(&s.id.to_string()).cloned().unwrap_or_default(),
            |s| series_size(s),
        );
    }
    let now = OffsetDateTime::now_utc();
    let views: Vec<Value> = items
        .iter()
        .skip(offset)
        .take(limit)
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
            if let Some(m) = v.as_object_mut() {
                m.insert("added_at".into(), json!(added.get(&s.id.to_string())));
                m.insert("size_bytes".into(), json!(series_size(s)));
                // Live per-season counts (the snapshot's statistics go stale after imports).
                m.insert("seasons".into(), json!(season_summaries(s, now, None)));
                let eps = m.remove("episodes");
                let list = eps.as_ref().and_then(|e| e.as_array());
                m.insert("episode_samples_present".into(), json!(list.is_some()));
                if let Some(list) = list {
                    let owned = list.iter().filter(|e| e["has_file"] == true).count();
                    let queued = list.iter().filter(|e| e["queued"] == true).count();
                    m.insert(
                        "episode_counts".into(),
                        json!({ "known": list.len(), "owned": owned, "queued": queued }),
                    );
                }
            }
            v
        })
        .collect();
    ok(json!({ "total": items.len(), "items": views, "observation": observation(&l) }))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SuppliedMetadata {
    title: String,
    year: Option<u16>,
    #[serde(default)]
    runtime_minutes: Option<u32>,
    #[serde(default)]
    original_language: Option<String>,
    #[serde(default)]
    alternate_titles: Vec<String>,
    #[serde(default)]
    imdb_id: Option<String>,
    /// Release evidence (YYYY-MM-DD). Without any, the movie is not available to grab
    /// (native minimum availability `released`).
    #[serde(default)]
    in_cinemas: Option<String>,
    #[serde(default)]
    physical_release: Option<String>,
    #[serde(default)]
    digital_release: Option<String>,
}

fn choose_profile(
    settings: &Settings,
    media: MediaKind,
    requested: Option<&str>,
) -> Result<String, ApiError> {
    match requested {
        Some(p) => match settings.profile(p) {
            Some(x) if x.media == media => Ok(x.id.clone()),
            _ => Err(ApiError::bad_request(
                ErrorCode::InvalidRequest,
                format!("{p} is not an imported {} quality profile", media.label()),
            )),
        },
        None => settings
            .sole_profile(media)
            .map(|p| p.id.clone())
            .ok_or_else(|| {
                ApiError::bad_request(
                    ErrorCode::ProfileRequired,
                    format!(
                        "several {} profiles exist; pass quality_profile",
                        media.label()
                    ),
                )
            }),
    }
}

fn choose_root(
    st: &AppState,
    settings: &Settings,
    media: MediaKind,
    requested: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let roots: Vec<&str> = settings
        .root_folders
        .iter()
        .filter(|r| r.media == media)
        .map(|r| r.path.trim_end_matches('/'))
        .collect();
    match requested.map(|r| r.trim_end_matches('/')) {
        Some(r) if roots.contains(&r) => Ok(Some(r.to_string())),
        Some(r) => Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            format!("{r} is not an imported {} root folder", media.label()),
        )),
        None => Ok(crate::ingest::native_root(st, media)
            .or_else(|| (roots.len() == 1).then(|| roots[0].to_string()))),
    }
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddBody {
    /// A movie, a series, or a season/episode (added with its series, monitoring only that
    /// scope when the series is new).
    media_id: String,
    #[serde(default)]
    monitored: Option<bool>,
    #[serde(default)]
    monitor: Option<MonitorMode>,
    #[serde(default)]
    quality_profile: Option<String>,
    #[serde(default)]
    root_folder: Option<String>,
    /// Search after adding (default true; false = monitor only).
    #[serde(default = "yes")]
    search: bool,
    /// Operator-supplied movie metadata (when neither Plex nor TMDB can answer).
    #[serde(default)]
    metadata: Option<SuppliedMetadata>,
    /// Accepted and ignored: older clients still send it (Sonarr/Radarr hand-over is gone).
    #[serde(default)]
    #[allow(dead_code)]
    native_handoff_confirmed: Option<serde::de::IgnoredAny>,
    /// Accepted and ignored, like `native_handoff_confirmed`.
    #[serde(default)]
    #[allow(dead_code)]
    preserve_monitoring: Option<serde::de::IgnoredAny>,
}

fn date_field(v: &Option<String>, what: &str) -> Result<Option<String>, ApiError> {
    match v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => time::Date::parse(s, time::macros::format_description!("[year]-[month]-[day]"))
            .map(|_| Some(s.to_string()))
            .map_err(|_| {
                ApiError::bad_request(
                    ErrorCode::InvalidBody,
                    format!("metadata.{what} must be YYYY-MM-DD"),
                )
            }),
    }
}

/// A new movie record from Plex, TMDB or operator metadata (never invented).
async fn new_movie_record(
    st: &AppState,
    l: &Loaded,
    tmdb: u32,
    body: &AddBody,
    now: OffsetDateTime,
) -> Result<crate::catalog::Movie, ApiError> {
    let profile = choose_profile(
        &l.settings,
        MediaKind::Movie,
        body.quality_profile.as_deref(),
    )?;
    let root = choose_root(
        st,
        &l.settings,
        MediaKind::Movie,
        body.root_folder.as_deref(),
    )?;
    let replace_illegal = l
        .settings
        .media_management(MediaKind::Movie)
        .is_none_or(|x| x.replace_illegal_characters);
    let (title, year, runtime, lang, alts, imdb, release, source) = match (
        &st.movies,
        &body.metadata,
    ) {
        (_, Some(md)) => {
            let title = md.title.trim();
            if title.is_empty()
                || title.chars().count() > 200
                || title.chars().any(char::is_control)
            {
                return Err(ApiError::bad_request(
                    ErrorCode::InvalidBody,
                    "metadata.title must be 1-200 printable characters",
                ));
            }
            if md.year.is_none_or(|y| !(1880..=2100).contains(&y)) {
                return Err(ApiError::bad_request(
                    ErrorCode::InvalidBody,
                    "metadata.year is required (1880-2100) for supplied movie metadata",
                ));
            }
            let release = crate::catalog::ReleaseDates {
                in_cinemas: date_field(&md.in_cinemas, "in_cinemas")?,
                physical_release: date_field(&md.physical_release, "physical_release")?,
                digital_release: date_field(&md.digital_release, "digital_release")?,
            };
            (
                title.to_string(),
                md.year,
                md.runtime_minutes,
                md.original_language.clone(),
                md.alternate_titles.iter().take(20).cloned().collect(),
                md.imdb_id.clone(),
                release,
                "operator",
            )
        }
        (Some(t), None) => {
            let m = t
                .movie(tmdb)
                .await
                .map_err(|e| e.into_api("movie metadata"))?
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::NOT_FOUND,
                        ErrorCode::MetadataUnavailable,
                        format!("no movie metadata for TMDB id {tmdb}"),
                    )
                })?;
            let alts = m
                .alternative_titles
                .titles
                .iter()
                .map(|a| a.title.clone())
                .take(20)
                .collect();
            let release = m.release();
            (
                m.title,
                metadata::year_of(m.release_date.as_deref()),
                m.runtime,
                m.original_language
                    .as_deref()
                    .and_then(metadata::language_name)
                    .map(str::to_owned),
                alts,
                m.imdb_id,
                release,
                "metadata",
            )
        }
        (None, None) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::MetadataUnavailable,
                format!(
                    "{} (An operator can instead supply metadata {{title, year, runtime_minutes?, digital_release?/physical_release?/in_cinemas? (YYYY-MM-DD)}} for this TMDB id.)",
                    crate::tmdbkey::MISSING
                ),
            ));
        }
    };
    let folder = library::folder_name(&title, year, replace_illegal);
    if !library::valid_folder(&folder) {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidBody,
            "the title does not give a usable folder name",
        ));
    }
    Ok(library::new_movie(
        tmdb,
        title,
        year,
        runtime,
        lang,
        alts,
        imdb,
        Some(profile),
        root,
        Some(folder),
        release,
        source,
        now,
    ))
}

/// Whether a season or episode is in a series' known episode list (or season list).
fn scope_known(s: &crate::catalog::Series, scope: MediaId) -> bool {
    match scope {
        MediaId::Season(_, n) => {
            s.episodes.iter().flatten().any(|e| e.season == n)
                || s.seasons.iter().any(|x| x.number == n)
        }
        MediaId::Episode(_, n, e) => s
            .episodes
            .iter()
            .flatten()
            .any(|x| x.season == n && x.number == e),
        _ => false,
    }
}

/// What will stop a movie from ever being acquired, from its own metadata and the profile:
/// actionable at add time instead of silently rejecting every release later.
fn movie_acquisition_blockers(
    st: &AppState,
    l: &Loaded,
    m: &crate::catalog::Movie,
    now: OffsetDateTime,
) -> Vec<Value> {
    let mut out = vec![];
    let profile = m
        .quality_profile
        .as_deref()
        .and_then(|p| l.settings.profile(p));
    match profile {
        None => out.push(json!({ "code": "profile_unknown", "message": "the movie has no usable quality profile; pass quality_profile" })),
        Some(p) => {
            let constrained = p.tiers.iter().flat_map(|t| &t.qualities).any(|q| {
                l.settings
                    .size_limit(MediaKind::Movie, *q)
                    .is_some_and(|s| s.min_mib_per_minute > 0.0 || s.max_mib_per_minute.is_some())
            });
            if m.runtime_minutes.is_none() && constrained {
                out.push(json!({ "code": "runtime_unknown", "message": "the runtime is unknown, so release sizes cannot be checked against the profile's size limits and every release is rejected; supply metadata.runtime_minutes" }));
            }
            if p.language.requirement == crate::settings::LanguageRequirement::Original
                && m.original_language.is_none()
            {
                out.push(json!({ "code": "original_language_unknown", "message": "the profile requires the original language, which is unknown; supply metadata.original_language" }));
            }
        }
    }
    match m.availability(now, st.availability_delay_days()) {
        crate::catalog::Availability::Available => {}
        crate::catalog::Availability::NotYet(at) => out.push(json!({ "code": "not_released_yet", "message": format!("not available before {at} (minimum availability released); a request retries until then"), "until": at })),
        crate::catalog::Availability::Unknown(why) => out.push(json!({ "code": "release_date_unknown", "message": format!("{why}; supply metadata.digital_release, physical_release or in_cinemas (YYYY-MM-DD)") })),
        crate::catalog::Availability::Unsupported(why) => out.push(json!({ "code": "availability_unsupported", "message": why })),
    }
    out
}

/// Ensures a series is managed. A new (or re-managed) series gets `monitor`; an already
/// managed one is left exactly as it is. Returns (series id, created).
/// Catalog episodes from a TVmaze list (regular seasons only).
fn catalog_episodes(eps: &[crate::metadata::TvmazeEpisode]) -> Vec<crate::catalog::Episode> {
    eps.iter()
        .filter_map(|e| {
            Some(crate::catalog::Episode {
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
                monitored: true,
                file_quality: None,
                file: None,
                ownership_unknown: false,
                queued: false,
                replaceable: false,
            })
        })
        .collect()
}

async fn ensure_series(
    st: &AppState,
    l: &Loaded,
    key: SeriesKey,
    body: &AddBody,
    monitor: MonitorMode,
    monitor_new: bool,
    now: OffsetDateTime,
) -> Result<(MediaId, bool, bool), ApiError> {
    let index = st.index();
    let replace_illegal = l
        .settings
        .media_management(MediaKind::Series)
        .is_none_or(|x| x.replace_illegal_characters);
    let (series, episodes) = match index.series(key).cloned() {
        // Active (acquiring here): keep every choice already made.
        Some(s) if s.monitored => return Ok((s.id, false, false)),
        // Known but without any episode list (bootstrapped from a library without TVmaze):
        // the full list comes from TVmaze now, so missing episodes become known.
        Some(s) if s.episodes.is_none() && st.tvmaze.is_some() => {
            let tv = st.tvmaze.as_ref().expect("checked");
            let (_, show) = st.resolver(&index, now).series(key).await?;
            let eps = match show {
                Some(show) => tv
                    .episodes(show.id)
                    .await
                    .map_err(|e| e.into_api("tvmaze"))?,
                None => vec![],
            };
            (s, catalog_episodes(&eps))
        }
        // Known but not active (observed, released, unmonitored, or never adopted in
        // coexistence mode): re-managed with exactly the requested monitoring.
        Some(s) => (s, vec![]),
        None => {
            let tv = st.tvmaze.as_ref().ok_or_else(|| {
                ApiError::new(StatusCode::CONFLICT, ErrorCode::MetadataUnavailable, "series outside the catalog need TVmaze ([tvmaze] enabled) for their episode list")
            })?;
            let (target, show) = st.resolver(&index, now).series(key).await?;
            let show = show.ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    ErrorCode::MetadataUnavailable,
                    "TVmaze has no show for this id",
                )
            })?;
            if target.series_type != "standard" {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    ErrorCode::InvalidRequest,
                    format!("{} series are not supported", target.series_type),
                ));
            }
            let eps = tv
                .episodes(show.id)
                .await
                .map_err(|e| e.into_api("tvmaze"))?;
            let profile = choose_profile(
                &l.settings,
                MediaKind::Series,
                body.quality_profile.as_deref(),
            )?;
            let root = choose_root(
                st,
                &l.settings,
                MediaKind::Series,
                body.root_folder.as_deref(),
            )?;
            let folder = library::clean_name(&target.title, replace_illegal);
            if !library::valid_folder(&folder) {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    ErrorCode::InvalidRequest,
                    "the series title does not give a usable folder name",
                ));
            }
            let sid = match target.id {
                MediaId::Series(k) => k,
                _ => key,
            };
            let s = library::new_series(
                sid,
                target.title.clone(),
                target.year,
                target.runtime_minutes,
                target.original_language.clone(),
                Some(show.id),
                Some(profile),
                root,
                Some(folder),
                "tvmaze",
                now,
            );
            (s, catalog_episodes(&eps))
        }
    };
    let sid = series.id;
    let added = st
        .store
        .with(move |c| library::add_series(c, &series, &episodes, monitor, monitor_new, now))
        .await
        .map_err(internal("library add"))?;
    Ok((sid, added.created, true))
}

async fn library_add(State(st): St, ApiJson(body): ApiJson<AddBody>) -> ApiResult {
    // Library mutations are serialised: a replayed or concurrent request sees the first
    // one's result.
    let _guard = st.library_lock.lock().await;
    let l = st.loaded()?;
    let id = parse_media_id(&body.media_id)?;
    let now = OffsetDateTime::now_utc();
    let index = st.index();
    let mut v = match id {
        MediaId::Movie { tmdb } => {
            let monitored = body.monitored.unwrap_or(true);
            let movie = match index.movie(tmdb).cloned() {
                Some(m) => m,
                None => new_movie_record(&st, &l, tmdb, &body, now).await?,
            };
            let added = st
                .store
                .with(move |c| library::add_movie(c, &movie, monitored, now))
                .await
                .map_err(internal("library add"))?;
            st.reload_index().await.map_err(internal("index"))?;
            let blockers = st
                .index()
                .movie(tmdb)
                .map(|m| movie_acquisition_blockers(&st, &l, m, now))
                .unwrap_or_default();
            json!({ "created": added.created, "media_id": id, "scope": id, "acquisition_blockers": blockers })
        }
        MediaId::Series(key) => {
            let monitor = body.monitor.unwrap_or(if body.monitored == Some(false) {
                MonitorMode::None
            } else {
                MonitorMode::All
            });
            let (sid, created, _) = ensure_series(
                &st,
                &l,
                key,
                &body,
                monitor,
                monitor != MonitorMode::None,
                now,
            )
            .await?;
            if !created
                && index
                    .series(key)
                    .is_some_and(|s| managed(&s.provenance.source_app))
                && body.monitored.is_some()
            {
                let m = body.monitored.unwrap_or(true);
                st.store
                    .with(move |c| library::set_monitored(c, sid, m, now))
                    .await
                    .map_err(internal("monitor"))?;
            }
            st.reload_index().await.map_err(internal("index"))?;
            json!({ "created": created, "media_id": sid, "scope": sid, "native_retained": [] })
        }
        MediaId::Season(key, _) | MediaId::Episode(key, _, _) => {
            if body.monitor.is_some() || body.monitored == Some(false) {
                return Err(ApiError::bad_request(
                    ErrorCode::InvalidRequest,
                    "a season or episode add monitors exactly that scope; use PATCH /library/{media_id} to change monitoring",
                ));
            }
            // A known series must already list the scope (checked before anything changes).
            if let Some(s) = index.series(key).filter(|s| s.episodes.is_some())
                && !scope_known(s, id)
            {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    ErrorCode::MetadataUnavailable,
                    format!("{id} is not in the series' episode list (refresh metadata first)"),
                ));
            }
            // A series that is not active here (new, observed, released or unmonitored) is
            // (re-)managed with nothing monitored and no later seasons, so exactly the
            // requested scope becomes effective, whatever dormant native flags it carried. An
            // active series keeps its other choices.
            let (sid, created, _) =
                ensure_series(&st, &l, key, &body, MonitorMode::None, false, now).await?;
            let scope = match id {
                MediaId::Season(_, s) => MediaId::Season(sid.series_key().unwrap_or(key), s),
                MediaId::Episode(_, s, e) => {
                    MediaId::Episode(sid.series_key().unwrap_or(key), s, e)
                }
                other => other,
            };
            st.reload_index().await.map_err(internal("index"))?;
            let known = st
                .index()
                .series(sid.series_key().unwrap_or(key))
                .is_some_and(|s| scope_known(s, scope));
            if !known {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    ErrorCode::MetadataUnavailable,
                    format!("{scope} is not in the series' episode list (refresh metadata first)"),
                ));
            }
            st.store
                .with(move |c| library::adopt_scope(c, scope, now))
                .await
                .map_err(internal("monitor"))?;
            st.reload_index().await.map_err(internal("index"))?;
            json!({ "created": created, "media_id": sid, "scope": scope, "native_retained": [] })
        }
    };
    {
        let index = st.index();
        let acquisition = v["media_id"]
            .as_str()
            .and_then(|s| s.parse::<MediaId>().ok())
            .and_then(|m| match m {
                MediaId::Movie { tmdb } => index.movie(tmdb).and_then(|x| x.acquisition.clone()),
                other => other
                    .series_key()
                    .and_then(|k| index.series(k))
                    .and_then(|x| x.acquisition.clone()),
            });
        v["adoption"] = json!({
            "mode": st.cfg.migration.as_str(),
            "acquisition": acquisition,
            "native_app": Value::Null,
            "native_handoff_confirmed": false,
            "note": "acquired here only; GET /api/v1/ownership lists adopted scopes",
        });
    }
    let parent = plan_media(id);
    let parent = match v["media_id"]
        .as_str()
        .and_then(|s| s.parse::<MediaId>().ok())
    {
        Some(p) => p,
        None => parent,
    };
    // Files already in the library are recognised before anything is planned.
    let scan = crate::scan::scan_item(&st, parent, now)
        .await
        .map_err(internal("scan"))?;
    v["scan"] = scan;
    if body.search {
        let scope: MediaId = v["scope"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(id);
        match scope {
            MediaId::Series(_) => {
                if st.cfg.execution_enabled {
                    let n = rfc3339(now);
                    let cmd = st
                        .store
                        .with(move |c| worker::queue_command(c, "search", Some(scope), &n))
                        .await
                        .map_err(internal("command"))?;
                    st.worker.notify.notify_one();
                    v["search"] = json!({ "command_id": cmd, "note": "each monitored season with aired missing episodes is searched by the worker" });
                } else {
                    v["search"] = json!({ "skipped": "execution is disabled; create shadow plans per season with POST /api/v1/plans" });
                }
            }
            other => {
                let a = st.acquire(other, false, true, "automatic", now).await?;
                v["plan"] = json!(a.plan);
            }
        }
    } else {
        v["search"] = json!({ "skipped": "search = false (monitor only)" });
    }
    created(v)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    monitored: bool,
    /// Accepted and ignored (see `AddBody`).
    #[serde(default)]
    #[allow(dead_code)]
    native_handoff_confirmed: Option<serde::de::IgnoredAny>,
}

async fn library_patch(
    State(st): St,
    p: Result<Path<String>, PathRejection>,
    ApiJson(body): ApiJson<PatchBody>,
) -> ApiResult {
    let _guard = st.library_lock.lock().await;
    st.loaded()?;
    let id = media_id(p)?;
    let now = OffsetDateTime::now_utc();
    let changed = st
        .store
        .with(move |c| library::set_monitored(c, id, body.monitored, now))
        .await
        .map_err(internal("monitor"))?;
    if !changed {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotManaged,
            format!("{id} is not a managed item"),
        ));
    }
    st.reload_index().await.map_err(internal("index"))?;
    ok(json!({ "media_id": id, "monitored": body.monitored }))
}

async fn library_remove(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let _guard = st.library_lock.lock().await;
    st.loaded()?;
    let id = media_id(p)?;
    if !matches!(id, MediaId::Movie { .. } | MediaId::Series(_)) {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidMediaId,
            "remove a movie or a series (unmonitor seasons/episodes with PATCH)",
        ));
    }
    let now = OffsetDateTime::now_utc();
    match st.store.with(move |c| library::remove(c, id, now)).await {
        Ok(true) => {
            st.reload_index().await.map_err(internal("index"))?;
            ok(
                json!({ "media_id": id, "managed": false, "note": "files, history and ownership evidence are kept; downloads are not touched" }),
            )
        }
        Ok(false) => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotManaged,
            format!("{id} is not in the managed catalog"),
        )),
        Err(e) => Err(ApiError::conflict(
            ErrorCode::ActiveDownloads,
            format!("{e:#}"),
        )),
    }
}

/// `GET /storage`: free and total space of the drive(s) holding the movie and TV folders.
/// Drives are named Movies, TV or Movies and TV; folder paths are never part of the reply.
async fn storage(State(st): St) -> ApiResult {
    let (movies, tv) = match &st.cfg.library {
        Some(l) => (l.movies.clone(), l.tv.clone()),
        None => (None, None),
    };
    let drives = tokio::task::spawn_blocking(move || {
        let one = |label: &str, p: &std::path::Path| {
            crate::paths::disk_space(p).map(
                |(free, total)| json!({ "label": label, "free_bytes": free, "total_bytes": total }),
            )
        };
        let mut out = vec![];
        match (movies, tv) {
            (Some(m), Some(t)) if crate::paths::same_drive(&m, &t) => {
                out.extend(one("Movies and TV", &m));
            }
            (m, t) => {
                out.extend(m.and_then(|p| one("Movies", &p)));
                out.extend(t.and_then(|p| one("TV", &p)));
            }
        }
        out
    })
    .await
    .map_err(|e| ApiError::internal("storage", e.to_string()))?;
    ok(json!({ "drives": drives }))
}

async fn library_roots(State(st): St) -> ApiResult {
    let rows = st
        .store
        .with(|c| crate::scan::roots(c))
        .await
        .map_err(internal("roots"))?;
    ok(
        json!({ "roots": rows, "note": "status from the last rescan: ok, unavailable, incomplete, empty_suspect; only ok scans change ownership" }),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WantedQuery {
    media_id: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn wanted(State(st): St, ApiQuery(q): ApiQuery<WantedQuery>) -> ApiResult {
    st.loaded()?;
    let (limit, offset) = page_bounds(q.limit, q.offset)?;
    let filter = q.media_id.as_deref().map(parse_media_id).transpose()?;
    let index = st.index();
    let now = OffsetDateTime::now_utc();
    let delay = st.availability_delay_days();
    let list = st
        .store
        .with(move |c| library::wanted(c, &index, now, delay))
        .await
        .map_err(internal("wanted"))?;
    let list: Vec<_> = list
        .into_iter()
        .filter(|w| filter.is_none_or(|f| w.media_id == f || w.target == f))
        .collect();
    ok(json!({
        "total": list.len(),
        "items": list.iter().skip(offset).take(limit).collect::<Vec<_>>(),
        "note": "monitored, aired, missing and not downloading. Routine acquisition is RSS-based (new releases); search the backlog explicitly with POST /api/v1/commands {\"name\": \"search_missing\"}",
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CalendarQuery {
    start: Option<String>,
    end: Option<String>,
}

async fn calendar(State(st): St, ApiQuery(q): ApiQuery<CalendarQuery>) -> ApiResult {
    st.loaded()?;
    let now = OffsetDateTime::now_utc();
    let parse = |s: Option<&str>, default: OffsetDateTime| -> Result<OffsetDateTime, ApiError> {
        match s {
            None => Ok(default),
            Some(s) => OffsetDateTime::parse(s, &Rfc3339).map_err(|_| {
                ApiError::bad_request(
                    ErrorCode::InvalidQuery,
                    "start/end must be RFC 3339 timestamps",
                )
            }),
        }
    };
    let start = parse(q.start.as_deref(), now - Duration::from_secs(86_400))?;
    let end = parse(q.end.as_deref(), now + Duration::from_secs(14 * 86_400))?;
    if end <= start || end - start > time::Duration::days(92) {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidQuery,
            "end must be after start and at most 92 days later",
        ));
    }
    let entries = library::calendar(&st.index(), start, end, now);
    ok(json!({ "start": rfc3339(start), "end": rfc3339(end), "items": entries }))
}

// ---------------------------------------------------------------- discovery and search

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DiscoverKind {
    Movie,
    Series,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoverQuery {
    kind: DiscoverKind,
    q: Option<String>,
    year: Option<u16>,
}

async fn discover(State(st): St, ApiQuery(q): ApiQuery<DiscoverQuery>) -> ApiResult {
    let text = text_query(q.q.as_deref(), true)?.expect("required");
    let mut results: BTreeMap<String, Value> = BTreeMap::new();
    let mut order: Vec<String> = vec![];
    let mut providers = vec![];
    let mut add = |id: MediaId, entry: Value, source: &str| {
        let key = id.to_string();
        match results.get_mut(&key) {
            Some(existing) => {
                if let Some(arr) = existing["sources"].as_array_mut() {
                    arr.push(json!(source));
                }
            }
            None => {
                let mut e = entry;
                e["media_id"] = json!(key);
                e["sources"] = json!([source]);
                order.push(key.clone());
                results.insert(key, e);
            }
        }
    };
    let index = st.index();
    let catalog = st.current().map(|_| index.as_ref());
    // Plex's review scores for movie hits, by TMDB id.
    let mut movie_scores: HashMap<u32, metadata::Scores> = HashMap::new();
    match (catalog, q.kind) {
        (None, _) => providers.push(json!({ "name": "catalog", "status": "not_imported" })),
        (Some(c), DiscoverKind::Movie) => {
            let hits = c.search_movies(&text);
            providers.push(json!({ "name": "catalog", "status": "ok", "results": hits.len() }));
            for m in hits.into_iter().take(25) {
                add(
                    m.id,
                    json!({ "kind": "movie", "title": m.title, "year": m.year, "in_catalog": true, "has_file": m.has_file, "monitored": m.monitored, "library_state": movie_state(m), "external_ids": { "tmdb": m.tmdb_id, "imdb": m.imdb_id } }),
                    "catalog",
                );
            }
        }
        (Some(c), DiscoverKind::Series) => {
            let hits = c.search_series(&text);
            providers.push(json!({ "name": "catalog", "status": "ok", "results": hits.len() }));
            for s in hits.into_iter().take(25) {
                let seasons: Vec<Value> = season_summaries(s, OffsetDateTime::now_utc(), None)
                    .into_iter()
                    .map(|mut x| {
                        x["season"] = x["number"].clone();
                        x
                    })
                    .collect();
                add(
                    s.id,
                    json!({ "kind": "series", "title": s.title, "year": s.year, "in_catalog": true, "monitored": s.monitored, "library_state": series_state(s), "seasons": seasons, "external_ids": { "tvdb": s.tvdb_id, "tvmaze": s.tvmaze_id, "imdb": s.imdb_id } }),
                    "catalog",
                );
            }
        }
    }
    match q.kind {
        DiscoverKind::Series => match &st.tvmaze {
            None => providers.push(json!({ "name": "tvmaze", "status": "disabled" })),
            Some(tv) => match tv.search(&text).await {
                Ok(shows) => {
                    providers.push(json!({ "name": "tvmaze", "status": "ok", "results": shows.len() }));
                    for show in shows.iter().take(25) {
                        let t = crate::target::series_from_tvmaze(show);
                        let found = catalog.and_then(|c| {
                            t.tvdb_id.and_then(|id| c.series(SeriesKey::Tvdb(id))).or_else(|| c.series(SeriesKey::Tvmaze(show.id)))
                        });
                        let in_catalog = found.is_some();
                        let id = match (t.tvdb_id, catalog) {
                            (Some(tvdb), _) => MediaId::Series(SeriesKey::Tvdb(tvdb)),
                            (None, Some(c)) => c.series(SeriesKey::Tvmaze(show.id)).map(|s| s.id).unwrap_or(t.id),
                            (None, None) => t.id,
                        };
                        add(id, json!({ "kind": "series", "title": show.name, "year": t.year, "in_catalog": in_catalog, "library_state": found.and_then(series_state), "status": show.status, "language": show.language, "external_ids": { "tvdb": t.tvdb_id, "tvmaze": show.id, "imdb": show.externals.imdb } }), "tvmaze");
                    }
                }
                Err(e) => providers.push(json!({ "name": "tvmaze", "status": "failed", "error": { "code": e.code(), "message": format!("tvmaze {}", e.describe()) } })),
            },
        },
        DiscoverKind::Movie => match &st.movies {
            None => providers.push(json!({ "name": "plex", "status": "not_configured", "message": crate::tmdbkey::MISSING })),
            Some(movies) => {
                let (found, answers) = movies.search(&text, q.year).await;
                let source = answers.iter().rev().find(|(_, a)| a.is_ok()).map_or("plex", |(n, _)| *n);
                for (name, answer) in answers {
                    providers.push(match answer {
                        Ok(n) => json!({ "name": name, "status": "ok", "results": n }),
                        Err(e) => json!({ "name": name, "status": "failed", "error": { "code": e.code(), "message": format!("{name} {}", e.describe()) } }),
                    });
                }
                for m in found.iter().take(25) {
                    if let Some(s) = m.scores {
                        movie_scores.insert(m.id, s);
                    }
                    let in_catalog = catalog.is_some_and(|c| c.movie(m.id).is_some());
                    add(MediaId::Movie { tmdb: m.id }, json!({ "kind": "movie", "title": m.title, "original_title": m.original_title, "year": metadata::year_of(m.release_date.as_deref()), "in_catalog": in_catalog, "library_state": catalog.and_then(|c| c.movie(m.id)).and_then(movie_state), "external_ids": { "tmdb": m.id } }), source);
                }
            }
        },
    }
    let mut list: Vec<Value> = order
        .into_iter()
        .filter_map(|k| results.remove(&k))
        .filter(|r| {
            q.year
                .is_none_or(|y| r["year"].as_u64().is_none_or(|ry| ry == u64::from(y)))
        })
        .collect();
    match q.kind {
        DiscoverKind::Movie => {
            for r in &mut list {
                let s = r["external_ids"]["tmdb"]
                    .as_u64()
                    .and_then(|id| u32::try_from(id).ok())
                    .and_then(|id| movie_scores.get(&id).copied());
                r["scores"] = metadata::scores_json(s);
            }
        }
        DiscoverKind::Series => {
            let refs: Vec<(Option<u32>, Option<String>)> = list
                .iter()
                .map(|r| {
                    let ids = &r["external_ids"];
                    (
                        ids["tvdb"].as_u64().and_then(|n| u32::try_from(n).ok()),
                        ids["imdb"].as_str().map(str::to_owned),
                    )
                })
                .collect();
            let scores = match &st.movies {
                Some(m) => m.show_scores(&refs, SCORES_BUDGET).await,
                None => vec![None; list.len()],
            };
            for (r, s) in list.iter_mut().zip(scores) {
                r["scores"] = metadata::scores_json(s);
            }
        }
    }
    ok(
        json!({ "query": text, "kind": match q.kind { DiscoverKind::Movie => "movie", DiscoverKind::Series => "series" }, "results": list, "providers": providers }),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeopleQuery {
    q: Option<String>,
}

fn provider_failed(name: &str, e: &crate::upstream::UpstreamError) -> Value {
    json!({ "name": name, "status": "failed", "error": { "code": e.code(), "message": format!("{name} {}", e.describe()) } })
}

/// People whose names match: Plex when it is set up (TVmaze if Plex fails), else TVmaze.
/// A provider failing is a status here, never an error.
async fn people(State(st): St, ApiQuery(q): ApiQuery<PeopleQuery>) -> ApiResult {
    let text = text_query(q.q.as_deref(), true)?.expect("required");
    let mut providers = vec![];
    let mut found: Vec<Value> = vec![];
    let mut answered = false;
    if let Some(plex) = st.movies.as_ref().and_then(Movies::plex) {
        match plex.people(&text).await {
            Ok(hits) => {
                answered = true;
                providers.push(json!({ "name": "plex", "status": "ok", "results": hits.len() }));
                found = hits
                    .into_iter()
                    .map(|p| json!({ "person_id": format!("plex:{}", p.id), "name": p.name, "known_for": p.known_for, "thumb": p.thumb, "score": p.score }))
                    .collect();
            }
            Err(e) => providers.push(provider_failed("plex", &e)),
        }
    } else {
        providers.push(json!({ "name": "plex", "status": "not_configured" }));
    }
    if !answered {
        match &st.tvmaze {
            None => providers.push(json!({ "name": "tvmaze", "status": "disabled" })),
            Some(tv) => {
                match tv.search_people(&text).await {
                    Ok(mut hits) => {
                        hits.sort_by(|a, b| b.0.total_cmp(&a.0));
                        providers.push(json!({ "name": "tvmaze", "status": "ok", "results": hits.len().min(5) }));
                        found = hits
                        .into_iter()
                        .take(5)
                        .map(|(score, p)| json!({ "person_id": format!("tvmaze:{}", p.id), "name": p.name, "known_for": null, "thumb": p.image.and_then(|i| i.medium), "score": score.clamp(0.0, 1.0) }))
                        .collect();
                    }
                    Err(e) => providers.push(provider_failed("tvmaze", &e)),
                }
            }
        }
    }
    ok(json!({ "query": text, "people": found, "providers": providers }))
}

/// One credit, with where it stands in the library (as discover says it).
fn credit_json(
    id: MediaId,
    title: &str,
    year: Option<u16>,
    role: Option<&str>,
    upcoming: bool,
    state: Option<(bool, bool, Option<&'static str>)>,
) -> Value {
    let (in_catalog, monitored, library_state) = state.unwrap_or((false, false, None));
    json!({ "media_id": id.to_string(), "kind": if matches!(id, MediaId::Movie { .. }) { "movie" } else { "series" }, "title": title, "year": year, "role": role, "in_catalog": in_catalog, "monitored": monitored, "library_state": library_state, "upcoming": upcoming })
}

/// A person and what they made, in groups by job (the one they are best known for first).
async fn person(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let Path(raw) =
        p.map_err(|e| ApiError::bad_request(ErrorCode::InvalidRequest, e.body_text()))?;
    let index = st.index();
    let catalog = st.current().map(|_| index.as_ref());
    let upstream = |e: crate::upstream::UpstreamError| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            e.code(),
            format!("people {}", e.describe()),
        )
    };
    let missing = || ApiError::not_found("no such person");
    let (name, known_for, thumb, groups) = match raw.split_once(':') {
        Some(("plex", id)) => {
            let plex = st
                .movies
                .as_ref()
                .and_then(Movies::plex)
                .ok_or_else(missing)?;
            let page = plex
                .person(id)
                .await
                .map_err(upstream)?
                .ok_or_else(missing)?;
            let groups: Vec<Value> = page
                .groups
                .iter()
                .map(|g| {
                    let credits: Vec<Value> = g
                        .credits
                        .iter()
                        .filter_map(|c| {
                            let (id, state) = match (c.series, c.tmdb, c.tvdb) {
                                (false, Some(tmdb), _) => (
                                    MediaId::Movie { tmdb },
                                    catalog.map(|c| c.movie(tmdb)).map(|m| {
                                        (
                                            m.is_some(),
                                            m.is_some_and(|m| m.monitored),
                                            m.and_then(movie_state),
                                        )
                                    }),
                                ),
                                (true, _, Some(tvdb)) => (
                                    MediaId::Series(SeriesKey::Tvdb(tvdb)),
                                    catalog.map(|c| c.series(SeriesKey::Tvdb(tvdb))).map(|s| {
                                        (
                                            s.is_some(),
                                            s.is_some_and(|s| s.monitored),
                                            s.and_then(series_state),
                                        )
                                    }),
                                ),
                                _ => return None,
                            };
                            Some(credit_json(
                                id,
                                &c.title,
                                c.year,
                                c.role.as_deref(),
                                c.upcoming,
                                state,
                            ))
                        })
                        .collect();
                    json!({ "job": g.job, "credits": credits })
                })
                .collect();
            (page.name, page.known_for, page.thumb, groups)
        }
        Some(("tvmaze", id)) => {
            let id: u32 = id.parse().map_err(|_| {
                ApiError::bad_request(ErrorCode::InvalidRequest, "invalid person id")
            })?;
            let tv = st.tvmaze.as_ref().ok_or_else(missing)?;
            let (who, credits) = tv.person(id).await.map_err(upstream)?.ok_or_else(missing)?;
            let mut seen = HashSet::new();
            let mut groups: Vec<(String, Vec<Value>)> = vec![];
            for (job, show) in credits
                .iter()
                .filter(|(_, s)| !s.name.is_empty())
                .filter(|(_, s)| seen.insert(s.id))
                .take(40)
            {
                let t = crate::target::series_from_tvmaze(show);
                let found = catalog.and_then(|c| {
                    t.tvdb_id
                        .and_then(|id| c.series(SeriesKey::Tvdb(id)))
                        .or_else(|| c.series(SeriesKey::Tvmaze(show.id)))
                });
                let media = match (t.tvdb_id, found) {
                    (Some(tvdb), _) => MediaId::Series(SeriesKey::Tvdb(tvdb)),
                    (None, Some(s)) => s.id,
                    (None, None) => t.id,
                };
                let upcoming = metadata::not_out_yet(show.premiered.as_deref(), t.year);
                let entry = credit_json(
                    media,
                    &show.name,
                    t.year,
                    None,
                    upcoming,
                    catalog.map(|_| {
                        (
                            found.is_some(),
                            found.is_some_and(|s| s.monitored),
                            found.and_then(series_state),
                        )
                    }),
                );
                match groups.iter_mut().find(|g| g.0 == *job) {
                    Some(g) => g.1.push(entry),
                    None => groups.push((job.clone(), vec![entry])),
                }
            }
            (
                who.name,
                None,
                who.image.and_then(|i| i.medium),
                groups
                    .into_iter()
                    .map(|(job, credits)| json!({ "job": job, "credits": credits }))
                    .collect(),
            )
        }
        _ => {
            return Err(ApiError::bad_request(
                ErrorCode::InvalidRequest,
                "a person id looks like plex:<id> or tvmaze:<n>",
            ));
        }
    };
    ok(
        json!({ "person_id": raw, "name": name, "known_for": known_for, "thumb": thumb, "groups": groups }),
    )
}

async fn media(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let id = media_id(p)?;
    let now = OffsetDateTime::now_utc();
    let index = st.index();
    let r = st.resolver(&index, now);
    let body = match id {
        MediaId::Series(key) => {
            let (series, _) = r.series(key).await?;
            let catalog_series = index.series(key).or_else(|| {
                series
                    .tvdb_id
                    .and_then(|t| index.series(SeriesKey::Tvdb(t)))
            });
            json!({
                "kind": "series",
                "media_id": series.id,
                "series": series,
                "seasons": catalog_series.map(|s| season_summaries(s, now, None)),
                "episode_samples_present": catalog_series.is_some_and(|s| s.episodes.is_some()),
                "managed": catalog_series.is_some_and(|s| managed(&s.provenance.source_app)),
            })
        }
        other => {
            let target = r.target(other).await?;
            json!({ "media_id": plan::canonical_id(&target), "requested": other, "target": target })
        }
    };
    ok(body)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleasesQuery {
    refresh: Option<bool>,
}

fn scope_of(target: &Target) -> Scope {
    match target {
        Target::Movie(_) => Scope::Movie,
        Target::Season(s) => Scope::Season(s.season),
        Target::Episode { season, episode } => Scope::Episode(season.season, *episode),
    }
}

/// Searches for the copies of one target and keeps the links of the approved ones, so they
/// can be grabbed. Shared by `GET /media/{id}/releases` and the web app's copies list.
/// `profile` names a profile for a target outside the catalog.
async fn release_run(
    st: &Arc<AppState>,
    id: MediaId,
    refresh: bool,
    profile: Option<&str>,
) -> Result<(Target, search::SearchRun, Option<String>), ApiError> {
    let loaded = st.loaded()?;
    let now = OffsetDateTime::now_utc();
    let index = st.index();
    let target = st.resolver(&index, now).target(id).await?;
    let profile = search::profile_for(&loaded.settings, &target, profile)?;
    let media = target_media(&target);
    let blocked = st
        .store
        .with(move |c| downloads::blocked_for(c, media))
        .await
        .map_err(internal("blocklist"))?;
    let inputs = st.search_inputs(&loaded, &index, now, false, refresh, Some(&blocked));
    let scope = scope_of(&target);
    let profile_id = profile.1.clone();
    let run = search::run(&inputs, &target, profile, &[scope], scope).await?;
    // Approved results can be grabbed with POST /grabs; keep their link material privately.
    let links = downloads::links_for(&run, &target, true);
    st.store
        .with(move |c| downloads::save_links(c, &links, now))
        .await
        .map_err(internal("links"))?;
    Ok((target, run, profile_id))
}

async fn releases(
    State(st): St,
    p: Result<Path<String>, PathRejection>,
    ApiQuery(q): ApiQuery<ReleasesQuery>,
) -> ApiResult {
    let id = media_id(p)?;
    let (target, run, profile_id) = release_run(&st, id, q.refresh.unwrap_or(false), None).await?;
    let approved = run
        .candidates
        .iter()
        .filter(|c| c.decision.approved)
        .count();
    let total = run.candidates.len();
    let views: Vec<Value> = run
        .candidates
        .iter()
        .take(search::MAX_RELEASES_IN_RESPONSE)
        .enumerate()
        .map(|(i, c)| search::view(c, i + 1, &run.queries))
        .collect();
    let mut uncertainty = vec![
        match &st.search {
            Some(crate::indexer::Backend::Native(_)) => "native indexers were queried directly: each query lists every indexer with its own status (see completeness); an explicit 'no results' is distinguished from a failure or an unconfirmed empty".to_string(),
            _ => "Prowlarr's search API does not report per-indexer failures: an indexer that returned nothing is indistinguishable from one that failed".to_string(),
        },
        "decisions are this server's own evaluation of the active policy; indexer results carry no Sonarr/Radarr approval".to_string(),
    ];
    let in_catalog = match &target {
        Target::Movie(m) => m.in_catalog,
        Target::Season(s) | Target::Episode { season: s, .. } => s.series.in_catalog,
    };
    uncertainty.push(if in_catalog {
        "ownership from the managed catalog (snapshot evidence, scans and imports by this server)"
            .to_string()
    } else {
        "target is not in the managed catalog: ownership assumed missing".to_string()
    });
    if let Target::Season(s) | Target::Episode { season: s, .. } = &target {
        uncertainty.push(format!("episode ownership source: {}", s.ownership_source));
        uncertainty.extend(s.notes.iter().cloned());
    }
    let completeness = run.completeness();
    let outcome = if total == 0 {
        uncertainty.push(
            if completeness.provider == crate::search::ProviderCompleteness::Complete {
                "no releases were observed and every indexer answered 'no results'; the indexers may still not carry it".to_string()
            } else {
                "no releases were observed; this is not proof that none exist (see completeness)".to_string()
            },
        );
        "observed_empty"
    } else {
        "results"
    };
    ok(json!({
        "target": plan::canonical_id(&target),
        "requested": id,
        "profile": profile_id,
        "policy_blockers": run.blockers,
        "outcome": outcome,
        "completeness": completeness,
        "counts": { "unique": total, "approved": approved, "rejected": total - approved, "rejection_codes": run.rejection_counts() },
        "queries": run.queries,
        "releases": views,
        "releases_omitted": total.saturating_sub(search::MAX_RELEASES_IN_RESPONSE),
        "grab": "approved releases can be grabbed with POST /api/v1/grabs {media_id, release_ref} while execution is enabled",
        "uncertainty": uncertainty,
    }))
}

// ---------------------------------------------------------------- plans

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct CreatePlan {
    media_id: String,
    #[serde(default)]
    quality_profile: Option<String>,
    #[serde(default)]
    replan: bool,
    /// Submit the planned candidates (needs `[execution] enabled`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    execute: bool,
    /// Also search owned units for native revision upgrades.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    revisions: bool,
}

fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(v) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let s = v.to_str().unwrap_or("");
    let valid = (1..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b));
    if !valid {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            "Idempotency-Key must be 1-128 characters of [A-Za-z0-9._:-]",
        ));
    }
    Ok(Some(s.to_string()))
}

fn key_reused() -> ApiError {
    ApiError::conflict(
        ErrorCode::IdempotencyKeyReused,
        "Idempotency-Key was already used for a different request",
    )
}

fn plan_response(created: bool, plan: &plan::Plan) -> Response {
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    (status, Json(json!({ "created": created, "plan": plan }))).into_response()
}

async fn create_plan(
    State(st): St,
    headers: HeaderMap,
    ApiJson(body): ApiJson<CreatePlan>,
) -> ApiResult {
    let requested = parse_media_id(&body.media_id)?;
    if let Some(p) = &body.quality_profile
        && (p.len() > 64 || !p.contains(':'))
    {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            "quality_profile must look like radarr:<id> or sonarr:<id>",
        ));
    }
    if body.execute
        && let Some((code, msg)) = downloads::execution_blocker(&st, plan_media(requested).kind())
    {
        return Err(ApiError::conflict(refusal_code(code), msg));
    }
    let key = idempotency_key(&headers)?;
    let canonical_body = CreatePlan {
        media_id: requested.to_string(),
        ..body
    };
    let fingerprint = hex::encode(Sha256::digest(
        serde_json::to_vec(&canonical_body).map_err(|e| ApiError::internal("fingerprint", e))?,
    ));
    let execute = canonical_body.execute;
    if let Some(k) = &key
        && let Some((existing, fp)) = st
            .store
            .by_idempotency_key(k.clone())
            .await
            .map_err(internal("idempotency lookup"))?
    {
        if fp != fingerprint {
            return Err(key_reused());
        }
        return Ok(plan_response(false, &existing));
    }
    let loaded = st.loaded()?;
    let now = OffsetDateTime::now_utc();
    let index = st.index();
    let target = st.resolver(&index, now).target(requested).await?;
    let canonical = plan::canonical_id(&target);
    if execute && let Some(why) = downloads::not_adopted(&st, &[canonical]) {
        return Err(ApiError::conflict(ErrorCode::NotAdopted, why));
    }
    let idem = key.map(|k| (k, fingerprint));
    if !canonical_body.replan {
        // Returning the active plan still binds a new key to it (atomically).
        match st
            .store
            .existing_active(canonical.to_string(), idem.clone())
            .await
            .map_err(internal("active plan lookup"))?
        {
            Some(CreateOutcome::Existing(p)) => {
                if execute && matches!(p.state, PlanState::ShadowPlanned | PlanState::ShadowPartial)
                {
                    let p = st.execute_plan(&p.id, "automatic").await?;
                    return Ok(plan_response(false, &p));
                }
                return Ok(plan_response(false, &p));
            }
            Some(_) => return Err(key_reused()),
            None => {}
        }
    }
    let profile = search::profile_for(
        &loaded.settings,
        &target,
        canonical_body.quality_profile.as_deref(),
    )?;
    let media = target_media(&target);
    let blocked = st
        .store
        .with(move |c| downloads::blocked_for(c, media))
        .await
        .map_err(internal("blocklist"))?;
    let inputs = PlanInputs {
        search: st.search_inputs(
            &loaded,
            &index,
            now,
            true,
            canonical_body.replan,
            Some(&blocked),
        ),
        import_id: Some(loaded.import_id.clone()),
        revisions: canonical_body.revisions,
        execution_enabled: st.cfg.execution_enabled,
        episode_deadline: tokio::time::Instant::now() + st.cfg.request_timeout.mul_f64(0.75),
    };
    let built = plan::build(&inputs, &target, requested, profile).await?;
    let links = built.links;
    st.store
        .with(move |c| downloads::save_links(c, &links, now))
        .await
        .map_err(internal("links"))?;
    let (created, plan) = match st
        .store
        .create(built.plan, idem, canonical_body.replan)
        .await
        .map_err(internal("plan insert"))?
    {
        CreateOutcome::Created(p) => (true, p),
        CreateOutcome::Existing(p) => (false, p),
        CreateOutcome::IdempotencyConflict => return Err(key_reused()),
    };
    let plan = if execute
        && matches!(
            plan.state,
            PlanState::ShadowPlanned | PlanState::ShadowPartial
        ) {
        st.execute_plan(&plan.id, "automatic").await?
    } else {
        plan
    };
    if execute && refused_wrong_payload(&plan) {
        // The chosen payload contradicted the item (now blocklisted): plan a valid
        // alternative right away; the response is that superseding plan.
        let a = st
            .acquire(
                requested,
                canonical_body.revisions,
                true,
                "automatic",
                OffsetDateTime::now_utc(),
            )
            .await?;
        return Ok(plan_response(true, &a.plan));
    }
    if execute {
        st.track_request(&plan, now)
            .await
            .map_err(internal("request"))?;
    }
    Ok(plan_response(created, &plan))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlansQuery {
    state: Option<String>,
    target: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn list_plans(State(st): St, ApiQuery(q): ApiQuery<PlansQuery>) -> ApiResult {
    let (limit, offset) = page_bounds(q.limit, q.offset)?;
    let state = match q.state.as_deref() {
        None => None,
        Some(s) => Some(PlanState::parse(s).ok_or_else(|| {
            ApiError::bad_request(ErrorCode::InvalidQuery, format!("unknown plan state {s:?}"))
        })?),
    };
    let target = match q.target.as_deref() {
        None => None,
        Some(t) => Some(parse_media_id(t)?.to_string()),
    };
    let (plans, total) = st
        .store
        .list(state, target, limit as u32, offset as u32)
        .await
        .map_err(internal("plan list"))?;
    ok(json!({ "total": total, "items": plans }))
}

async fn get_plan(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let id = plan_id(p)?;
    match st.store.get(id).await.map_err(internal("plan get"))? {
        Some(plan) => ok(json!({ "plan": plan })),
        None => Err(ApiError::not_found("no such plan")),
    }
}

async fn cancel_plan(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let id = plan_id(p)?;
    match st
        .store
        .cancel(id, rfc3339(OffsetDateTime::now_utc()))
        .await
        .map_err(internal("plan cancel"))?
    {
        None => Err(ApiError::not_found("no such plan")),
        Some(Err(state)) => Err(ApiError::conflict(
            ErrorCode::PlanNotActive,
            format!("plan is {} and cannot be cancelled", state.as_str()),
        )),
        Some(Ok(plan)) => ok(json!({ "plan": plan })),
    }
}

async fn execute_plan(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let id = plan_id(p)?;
    let plan = st.execute_plan(&id, "automatic").await?;
    st.track_request(&plan, OffsetDateTime::now_utc())
        .await
        .map_err(internal("request"))?;
    ok(json!({ "plan": plan }))
}

// ---------------------------------------------------------------- grabs and queue

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrabBody {
    media_id: String,
    release_ref: String,
}

async fn grab(State(st): St, ApiJson(body): ApiJson<GrabBody>) -> ApiResult {
    let target = parse_media_id(&body.media_id)?;
    let r = body.release_ref.clone();
    if !(r.len() == 28 && r.starts_with("rel_") && r[4..].bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            "release_ref must look like rel_<24 hex>",
        ));
    }
    let link = st
        .store
        .with(move |c| downloads::load_link(c, &r, Some(target)))
        .await
        .map_err(internal("link"))?;
    let link = link.filter(|l| l.target == target).ok_or_else(|| {
        ApiError::new(StatusCode::NOT_FOUND, ErrorCode::ReleaseNotFound, "no approved release with this ref for this target; search with GET /api/v1/media/{id}/releases first (links expire after 7 days)")
    })?;
    if let Some(why) = downloads::not_adopted(&st, &[link.target]) {
        return Err(ApiError::conflict(ErrorCode::NotAdopted, why));
    }
    let provider = st.provider_label(link.indexer_id);
    let g = downloads::enqueue(
        &st,
        EnqueueRequest {
            link,
            plan_id: None,
            origin: "interactive",
        },
    )
    .await
    .map_err(|e| enqueue_error(e, &provider))?;
    created(json!({ "grab": downloads::view(&g) }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueQuery {
    all: Option<bool>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn queue(State(st): St, ApiQuery(q): ApiQuery<QueueQuery>) -> ApiResult {
    let (limit, offset) = page_bounds(q.limit, q.offset)?;
    let all = q.all.unwrap_or(false);
    let (items, total) = st
        .store
        .with(move |c| downloads::queue(c, all, limit as u32, offset as u32))
        .await
        .map_err(internal("queue"))?;
    ok(json!({ "total": total, "items": items.iter().map(downloads::view).collect::<Vec<_>>() }))
}

fn grab_id(p: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    prefixed_id(p, "grb_", "download")
}

async fn queue_item(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let id = grab_id(p)?;
    let id2 = id.clone();
    let (g, events, journal) = st
        .store
        .with(move |c| {
            let g = downloads::get(c, &id2)?;
            let mut stmt = c.prepare("SELECT at, kind, message FROM events WHERE grab_id = ?1 ORDER BY id DESC LIMIT 50")?;
            let events: Vec<Value> = stmt.query_map([&id2], |r| Ok(json!({ "at": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "message": r.get::<_, String>(2)? })))?.collect::<rusqlite::Result<_>>()?;
            let mut stmt = c.prepare("SELECT dest, method, state, error, updated_at FROM import_journal WHERE grab_id = ?1 ORDER BY id")?;
            let journal: Vec<Value> = stmt
                .query_map([&id2], |r| Ok(json!({ "dest": r.get::<_, String>(0)?, "method": r.get::<_, Option<String>>(1)?, "state": r.get::<_, String>(2)?, "error": r.get::<_, Option<String>>(3)?, "updated_at": r.get::<_, String>(4)? })))?
                .collect::<rusqlite::Result<_>>()?;
            Ok((g, events, journal))
        })
        .await
        .map_err(internal("queue item"))?;
    let g = g.ok_or_else(|| ApiError::not_found("no such download"))?;
    ok(json!({ "grab": downloads::view(&g), "imports": journal, "events": events }))
}

async fn queue_pause(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let id = grab_id(p)?;
    let g = downloads::pause(&st, &id, true)
        .await
        .map_err(|e| action_error(e, st.cfg.download_backend().unwrap_or("download_client")))?;
    ok(json!({ "grab": downloads::view(&g) }))
}

async fn queue_resume(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let id = grab_id(p)?;
    let g = downloads::pause(&st, &id, false)
        .await
        .map_err(|e| action_error(e, st.cfg.download_backend().unwrap_or("download_client")))?;
    ok(json!({ "grab": downloads::view(&g) }))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CancelBody {
    #[serde(default)]
    blocklist: bool,
    #[serde(default)]
    remove: Option<Removal>,
    #[serde(default)]
    ignore_seed_criteria: bool,
    #[serde(default)]
    research: bool,
}

async fn queue_cancel(
    State(st): St,
    p: Result<Path<String>, PathRejection>,
    body: Bytes,
) -> ApiResult {
    let id = grab_id(p)?;
    let b: CancelBody = optional_body(&body)?;
    let g = downloads::cancel(
        &st,
        &id,
        CancelOptions {
            blocklist: b.blocklist,
            removal: b.remove.unwrap_or(Removal::Keep),
            ignore_seed_criteria: b.ignore_seed_criteria,
            research: b.research,
            only_unfinished: false,
        },
    )
    .await
    .map_err(|e| action_error(e, st.cfg.download_backend().unwrap_or("download_client")))?;
    ok(json!({ "grab": downloads::view(&g) }))
}

async fn queue_retry(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let id = grab_id(p)?;
    let (g, action) = downloads::retry(&st, &id)
        .await
        .map_err(|e| action_error(e, st.cfg.download_backend().unwrap_or("download_client")))?;
    ok(json!({ "grab": downloads::view(&g), "action": action }))
}

// ---------------------------------------------------------------- history, blocklist

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    media_id: Option<String>,
    kind: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn history(State(st): St, ApiQuery(q): ApiQuery<HistoryQuery>) -> ApiResult {
    let (limit, offset) = page_bounds(q.limit, q.offset)?;
    let media = q
        .media_id
        .as_deref()
        .map(parse_media_id)
        .transpose()?
        .map(|m| plan_media(m).to_string());
    if q.kind
        .as_deref()
        .is_some_and(|k| k.len() > 40 || !k.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidQuery,
            "kind must be an event kind like grabbed or file_imported",
        ));
    }
    let kind = q.kind.clone();
    let (rows, total) = st
        .store
        .with(move |c| {
            let filter = "(?1 IS NULL OR media_id = ?1) AND (?2 IS NULL OR kind = ?2)";
            let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM events WHERE {filter}"), params(&media, &kind), |r| r.get(0))?;
            let mut stmt = c.prepare(&format!("SELECT id, at, kind, media_id, target, grab_id, message, data FROM events WHERE {filter} ORDER BY id DESC LIMIT ?3 OFFSET ?4"))?;
            let rows: Vec<Value> = stmt
                .query_map(rusqlite::params![media, kind, limit as i64, offset as i64], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?, "at": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?, "media_id": r.get::<_, Option<String>>(3)?,
                        "target": r.get::<_, Option<String>>(4)?, "grab_id": r.get::<_, Option<String>>(5)?, "message": r.get::<_, String>(6)?,
                        "data": r.get::<_, Option<String>>(7)?.and_then(|d| serde_json::from_str::<Value>(&d).ok()),
                    }))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok((rows, total))
        })
        .await
        .map_err(internal("history"))?;
    ok(json!({ "total": total, "items": rows }))
}

fn params<'a>(a: &'a Option<String>, b: &'a Option<String>) -> [&'a dyn rusqlite::ToSql; 2] {
    [a, b]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlocklistQuery {
    media_id: Option<String>,
}

async fn blocklist(State(st): St, ApiQuery(q): ApiQuery<BlocklistQuery>) -> ApiResult {
    let media = q
        .media_id
        .as_deref()
        .map(parse_media_id)
        .transpose()?
        .map(|m| plan_media(m).to_string());
    let rows = st
        .store
        .with(move |c| {
            let mut stmt = c.prepare("SELECT id, media_id, release_ref, title, reason, grab_id, created_at FROM blocklist WHERE (?1 IS NULL OR media_id = ?1) ORDER BY id DESC LIMIT 500")?;
            let rows: Vec<Value> = stmt
                .query_map([&media], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?, "media_id": r.get::<_, String>(1)?, "release_ref": r.get::<_, String>(2)?,
                        "title": r.get::<_, String>(3)?, "reason": r.get::<_, String>(4)?, "grab_id": r.get::<_, Option<String>>(5)?, "created_at": r.get::<_, String>(6)?,
                    }))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await
        .map_err(internal("blocklist"))?;
    ok(json!({ "items": rows }))
}

async fn blocklist_remove(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let Path(raw) =
        p.map_err(|e| ApiError::bad_request(ErrorCode::InvalidRequest, e.body_text()))?;
    let id: i64 = raw
        .parse()
        .ok()
        .filter(|i| *i > 0)
        .ok_or_else(|| ApiError::not_found("no such blocklist entry"))?;
    let n = st
        .store
        .with(move |c| Ok(c.execute("DELETE FROM blocklist WHERE id = ?1", [id])?))
        .await
        .map_err(internal("blocklist"))?;
    if n == 0 {
        return Err(ApiError::not_found("no such blocklist entry"));
    }
    ok(json!({ "removed": id }))
}

// ---------------------------------------------------------------- commands and tasks

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandBody {
    name: String,
    #[serde(default)]
    media_id: Option<String>,
}

async fn command_create(State(st): St, ApiJson(body): ApiJson<CommandBody>) -> ApiResult {
    if !worker::COMMANDS.contains(&body.name.as_str())
        || body.name == "plex_refresh" && body.media_id.is_none()
    {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            format!(
                "name must be one of {:?} (plex_refresh needs a media_id)",
                worker::COMMANDS
            ),
        ));
    }
    let media = body.media_id.as_deref().map(parse_media_id).transpose()?;
    if body.name == "search" && media.is_none() {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            "search needs a media_id (movie, series, season or episode)",
        ));
    }
    if !st.cfg.execution_enabled {
        return Err(ApiError::conflict(
            ErrorCode::ExecutionDisabled,
            "commands run in the background worker, which runs only with [execution] enabled = true",
        ));
    }
    st.loaded()?;
    let (name, now) = (body.name.clone(), rfc3339(OffsetDateTime::now_utc()));
    let (id, cmd) = st
        .store
        .with(move |c| {
            let id = worker::queue_command(c, &name, media, &now)?;
            Ok((id.clone(), worker::get_command(c, &id)?))
        })
        .await
        .map_err(internal("command"))?;
    st.worker.notify.notify_one();
    let cmd = cmd.ok_or_else(|| ApiError::internal("command", "vanished"))?;
    let fresh = cmd.state == "queued" && cmd.started_at.is_none() && cmd.attempts == 0;
    let status = if fresh {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(json!({ "command": cmd, "id": id, "note": if fresh { "queued" } else { "an identical command is already pending" } }))).into_response())
}

async fn command_list(State(st): St, ApiQuery(q): ApiQuery<ListQuery>) -> ApiResult {
    let (limit, offset) = page_bounds(q.limit, q.offset)?;
    let (rows, total) = st
        .store
        .with(move |c| worker::list_commands(c, limit as u32, offset as u32))
        .await
        .map_err(internal("commands"))?;
    ok(json!({ "total": total, "items": rows }))
}

async fn command_get(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let id = prefixed_id(p, "cmd_", "command")?;
    match st
        .store
        .with(move |c| worker::get_command(c, &id))
        .await
        .map_err(internal("command"))?
    {
        Some(c) => ok(json!({ "command": c })),
        None => Err(ApiError::not_found("no such command")),
    }
}

async fn system_tasks(State(st): St) -> ApiResult {
    let tasks = st
        .store
        .with(|c| worker::tasks(c))
        .await
        .map_err(internal("tasks"))?;
    ok(json!({
        "worker": if st.cfg.execution_enabled { "running" } else { "stopped (execution disabled)" },
        "poll_seconds": st.cfg.automation.poll.as_secs(),
        "rss_interval_minutes": st.current().and_then(|l| crate::rss::interval(&l.settings, st.cfg.automation.rss_interval_minutes)).map(|d| d.as_secs() / 60),
        "tasks": tasks,
        "note": "routine acquisition follows RSS; nothing searches the whole backlog unless search_missing is requested",
    }))
}

async fn requests(State(st): St) -> ApiResult {
    let rows = st
        .store
        .with(|c| worker::list_requests(c, false))
        .await
        .map_err(internal("requests"))?;
    ok(
        json!({ "items": rows, "note": "explicit requests that left a wanted unit unplanned, or refused for it with no active download covering it, are retried with backoff until satisfied or expired" }),
    )
}

async fn rss_status(State(st): St) -> ApiResult {
    let (state, seen) = st
        .store
        .with(|c| {
            let state = crate::rss::state(c)?;
            let mut stmt = c.prepare("SELECT indexer_id, title, published_at, first_seen, last_seen, status, detail, grab_id FROM rss_seen ORDER BY last_seen DESC, first_seen DESC LIMIT 200")?;
            let seen: Vec<Value> = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "indexer_id": r.get::<_, i64>(0)?, "title": r.get::<_, String>(1)?, "published_at": r.get::<_, Option<String>>(2)?,
                        "first_seen": r.get::<_, String>(3)?, "last_seen": r.get::<_, String>(4)?, "status": r.get::<_, String>(5)?,
                        "detail": r.get::<_, Option<String>>(6)?, "grab_id": r.get::<_, Option<String>>(7)?,
                    }))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok((state, seen))
        })
        .await
        .map_err(internal("rss"))?;
    let feeds = st
        .current()
        .map(|l| crate::rss::feeds(&l.settings))
        .unwrap_or_default();
    ok(json!({
        "feeds": feeds.iter().map(|(i, c)| json!({ "indexer_id": i, "categories": c })).collect::<Vec<_>>(),
        "indexers": state,
        "recent": seen,
        "provider_completeness": if st.search.as_ref().is_some_and(|b| b.native().is_some()) { "per_indexer" } else { "unverified" },
        "note": match &st.search {
            Some(Backend::Native(_)) => "feeds are read directly from each native indexer (TPB recent top-100, Torznab t=search); links stay private; a feed window can miss releases (gap_possible)",
            _ => "feeds are read from the configured Prowlarr only; links stay private; a feed window can miss releases (gap_possible)",
        },
    }))
}

// ---------------------------------------------------------------- setup

/// What a fresh deployment has and still needs (nothing is contacted).
async fn setup_status(State(st): St) -> ApiResult {
    let loaded = st.current();
    let mut next: Vec<String> = vec![];
    if loaded.is_none() {
        next.push("POST /api/v1/setup with a managed settings document (or {} for the defaults) to create profiles, indexer, download and root policies and an empty catalog".into());
    }
    if st.search.is_none() {
        next.push("add [[indexer]] entries (kind = \"thepiratebay\", \"knaben\" or \"torznab\") to config.toml and restart".into());
    }
    if st.cfg.library.is_none() {
        next.push("set [library] movies and/or tv in config.toml and restart".into());
    }
    if st.cfg.download_backend().is_none() {
        next.push(
            "add an [embedded] download engine (or [qbittorrent]) to config.toml and restart"
                .into(),
        );
    }
    if st.movies.is_none() {
        next.push(format!("optional: {}", crate::tmdbkey::MISSING));
    }
    if st.tvmaze.is_none() {
        next.push(
            "optional: [tvmaze] enabled = true for series discovery and episode lists".into(),
        );
    }
    if st.plex.is_some() {
        next.push("check the Plex mapping with GET /api/v1/setup/plex".into());
    }
    if !st.cfg.execution_enabled {
        next.push("when the readiness checks pass, enable [execution] deliberately (setup never does) and restart".into());
    }
    ok(json!({
        "settings": loaded.as_deref().map(|l| json!({
            "origin": l.origin, "import_id": l.import_id, "activated_at": l.imported_at,
            "profiles": l.settings.profiles.iter().map(|p| json!({ "id": p.id, "media": p.media, "name": p.name })).collect::<Vec<_>>(),
            "indexers": l.settings.indexers.iter().map(|i| json!({ "id": i.prowlarr_indexer_id, "media": i.media, "name": i.name })).collect::<Vec<_>>(),
            "root_folders": l.settings.root_folders,
        })),
        "accepts_managed_setup": loaded.as_deref().is_none_or(|l| l.origin == "managed_setup"),
        "search_backend": st.cfg.search_backend(),
        "native_indexers": st.cfg.indexers.iter().map(|i| json!({ "id": i.id, "name": i.name, "kind": i.kind })).collect::<Vec<_>>(),
        "download_backend": st.cfg.download_backend(),
        "execution_enabled": st.cfg.execution_enabled,
        "library": st.cfg.library.as_ref().map(|l| json!({ "movies": l.movies, "tv": l.tv })),
        "plex": st.cfg.plex.as_ref().map(|p| json!({ "movie_section": p.movie_section, "tv_section": p.tv_section, "movies_path": p.movies_path, "tv_path": p.tv_path })),
        "metadata": { "tvmaze": st.tvmaze.is_some(), "plex": st.movies.as_ref().is_some_and(Movies::has_plex), "tmdb": st.movies.as_ref().is_some_and(Movies::has_tmdb) },
        "mcp": st.cfg.mcp.is_some(),
        "next_steps": next,
        "note": "connections and credentials (as file paths) live in config.toml; this endpoint manages policy only and never enables execution",
    }))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SetupBody {
    /// The managed settings document (every key optional; `{}` = the defaults).
    #[serde(default)]
    document: crate::setup::SetupDoc,
    /// Validate and report without writing anything.
    #[serde(default)]
    dry_run: bool,
}

/// Creates (or replaces) managed settings and activates them in this process.
async fn setup_apply(State(st): St, body: Bytes) -> ApiResult {
    let body: SetupBody = optional_body(&body)?;
    // No library mutation or other setup interleaves with the replacement check.
    let _setup = st.setup_lock.lock().await;
    let _library = st.library_lock.lock().await;
    let now = OffsetDateTime::now_utc();
    let prepared = crate::setup::prepare(&body.document, &st.cfg, now).map_err(|errors| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidSetup,
            format!("the setup document is invalid: {}", errors.join("; ")),
        )
        .with_details(json!({ "errors": errors }))
    })?;
    let active = st.current();
    let grabs = st
        .store
        .with(|c| crate::setup::active_grabs(c))
        .await
        .map_err(internal("active grabs"))?;
    let conflicts = crate::setup::replace_conflicts(
        active.as_deref().map(|l| l.origin),
        &prepared.normalized.settings,
        &st.index(),
        &grabs,
    );
    if !conflicts.is_empty() {
        return Err(ApiError::conflict(
            ErrorCode::SetupConflict,
            format!("managed setup refused: {}", conflicts.join("; ")),
        )
        .with_details(json!({ "conflicts": conflicts })));
    }
    let id = prepared.normalized.report.import_id.clone();
    if body.dry_run {
        return ok(crate::setup::summary(&prepared, "dry_run", None));
    }
    if active.as_deref().is_some_and(|l| l.import_id == id) {
        return ok(crate::setup::summary(&prepared, "unchanged", None));
    }
    let state_dir = st.cfg.state_dir.clone();
    let source = prepared.source.clone();
    let normalized = import::Normalized {
        settings: prepared.normalized.settings.clone(),
        catalog: prepared.normalized.catalog.clone(),
        report: prepared.normalized.report.clone(),
    };
    let (status, dir, loaded) = tokio::task::spawn_blocking(move || {
        let (status, dir) = import::write_and_activate(
            &state_dir,
            crate::setup::SOURCE_FILE,
            &source,
            &normalized,
        )?;
        let loaded = import::load_active(&state_dir)?
            .ok_or_else(|| anyhow::anyhow!("the written setup is not active"))?;
        Ok::<_, anyhow::Error>((status, dir, loaded))
    })
    .await
    .map_err(|e| ApiError::internal("setup task", e))?
    .map_err(internal("setup write"))?;
    if loaded.manifest.import_id != id {
        return Err(ApiError::internal(
            "setup",
            "a different import became active",
        ));
    }
    st.activate(loaded)
        .await
        .map_err(internal("setup activation"))?;
    tracing::info!(import_id = %id, status, "managed setup activated");
    let mut v = crate::setup::summary(&prepared, status, Some(dir));
    v["replaced"] = json!(active.as_deref().map(|l| l.import_id.clone()));
    created(v)
}

/// Read-only Plex discovery: sections and the configured section/path mapping.
async fn setup_plex(State(st): St) -> ApiResult {
    let Some(plex) = &st.plex else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderNotConfigured,
            "Plex is not configured ([plex] base_url and token_file in config.toml)",
        ));
    };
    let sections = plex.sections().await.map_err(|e| e.into_api("plex"))?;
    ok(crate::setup::plex_check(plex.config(), &st.cfg, &sections))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct BootstrapBody {
    /// `movies`, `tv` or `all` (default).
    #[serde(default)]
    media: Option<String>,
    /// Start over (the previous cursor and counts are discarded; evidence stays).
    #[serde(default)]
    restart: bool,
    /// Report what would be recorded without writing.
    #[serde(default)]
    dry_run: bool,
    /// Items per batch (default 50 movies or 5 shows; at most 200).
    #[serde(default)]
    batch: Option<u32>,
}

fn bootstrap_kinds(media: Option<&str>) -> Result<Vec<MediaKind>, ApiError> {
    match media.unwrap_or("all") {
        "all" => Ok(vec![MediaKind::Movie, MediaKind::Series]),
        "movies" | "movie" => Ok(vec![MediaKind::Movie]),
        "tv" | "series" => Ok(vec![MediaKind::Series]),
        other => Err(ApiError::bad_request(
            ErrorCode::InvalidBody,
            format!("media must be movies, tv or all (got {other:?})"),
        )),
    }
}

async fn bootstrap_status(State(st): St) -> ApiResult {
    let runs = st
        .store
        .with(|c| crate::bootstrap::runs(c))
        .await
        .map_err(internal("bootstrap state"))?;
    ok(
        json!({ "runs": runs, "note": "POST /api/v1/setup/plex/bootstrap runs one bounded batch per media kind and resumes from the stored cursor" }),
    )
}

/// One bounded, resumable batch of the read-only Plex library bootstrap per media kind.
async fn bootstrap_batch(State(st): St, body: Bytes) -> ApiResult {
    let body: BootstrapBody = optional_body(&body)?;
    let kinds = bootstrap_kinds(body.media.as_deref())?;
    let mut runs = vec![];
    for m in kinds {
        runs.push(crate::bootstrap::batch(&st, m, body.restart, body.dry_run, body.batch).await?);
    }
    let done = runs.iter().all(|r| r.status != "running");
    ok(json!({
        "runs": runs,
        "done": done,
        "next": if done { "finished (see each run's status, counts and issues)" } else { "call again to continue from the stored cursor" },
        "monitoring": "unchanged: bootstrapped titles are not monitored and nothing is searched",
    }))
}

// ---------------------------------------------------------------- server

pub async fn serve(cfg: RuntimeConfig) -> anyhow::Result<()> {
    // Execution imports media only after validating it: without a working ffprobe the server
    // does not start in execution mode (nothing is fetched or skipped).
    if cfg.execution_enabled
        && let Some(l) = &cfg.library
    {
        let version = crate::probe::preflight(&l.ffprobe)
            .await
            .map_err(|e| anyhow::anyhow!("media validation preflight failed: {e}"))?;
        tracing::info!(ffprobe = %l.ffprobe.display(), %version, "media probe available");
    }
    let listen = cfg.listen;
    let state = Arc::new(AppState::new(cfg)?);
    // The API socket first: an occupied API port stops startup before any transfer starts.
    let listener = tokio::net::TcpListener::bind(listen).await?;
    // The embedded engine starts with the server, so a failure (for example a listen address
    // in use, or another engine on this state) stops startup instead of surfacing later. A
    // failed start leaves no session running.
    let engine = state
        .client
        .as_deref()
        .and_then(Client::embedded)
        .filter(|_| state.cfg.execution_enabled);
    if let Some(e) = engine {
        e.start()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.describe()))?;
        tracing::info!(listen = ?e.listen_addr(), engine = crate::embedded::ENGINE, "embedded download engine started");
    }
    match state.current() {
        Some(l) => {
            tracing::info!(import_id = %l.import_id, observed_at = %l.observed_at, "loaded import")
        }
        None => tracing::warn!(
            "no active import; catalog, search and plan endpoints return catalog_not_imported"
        ),
    }
    let worker = state.cfg.execution_enabled.then(|| {
        // Normal operation, not a problem: info, so the Health screen doesn't list it.
        tracing::info!(
            backend = state.cfg.download_backend().unwrap_or("none"),
            "execution is ENABLED: torrents are submitted to the configured download client and files imported into [library]"
        );
        worker::spawn(state.clone())
    });
    tracing::info!(%listen, mode = mode(&state), "listening");
    let (app, stop_mcp) = app(state.clone())?;
    if state.cfg.mcp.is_some() {
        tracing::info!(
            path = "/mcp",
            oauth = state.cfg.mcp.as_ref().is_some_and(|m| m.oauth.is_some()),
            "remote MCP (Streamable HTTP, bearer token) enabled"
        );
    }
    let served = axum::serve(listener, app)
        .with_graceful_shutdown({
            let state = state.clone();
            async move {
                // A signal, or a changed setting that needs Arr to start again.
                tokio::select! { () = shutdown() => {}, () = state.restart.wait() => {} }
                stop_mcp();
            }
        })
        .await;
    // The worker finishes its current pass and exits; then the engine records its counters,
    // stops, lets running piece checks finish and releases the state's engine lock.
    if let Some(mut w) = worker {
        state.worker.stop();
        if tokio::time::timeout(Duration::from_secs(120), &mut w)
            .await
            .is_err()
        {
            tracing::warn!("the worker did not finish its pass within 120 s; stopping it");
            w.abort();
            let _ = w.await;
        }
    }
    if let Some(e) = engine {
        e.shutdown().await;
    }
    served?;
    if state.restart.requested() {
        // Everything is stopped. A non-zero exit is what makes systemd and the Windows task
        // start Arr again (launchd does after any exit); see `lifecycle.rs`.
        tracing::info!("restarting to use a changed setting");
        std::process::exit(crate::lifecycle::RESTART_EXIT);
    }
    Ok(())
}

/// The whole HTTP application: the API and the web app, plus the remote MCP endpoint at `/mcp` when
/// `[mcp] enabled`. The closure ends MCP processing at shutdown.
pub fn app(state: Arc<AppState>) -> anyhow::Result<(Router, Box<dyn FnOnce() + Send>)> {
    let api = router(state.clone());
    let web = crate::webapp::router(&state, api.clone())?;
    // gzip for the JSON API and the web app (the default predicate leaves images, event
    // streams and tiny bodies alone). Not for `/mcp`, whose replies may stream.
    let (api, web) = (api.layer(compress()), web.layer(compress()));
    match &state.cfg.mcp {
        Some(m) => {
            let (mcp, stop) = crate::mcp::remote(
                m,
                api.clone(),
                state.cfg.api_token.expose(),
                &state.cfg.state_dir,
            )?;
            Ok((mcp.merge(api).merge(web), Box::new(stop)))
        }
        None => Ok((api.merge(web), Box::new(|| {}))),
    }
}

fn compress() -> tower_http::compression::CompressionLayer {
    tower_http::compression::CompressionLayer::new()
        .no_br()
        .no_deflate()
        .no_zstd()
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    // Windows: Ctrl+C, closing the console window, logging off or shutting down.
    #[cfg(windows)]
    let term = async {
        use tokio::signal::windows;
        match (
            windows::ctrl_close(),
            windows::ctrl_logoff(),
            windows::ctrl_shutdown(),
        ) {
            (Ok(mut a), Ok(mut b), Ok(mut c)) => {
                tokio::select! { _ = a.recv() => {}, _ = b.recv() => {}, _ = c.recv() => {} }
            }
            _ => std::future::pending::<()>().await,
        }
    };
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

// ---------------------------------------------------------------- ownership, status, attention

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipQuery {
    state: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

/// Acquisition ownership: which titles (and, for series, which seasons and episodes) this
/// server acquires. (`handoff` is kept as a constant note for older clients.)
async fn ownership(State(st): St, ApiQuery(q): ApiQuery<OwnershipQuery>) -> ApiResult {
    st.loaded()?;
    let (limit, offset) = page_bounds(q.limit, q.offset)?;
    if let Some(s) = q.state.as_deref()
        && !matches!(s, "adopted" | "legacy_managed" | "observed" | "released")
    {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidQuery,
            "state must be adopted, legacy_managed, observed or released",
        ));
    }
    let state = q.state.clone();
    let (items, total) = st
        .store
        .with(move |c| library::ownership(c, state.as_deref(), None, limit, offset))
        .await
        .map_err(internal("ownership"))?;
    let counts = st
        .store
        .with(|c| library::ownership_counts(c))
        .await
        .map_err(internal("ownership"))?;
    ok(json!({
        "mode": st.cfg.migration.as_str(),
        "acquiring": "adopted items only (effective monitored scopes)",
        "counts": counts,
        "total": total,
        "items": items,
        "handoff": { "note": "this server does not contact or depend on Sonarr or Radarr" },
    }))
}

/// Per-season counts from the live episode list (owned, queued, aired, missing), falling back
/// to the snapshot's season statistics (marked `snapshot`) when no episode list is known.
fn season_summaries(
    s: &crate::catalog::Series,
    now: OffsetDateTime,
    only: Option<u16>,
) -> Vec<Value> {
    let aired = |e: &crate::catalog::Episode| {
        e.air_date_utc
            .as_deref()
            .and_then(|d| OffsetDateTime::parse(d, &Rfc3339).ok())
            .is_some_and(|t| t <= now)
    };
    let Some(eps) = &s.episodes else {
        return s
            .seasons
            .iter()
            .filter(|x| x.number > 0 && only.is_none_or(|o| o == x.number))
            .map(|x| {
                json!({
                    "number": x.number, "monitored": x.monitored, "source": "snapshot",
                    "episode_file_count": x.episode_file_count, "episode_count": x.episode_count,
                    "total_episode_count": x.total_episode_count,
                })
            })
            .collect();
    };
    let mut numbers: Vec<u16> = eps.iter().map(|e| e.season).filter(|n| *n > 0).collect();
    numbers.extend(s.seasons.iter().map(|x| x.number).filter(|n| *n > 0));
    numbers.sort_unstable();
    numbers.dedup();
    numbers
        .into_iter()
        .filter(|n| only.is_none_or(|o| o == *n))
        .map(|n| {
            let list: Vec<&crate::catalog::Episode> =
                eps.iter().filter(|e| e.season == n).collect();
            let owned = list.iter().filter(|e| e.has_file).count();
            let queued = list.iter().filter(|e| !e.has_file && e.queued).count();
            let aired_n = list.iter().filter(|e| aired(e)).count();
            let missing: Vec<&&crate::catalog::Episode> = list
                .iter()
                .filter(|e| aired(e) && !e.has_file && !e.queued)
                .collect();
            let monitored_missing = missing
                .iter()
                .filter(|e| e.monitored && s.monitored)
                .count();
            let unknown = list.iter().filter(|e| e.ownership_unknown).count();
            let aired_owned = list.iter().filter(|e| aired(e) && e.has_file).count();
            json!({
                "number": n,
                "monitored": s.seasons.iter().find(|x| x.number == n).map(|x| x.monitored),
                "source": "live",
                "episode_file_count": owned,
                "episode_count": aired_n,
                "total_episode_count": list.len(),
                "queued": queued,
                "missing": missing.len(),
                "monitored_missing": monitored_missing,
                "unaired": list.len() - aired_n,
                "ownership_unknown": unknown,
                "monitored_episodes": list.iter().filter(|e| e.monitored && s.monitored).count(),
                "complete_aired": aired_n > 0 && aired_owned == aired_n,
                "complete": !list.is_empty() && owned == list.len(),
            })
        })
        .collect()
}

fn grab_brief(g: &downloads::Grab) -> Value {
    json!({
        "id": g.id, "target": g.target, "state": g.state, "status": downloads::status(g),
        "progress": g.progress, "client_state": g.client_state, "active": g.active,
        "failure": g.failure, "last_error": g.last_error.as_deref().map(|e| e.chars().take(300).collect::<String>()),
        "retry": g.retry, "updated_at": g.updated_at,
    })
}

/// The attention item for a series' metadata numbering hold, if it has one.
fn hold_item(s: &crate::catalog::Series) -> Option<Value> {
    let h = s.metadata_hold.as_ref()?;
    Some(json!({
        "kind": "metadata_hold", "media_id": s.id, "title": s.title,
        "since": h["since"], "updated_at": h["updated_at"], "conflicts": h["conflicts"],
        "held_back": h["held_back"], "reasons": h["reasons"],
        "action": "compare the metadata provider's numbering with the native one; conflicting episodes and new seasons are not added until a refresh that lists every imported episode finds no conflict",
    }))
}

/// Whether a movie, episode or whole season is in the library now, by the catalog.
fn owned_now(index: &CatalogIndex, t: MediaId) -> bool {
    let episodes = |k: SeriesKey| index.series(k).and_then(|s| s.episodes.as_deref());
    match t {
        MediaId::Movie { tmdb } => index.movie(tmdb).is_some_and(|m| m.has_file),
        MediaId::Episode(k, n, e) => episodes(k).is_some_and(|eps| {
            eps.iter()
                .any(|x| x.season == n && x.number == e && x.has_file)
        }),
        MediaId::Season(k, n) => episodes(k).is_some_and(|eps| {
            let mut season = eps.iter().filter(|x| x.season == n).peekable();
            season.peek().is_some() && season.all(|x| x.has_file)
        }),
        _ => false,
    }
}

/// Items needing a person (or the bridge): metadata numbering holds, imports that cannot
/// proceed, recent failures, requests that keep retrying or expired. Bounded.
/// The movie or episodes a download covered (its own target when none are recorded).
fn covered_by(g: &downloads::Grab) -> Vec<MediaId> {
    if g.targets.is_empty() {
        vec![g.target]
    } else {
        g.targets.iter().map(|t| t.target).collect()
    }
}

fn attention_items(grabs: &[downloads::Grab], requests: &[Value], holds: Vec<Value>) -> Vec<Value> {
    let mut out = holds;
    for g in grabs {
        if g.active && g.state == "import_blocked" {
            out.push(json!({ "kind": "import_blocked", "target": g.target, "grab_id": g.id, "since": g.updated_at, "reason": g.last_error, "action": "fix the reason, then POST /api/v1/queue/{grab_id}/retry, or cancel it" }));
        } else if g.state == "failed" {
            out.push(json!({ "kind": "download_failed", "target": g.target, "grab_id": g.id, "failure": g.failure, "reason": g.last_error, "retry": g.retry, "at": g.updated_at, "covers": covered_by(g) }));
        }
    }
    for r in requests {
        let state = r["state"].as_str().unwrap_or("");
        let attempts = r["attempts"].as_i64().unwrap_or(0);
        if state == "expired" {
            out.push(json!({ "kind": "request_expired", "target": r["target"], "last_result": r["last_result"], "at": r["updated_at"] }));
        } else if state == "active" && attempts >= 3 {
            out.push(json!({ "kind": "request_retrying", "target": r["target"], "attempts": attempts, "next_attempt_at": r["next_attempt_at"], "last_result": r["last_result"] }));
        }
    }
    out.truncate(100);
    out
}

/// Live status of one movie, series, season or episode: ownership and acquisition state,
/// per-season completeness, per-episode state with download progress, the explicit request
/// and anything needing attention.
async fn media_status(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    st.loaded()?;
    let now = OffsetDateTime::now_utc();
    let index = st.index();
    // Stored grabs and requests use the canonical series id (TVDB when known): resolve a
    // TVmaze-keyed scope to it first.
    let canon_key = |k: SeriesKey| index.series(k).and_then(|s| s.id.series_key()).unwrap_or(k);
    let canonical = |m: MediaId| match m {
        MediaId::Series(k) => MediaId::Series(canon_key(k)),
        MediaId::Season(k, n) => MediaId::Season(canon_key(k), n),
        MediaId::Episode(k, n, e) => MediaId::Episode(canon_key(k), n, e),
        other => other,
    };
    let id = canonical(media_id(p)?);
    let parent = plan_media(id);
    let (parent_s, since) = (
        parent.to_string(),
        rfc3339(now - Duration::from_secs(7 * 86_400)),
    );
    let grabs = st
        .store
        .with(move |c| {
            let grabs = downloads::select(
                c,
                "media_id = ?1 AND (active = 1 OR updated_at >= ?2) ORDER BY created_at DESC LIMIT 50",
                &[&parent_s, &since],
            )?;
            Ok((grabs, downloads::active_coverage(c)?))
        })
        .await
        .map_err(internal("grabs"))?;
    let (grabs, active_cover) = grabs;
    let parent_s = parent.to_string();
    let requests: Vec<Value> = st
        .store
        .with(|c| worker::list_requests(c, false))
        .await
        .map_err(internal("requests"))?
        .into_iter()
        .filter(|r| r["media_id"] == parent_s.as_str())
        .collect();
    // Active download per covered target.
    let mut active: std::collections::HashMap<String, &downloads::Grab> =
        std::collections::HashMap::new();
    for g in grabs.iter().filter(|g| g.active) {
        for t in &g.targets {
            active.insert(t.target.to_string(), g);
        }
    }
    // Structural scope: a season includes its episodes' grabs and requests, an episode the
    // requests of its season, and either the whole series' (whatever the id kind).
    let in_scope = |t: MediaId| -> bool {
        match (id, canonical(t)) {
            (MediaId::Movie { .. }, t) => t == id,
            (MediaId::Series(k), t) => t.series_key() == Some(k),
            (MediaId::Season(k, n), MediaId::Season(k2, n2))
            | (MediaId::Season(k, n), MediaId::Episode(k2, n2, _))
            | (MediaId::Episode(k, n, _), MediaId::Season(k2, n2)) => k == k2 && n == n2,
            (MediaId::Episode(k, n, e), MediaId::Episode(k2, n2, e2)) => {
                k == k2 && n == n2 && e == e2
            }
            (MediaId::Season(k, _) | MediaId::Episode(k, _, _), MediaId::Series(k2)) => k == k2,
            _ => false,
        }
    };
    let parsed = |v: &Value| v.as_str().and_then(|t| t.parse::<MediaId>().ok());
    let request_view = |r: &Value| json!({ "target": r["target"], "state": r["state"], "attempts": r["attempts"], "next_attempt_at": r["next_attempt_at"], "expires_at": r["expires_at"], "last_result": r["last_result"] });
    let mut body = match id {
        MediaId::Movie { tmdb } => {
            let m = index.movie(tmdb).ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    ErrorCode::NotManaged,
                    format!(
                        "{id} is not in the managed catalog (add it with POST /api/v1/library)"
                    ),
                )
            })?;
            let availability = m.availability(now, st.availability_delay_days());
            let download = active.get(&id.to_string()).map(|g| grab_brief(g));
            let state = if m.has_file {
                "owned"
            } else if download.is_some() || m.queued {
                "downloading"
            } else if m.ownership_unknown {
                "unknown"
            } else if availability != crate::catalog::Availability::Available {
                "not_available"
            } else {
                "missing"
            };
            json!({
                "media_id": id, "kind": "movie", "title": m.title, "year": m.year,
                "acquisition": m.acquisition, "monitored": m.monitored, "state": state,
                "file": m.file.as_ref().map(|f| json!({ "path": f.path, "quality": f.quality, "source": f.source, "observed_at": f.observed_at })),
                "availability": match availability {
                    crate::catalog::Availability::Available => json!({ "available": true }),
                    crate::catalog::Availability::NotYet(at) => json!({ "available": false, "from": at }),
                    crate::catalog::Availability::Unknown(why) | crate::catalog::Availability::Unsupported(why) => json!({ "available": false, "reason": why }),
                },
                "download": download,
            })
        }
        other => {
            let key = other.series_key().ok_or_else(|| {
                ApiError::bad_request(ErrorCode::InvalidMediaId, "unsupported media id")
            })?;
            let s = index.series(key).ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    ErrorCode::NotManaged,
                    format!(
                        "{id} is not in the managed catalog (add it with POST /api/v1/library)"
                    ),
                )
            })?;
            let only = match other {
                MediaId::Season(_, n) | MediaId::Episode(_, n, _) => Some(n),
                _ => None,
            };
            let mut v = json!({
                "media_id": id, "series": s.id, "kind": match other { MediaId::Season(..) => "season", MediaId::Episode(..) => "episode", _ => "series" },
                "title": s.title, "acquisition": s.acquisition, "monitored": s.monitored,
                "episode_list": if s.episodes.is_some() { "known" } else { "unknown" },
                "metadata_hold": s.metadata_hold,
                "native_retained": Vec::<Value>::new(),
                "seasons": season_summaries(s, now, only),
            });
            if only.is_some() {
                let aired = |e: &crate::catalog::Episode| {
                    e.air_date_utc
                        .as_deref()
                        .and_then(|d| OffsetDateTime::parse(d, &Rfc3339).ok())
                        .is_some_and(|t| t <= now)
                };
                let eps: Vec<Value> = s
                    .episodes
                    .iter()
                    .flatten()
                    .filter(|e| match other {
                        MediaId::Season(_, n) => e.season == n,
                        MediaId::Episode(_, n, x) => e.season == n && e.number == x,
                        _ => false,
                    })
                    .map(|e| {
                        let eid = library::episode_id(s.id, e.season, e.number).to_string();
                        let download = active.get(&eid).map(|g| grab_brief(g));
                        let state = if e.has_file {
                            "owned"
                        } else if download.is_some() || e.queued {
                            "downloading"
                        } else if e.ownership_unknown {
                            "unknown"
                        } else if !aired(e) {
                            "unaired"
                        } else {
                            "missing"
                        };
                        json!({
                            "media_id": eid, "season": e.season, "episode": e.number, "title": e.title,
                            "air_date_utc": e.air_date_utc, "monitored": e.monitored && s.monitored,
                            "state": state, "download": download,
                            "file": e.file.as_ref().map(|f| json!({ "path": f.path, "quality": f.quality, "source": f.source })),
                        })
                    })
                    .collect();
                v["episodes"] = json!(eps);
            }
            v
        }
    };
    let scoped: Vec<downloads::Grab> = grabs
        .into_iter()
        // A download belongs to a scope through what it actually covers, not through the
        // (possibly broader) target of the plan that created it.
        .filter(|g| g.targets.iter().any(|t| in_scope(t.target)))
        .collect();
    let scoped_requests: Vec<Value> = requests
        .into_iter()
        .filter(|r| parsed(&r["target"]).is_some_and(in_scope))
        .collect();
    body["downloads"] = json!(scoped.iter().map(grab_brief).collect::<Vec<_>>());
    body["requests"] = json!(scoped_requests.iter().map(request_view).collect::<Vec<_>>());
    let holds: Vec<Value> = match id {
        MediaId::Movie { .. } => vec![],
        other => other
            .series_key()
            .and_then(|k| index.series(k))
            .map(hold_item)
            .into_iter()
            .flatten()
            .collect(),
    };
    // Failures another download has made good stay in the history, not in attention.
    let needs: Vec<downloads::Grab> = scoped
        .iter()
        .filter(|g| {
            !downloads::made_good(g, |t| owned_now(&index, t), &active_cover)
                && !downloads::not_wanted(g, |t| downloads::wanted_now(&index, t))
        })
        .cloned()
        .collect();
    body["attention"] = json!(attention_items(&needs, &scoped_requests, holds));
    body["observed_at"] = json!(rfc3339(now));
    ok(body)
}

/// Everything needing attention, across the catalog (bounded): blocked imports, failures of
/// the last 7 days, requests retrying 3+ times or expired in the last 14 days.
async fn attention(State(st): St) -> ApiResult {
    st.loaded()?;
    let now = OffsetDateTime::now_utc();
    let items = attention_list(&st, now, true, false).await?;
    ok(json!({ "total": items.len(), "items": items, "observed_at": rfc3339(now) }))
}

async fn attention_list(
    st: &AppState,
    now: OffsetDateTime,
    with_holds: bool,
    collapse: bool,
) -> Result<Vec<Value>, ApiError> {
    let since = rfc3339(now - Duration::from_secs(7 * 86_400));
    let index = st.index();
    let grabs = st
        .store
        .with(move |c| {
            // Failures another download has made good are left out before the bound, so they
            // never crowd out something that needs a person.
            let active = downloads::active_coverage(c)?;
            let mut kept = downloads::select(
                c,
                "active = 1 AND state = 'import_blocked' ORDER BY updated_at DESC LIMIT 100",
                &[],
            )?;
            const PAGE: i64 = 200;
            let mut offset: i64 = 0;
            // With `collapse`, a failure shows once per wanted episode or movie: the most
            // recent one (newest first), the older ones only raising its `count`. Done before
            // the bound too, so repeats never crowd out a different problem.
            let mut shown: HashMap<MediaId, String> = HashMap::new();
            let mut counts: HashMap<String, i64> = HashMap::new();
            while kept.len() < 100 {
                let page = downloads::select(
                    c,
                    "state = 'failed' AND updated_at >= ?1 ORDER BY updated_at DESC, rowid DESC LIMIT ?2 OFFSET ?3",
                    &[&since, &PAGE, &offset],
                )?;
                let n = page.len() as i64;
                for g in page.into_iter().filter(|g| {
                    !downloads::made_good(g, |t| owned_now(&index, t), &active)
                        && !downloads::not_wanted(g, |t| downloads::wanted_now(&index, t))
                }) {
                    if kept.len() >= 100 {
                        break;
                    }
                    if collapse
                        && downloads::fold_failure(&mut shown, &mut counts, &g.id, &covered_by(&g))
                    {
                        continue;
                    }
                    kept.push(g);
                }
                if n < PAGE {
                    break;
                }
                offset += PAGE;
            }
            kept.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            kept.truncate(100);
            Ok((kept, counts))
        })
        .await
        .map_err(internal("grabs"))?;
    let (grabs, counts) = grabs;
    let horizon = rfc3339(now - Duration::from_secs(14 * 86_400));
    let requests: Vec<Value> = st
        .store
        .with(|c| worker::list_requests(c, false))
        .await
        .map_err(internal("requests"))?
        .into_iter()
        .filter(|r| {
            r["state"] != "expired"
                || r["updated_at"]
                    .as_str()
                    .is_some_and(|u| u >= horizon.as_str())
        })
        .collect();
    let holds: Vec<Value> = if with_holds {
        st.index()
            .catalog
            .series
            .iter()
            .filter_map(hold_item)
            .collect()
    } else {
        vec![]
    };
    let mut items = attention_items(&grabs, &requests, holds);
    if collapse {
        for i in &mut items {
            if let Some(n) = i["grab_id"].as_str().and_then(|g| counts.get(g)) {
                i["count"] = json!(n);
            }
        }
    }
    Ok(items)
}

// ---------------------------------------------------------------- web app: feed, ideas, quick add

/// A title and year for the app, from the catalog; a plain placeholder when it is unknown.
fn feed_title(index: &CatalogIndex, id: MediaId) -> (String, Option<u16>) {
    match id {
        MediaId::Movie { tmdb } => index
            .movie(tmdb)
            .map(|m| (m.title.clone(), m.year))
            .unwrap_or_else(|| (format!("Movie {tmdb}"), None)),
        other => other
            .series_key()
            .and_then(|k| {
                index
                    .series(k)
                    .map(|s| (s.title.clone(), s.year))
                    .or_else(|| {
                        let (SeriesKey::Tvdb(n) | SeriesKey::Tvmaze(n)) = k;
                        Some((format!("Show {n}"), None))
                    })
            })
            .unwrap_or_else(|| ("Unknown title".into(), None)),
    }
}

/// "Season 2", "Seasons 1–3", "S4 E1 · Title", or none for a movie or a whole series.
fn feed_detail(index: &CatalogIndex, targets: &[MediaId]) -> Option<String> {
    let mut seasons = std::collections::BTreeSet::new();
    for t in targets {
        match *t {
            MediaId::Season(_, s) | MediaId::Episode(_, s, _) => {
                seasons.insert(s);
            }
            _ => {}
        }
    }
    if let [MediaId::Episode(k, s, e)] = targets {
        let name = index
            .series(*k)
            .and_then(|x| x.episodes.as_ref())
            .and_then(|eps| eps.iter().find(|x| x.season == *s && x.number == *e))
            .and_then(|x| x.title.clone())
            .filter(|t| !t.trim().is_empty());
        return Some(match name {
            Some(n) => format!("S{s} E{e} \u{b7} {n}"),
            None => format!("S{s} E{e}"),
        });
    }
    let (first, last) = (*seasons.first()?, *seasons.last()?);
    Some(if first == last {
        format!("Season {first}")
    } else if seasons.len() == usize::from(last - first) + 1 {
        format!("Seasons {first}\u{2013}{last}")
    } else {
        let list: Vec<String> = seasons.iter().map(u16::to_string).collect();
        format!("Seasons {}", list.join(", "))
    })
}

/// Whether a download of `grab` is already looking after what `request` asks for.
fn download_covers(request: MediaId, grab: MediaId) -> bool {
    match (request, grab) {
        (MediaId::Series(a), g) => g.series_key() == Some(a),
        // An episode download covers only that episode: the season's other episodes may still be missing.
        (MediaId::Season(a, s), MediaId::Season(b, t)) => a == b && s == t,
        (MediaId::Episode(a, s, e), MediaId::Season(b, t)) => a == b && s == t && e > 0,
        (r, g) => r == g,
    }
}

async fn feed(State(st): St) -> ApiResult {
    st.loaded()?;
    let now = OffsetDateTime::now_utc();
    let index = st.index();
    // Arrived shows the last day only: the home is about what's new, Plex holds the rest.
    let since = rfc3339(now - Duration::from_secs(86_400));
    let (active, imported, requests) = st
        .store
        .with(move |c| {
            Ok((
                downloads::select(c, "active = 1 ORDER BY created_at LIMIT 200", &[])?,
                downloads::select(
                    c,
                    "state = 'imported' AND imported_at >= ?1 ORDER BY imported_at DESC LIMIT 20",
                    &[&since],
                )?,
                worker::list_requests(c, true)?,
            ))
        })
        .await
        .map_err(internal("feed"))?;
    let targets_of = |g: &downloads::Grab| -> Vec<MediaId> {
        let t: Vec<MediaId> = g.targets.iter().map(|t| t.target).collect();
        if t.is_empty() { vec![g.target] } else { t }
    };
    let entry = |id: MediaId, targets: &[MediaId]| {
        let (title, year) = feed_title(&index, id);
        (title, year, feed_detail(&index, targets))
    };

    let on_its_way: Vec<Value> = active
        .iter()
        .filter_map(|g| {
            let stage = match g.state.as_str() {
                "queued" | "unconfirmed" => "found",
                "downloading" => "downloading",
                "completed" | "importing" => "checking",
                _ => return None,
            };
            let targets = targets_of(g);
            let (title, year, detail) = entry(targets[0], &targets);
            let paused = g.user_paused || g.client_state.as_deref() == Some("paused");
            // Time left, from the download rate; none when unknown, paused or stalled.
            let eta = (stage == "downloading" && !paused && g.client_state.as_deref() == Some("downloading") && g.stalled_since.is_none())
                .then(|| st.worker.eta_seconds(&g.id))
                .flatten();
            Some(json!({
                "grab_id": g.id, "media_id": targets[0], "title": title, "year": year, "detail": detail,
                "stage": stage,
                "progress": (stage == "downloading").then_some(g.progress).flatten().map(|p| p.clamp(0.0, 1.0)),
                "paused": paused,
                "eta_seconds": eta,
            }))
        })
        .collect();

    let arrived: Vec<Value> = imported
        .iter()
        .map(|g| {
            let targets = targets_of(g);
            let (title, year, detail) = entry(targets[0], &targets);
            json!({ "media_id": targets[0], "title": title, "year": year, "detail": detail, "at": g.imported_at })
        })
        .collect();

    let needs_you: Vec<Value> = attention_list(&st, now, false, true)
        .await?
        .into_iter()
        .filter_map(|a| {
            let kind = a["kind"].as_str()?;
            let problem = match kind {
                "download_failed" => "The download didn't work",
                "import_blocked" => "Arr couldn't add the file to Plex",
                "request_expired" => "Arr couldn't find a copy to download",
                _ => return None,
            };
            let target: MediaId = a["target"].as_str()?.parse().ok()?;
            let grab_id = a["grab_id"].as_str();
            // A request is retried by adding the title again, which takes a movie or a series.
            let media_id = match (grab_id, target) {
                (None, t @ MediaId::Movie { .. }) => t,
                (None, t) => MediaId::Series(t.series_key()?),
                (Some(_), t) => t,
            };
            // A download made for a season plan names the episodes it really covered.
            let covers: Vec<MediaId> = a["covers"]
                .as_array()
                .map(|v| v.iter().filter_map(|x| x.as_str()?.parse().ok()).collect())
                .filter(|v: &Vec<MediaId>| !v.is_empty())
                .unwrap_or_else(|| vec![target]);
            let (title, year, detail) = entry(target, &covers);
            Some(json!({
                "key": format!("{kind}:{}", grab_id.map_or_else(|| target.to_string(), str::to_string)),
                "media_id": media_id, "target": target, "title": title, "year": year, "detail": detail,
                "problem": problem, "grab_id": grab_id,
                "kind": kind, "failure": a.get("failure").cloned().unwrap_or(Value::Null),
                "count": a.get("count").cloned().unwrap_or(json!(1)),
                "at": a.get("at").or_else(|| a.get("since")).cloned().unwrap_or(Value::Null),
            }))
        })
        .collect();

    let covered: Vec<MediaId> = active.iter().flat_map(&targets_of).collect();
    let today = now.date().to_string();
    let looking: Vec<Value> = requests
        .iter()
        .filter_map(|r| {
            let target: MediaId = r["target"].as_str()?.parse().ok()?;
            if covered.iter().any(|g| download_covers(target, *g)) {
                return None;
            }
            let (title, year, detail) = entry(target, &[target]);
            let until = match target {
                MediaId::Movie { tmdb } => index
                    .movie(tmdb)
                    .and_then(|m| m.digital_release.as_deref())
                    .and_then(|d| d.get(..10))
                    .filter(|d| *d > today.as_str())
                    .map(str::to_string),
                _ => None,
            };
            Some(json!({
                "media_id": target, "title": title, "year": year, "detail": detail,
                "until": until, "next_try_at": r["next_attempt_at"],
            }))
        })
        .collect();

    let coming_up: Vec<Value> =
        library::calendar(&index, now, now + Duration::from_secs(14 * 86_400), now)
            .into_iter()
            .filter(|e| e.monitored && e.status == "upcoming")
            .take(20)
            .map(|e| {
                let (title, year) = feed_title(&index, e.series);
                json!({
                    "media_id": e.target, "title": title, "year": year,
                    "detail": feed_detail(&index, &[e.target]), "at": e.air_date_utc,
                })
            })
            .collect();

    ok(json!({
        "generated_at": rfc3339(now),
        "movies_enabled": st.movies.is_some(),
        "tmdb_backup": st.movies.as_ref().is_some_and(Movies::has_tmdb),
        "needs_you": needs_you,
        "on_its_way": on_its_way,
        "arrived": arrived,
        "looking": looking,
        "coming_up": coming_up,
    }))
}

/// The longest a page waits for review scores; whatever is ready by then is shown.
pub(crate) const SCORES_BUDGET: Duration = Duration::from_millis(1500);

/// Where a movie stands in the library, in one word: `in_plex`, `on_its_way`, `tracking`
/// (Arr is looking for it), or none.
fn movie_state(m: &crate::catalog::Movie) -> Option<&'static str> {
    if m.has_file {
        Some("in_plex")
    } else if m.queued {
        Some("on_its_way")
    } else if m.monitored && managed(&m.provenance.source_app) {
        Some("tracking")
    } else {
        None
    }
}

/// The same for a show: some episodes in Plex, some on their way, or only tracked.
fn series_state(s: &crate::catalog::Series) -> Option<&'static str> {
    // Current episode ownership wins; imported season counts are only for when it is unavailable.
    let owned = match s.episodes.as_ref() {
        Some(e) => e.iter().any(|e| e.has_file),
        None => s
            .seasons
            .iter()
            .any(|x| x.number > 0 && x.episode_file_count.unwrap_or(0) > 0),
    };
    if owned {
        Some("in_plex")
    } else if s
        .episodes
        .as_ref()
        .is_some_and(|e| e.iter().any(|e| e.queued))
    {
        Some("on_its_way")
    } else if s.monitored && managed(&s.provenance.source_app) {
        Some("tracking")
    } else {
        None
    }
}

/// The shows people said they are not interested in.
async fn hidden_ideas(st: &AppState) -> Result<std::collections::HashSet<String>, ApiError> {
    let dir = st.cfg.state_dir.clone();
    tokio::task::spawn_blocking(move || crate::webapp::hidden_ideas(&dir))
        .await
        .map_err(|e| ApiError::internal("ideas", e.to_string()))?
        .map_err(internal("ideas"))
}

/// Popular shows with new episodes this week (TVmaze web schedule), without the ones people
/// hid. Each item says whether the show is already here (`owned`, `library_state`).
async fn ideas(State(st): St) -> ApiResult {
    let Some(tv) = &st.tvmaze else {
        return ok(json!({ "status": "disabled", "items": [] }));
    };
    let Ok(shows) = tv.ideas().await else {
        return ok(json!({ "status": "unavailable", "items": [] }));
    };
    let hidden = hidden_ideas(&st).await.unwrap_or_default();
    let index = st.index();
    let refs: Vec<(Option<u32>, Option<String>)> =
        shows.iter().map(|s| (s.tvdb, s.imdb.clone())).collect();
    let scores = match &st.movies {
        Some(m) => m.show_scores(&refs, SCORES_BUDGET).await,
        None => vec![None; shows.len()],
    };
    let items: Vec<Value> = shows
        .iter()
        .zip(scores)
        .filter(|(s, _)| {
            let tvmaze = MediaId::Series(SeriesKey::Tvmaze(s.id)).to_string();
            let tvdb = s
                .tvdb
                .map(|t| MediaId::Series(SeriesKey::Tvdb(t)).to_string());
            !hidden.contains(&tvmaze) && !tvdb.is_some_and(|t| hidden.contains(&t))
        })
        .map(|(s, scores)| {
            let key = s.tvdb.map_or(SeriesKey::Tvmaze(s.id), SeriesKey::Tvdb);
            let found: Vec<&crate::catalog::Series> = [
                SeriesKey::Tvdb(s.tvdb.unwrap_or(0)),
                SeriesKey::Tvmaze(s.id),
            ]
            .into_iter()
            .filter_map(|k| index.series(k))
            .collect();
            let in_library = found
                .iter()
                .any(|x| x.monitored && managed(&x.provenance.source_app));
            let state = found.iter().find_map(|x| series_state(x));
            json!({
                "media_id": MediaId::Series(key), "title": s.name, "year": s.year,
                "network": s.network, "in_library": in_library, "summary": s.summary,
                "rating": s.rating, "scores": metadata::scores_json(scores), "genres": s.genres,
                "owned": in_library || state.is_some(), "library_state": state,
            })
        })
        .collect();
    ok(json!({ "status": "ok", "items": items }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HideBody {
    media_id: String,
}

fn idea_id(raw: &str) -> Result<String, ApiError> {
    match parse_media_id(raw)? {
        id @ MediaId::Series(_) => Ok(id.to_string()),
        _ => Err(ApiError::bad_request(
            ErrorCode::InvalidMediaId,
            "expected a series id",
        )),
    }
}

async fn change_hidden(st: &AppState, id: String, hide: bool) -> Result<(), ApiError> {
    let dir = st.cfg.state_dir.clone();
    tokio::task::spawn_blocking(move || {
        if hide {
            crate::webapp::hide_idea(&dir, &id)
        } else {
            crate::webapp::unhide_idea(&dir, &id)
        }
    })
    .await
    .map_err(|e| ApiError::internal("ideas", e.to_string()))?
    .map_err(internal("ideas"))
}

/// `POST /ideas/hide`: "Not interested". Hides the show from Ideas for everyone using this
/// Arr. It changes nothing else: no download, no file, no library entry.
async fn ideas_hide(State(st): St, ApiJson(body): ApiJson<HideBody>) -> ApiResult {
    let id = idea_id(&body.media_id)?;
    change_hidden(&st, id.clone(), true).await?;
    ok(json!({ "media_id": id, "hidden": true }))
}

/// `DELETE /ideas/hide/{media_id}`: show it in Ideas again (also fine if it was not hidden).
async fn ideas_unhide(State(st): St, p: Result<Path<String>, PathRejection>) -> ApiResult {
    let Path(raw) =
        p.map_err(|e| ApiError::bad_request(ErrorCode::InvalidMediaId, e.body_text()))?;
    let id = idea_id(&raw)?;
    change_hidden(&st, id.clone(), false).await?;
    ok(json!({ "media_id": id, "hidden": false }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuickAddBody {
    media_id: String,
    /// smaller, standard or best: the quality the person picked on the page.
    #[serde(default)]
    level: Option<String>,
}

/// `POST /library` with the defaults, in-process; the reply's JSON.
async fn add_value(
    st: &Arc<AppState>,
    media_id: String,
    monitor: Option<MonitorMode>,
    search: bool,
    profile: Option<String>,
) -> Result<Value, ApiError> {
    let body = AddBody {
        media_id,
        monitored: None,
        monitor,
        quality_profile: profile,
        root_folder: None,
        search,
        metadata: None,
        native_handoff_confirmed: None,
        preserve_monitoring: None,
    };
    let resp = library_add(State(st.clone()), ApiJson(body)).await?;
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .map_err(|_| ApiError::internal("quick add", "unreadable reply"))?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::internal("quick add", "unreadable reply"))
}

/// Day and month for a sentence ("12 Dec"), from `YYYY-MM-DD`.
fn short_date(d: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = d
        .get(5..7)
        .and_then(|m| m.parse::<usize>().ok())
        .and_then(|m| MONTHS.get(m.wrapping_sub(1)));
    match (month, d.get(8..10).and_then(|x| x.parse::<u8>().ok())) {
        (Some(m), Some(day)) => format!("{day} {m}"),
        _ => d.to_string(),
    }
}

/// What an add did, in the app's words, from the add reply (mirrors the MCP `automatic` summary).
fn quick_outcome(v: &Value) -> (&'static str, Option<String>, String) {
    let state = v["plan"]["state"].as_str().unwrap_or("");
    let ex = &v["plan"]["execution"];
    let queued = ex["mode"] == "executed" && ex["grabs"].as_array().is_some_and(|g| !g.is_empty());
    if queued
        || matches!(
            state,
            "download_queued" | "download_started" | "in_progress"
        )
    {
        return (
            "on_its_way",
            None,
            "Arr found a copy and is getting it now.".into(),
        );
    }
    if matches!(state, "already_satisfied" | "imported") {
        return ("in_plex", None, "It's already in Plex.".into());
    }
    let until = v["acquisition_blockers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["code"] == "not_released_yet")
        .map(|b| {
            b["until"]
                .as_str()
                .and_then(|u| u.get(..10))
                .map(str::to_string)
        });
    if let Some(until) = until {
        let message = match &until {
            Some(d) => format!("Not out yet. Arr will get it after {}.", short_date(d)),
            None => "Not out yet. Arr will get it once it's out.".into(),
        };
        return ("waiting", until, message);
    }
    (
        "looking",
        None,
        "Arr is looking for a copy and will keep trying.".into(),
    )
}

/// One tap to get a movie, a show, a season or an episode: what a person means by "get me this".
async fn quick_add(State(st): St, ApiJson(body): ApiJson<QuickAddBody>) -> ApiResult {
    st.loaded()?;
    let id = parse_media_id(&body.media_id)?;
    // The picked level applies to a title Arr already has, and to one it adds now.
    let profile = match &body.level {
        Some(l) => Some(options::level_profile(&st, id, l)?),
        None => None,
    };
    if let Some(p) = &profile {
        options::apply_profile(&st, id, p).await?;
    }
    match id {
        MediaId::Movie { tmdb } => {
            let v = match add_value(&st, id.to_string(), None, true, profile).await {
                Ok(v) => v,
                Err(e)
                    if e.status == StatusCode::CONFLICT
                        && e.code == ErrorCode::MetadataUnavailable
                        && st.movies.is_none() =>
                {
                    return ok(json!({
                        "media_id": id, "title": format!("Movie {tmdb}"), "detail": null,
                        "outcome": "movies_off", "until": null,
                        "message": "Movie search isn't set up yet. The person who runs Arr needs to sign in to Plex again.",
                    }));
                }
                Err(e) => return Err(e),
            };
            let (outcome, until, message) = quick_outcome(&v);
            let title = feed_title(&st.index(), id).0;
            ok(json!({
                "media_id": id, "title": title, "detail": null,
                "outcome": outcome, "until": until, "message": message,
            }))
        }
        MediaId::Series(_) => {
            let v = add_value(
                &st,
                id.to_string(),
                Some(MonitorMode::Future),
                false,
                profile.clone(),
            )
            .await?;
            let sid: MediaId = v["media_id"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(id);
            let index = st.index();
            let (title, _) = feed_title(&index, sid);
            let now = OffsetDateTime::now_utc();
            let latest = sid
                .series_key()
                .and_then(|k| index.series(k))
                .and_then(|s| {
                    s.episodes
                        .iter()
                        .flatten()
                        .filter(|e| e.season > 0)
                        .filter(|e| {
                            e.air_date_utc.as_deref().is_some_and(|d| {
                                OffsetDateTime::parse(d, &Rfc3339).is_ok_and(|t| t <= now)
                            })
                        })
                        .map(|e| e.season)
                        .max()
                });
            let (Some(season), Some(key)) = (latest, sid.series_key()) else {
                return ok(json!({
                    "media_id": sid, "title": title, "detail": null,
                    "outcome": "following", "until": null,
                    "message": "Arr will get new episodes as they air.",
                }));
            };
            let v = add_value(
                &st,
                MediaId::Season(key, season).to_string(),
                None,
                true,
                profile,
            )
            .await?;
            let (outcome, until, message) = quick_outcome(&v);
            ok(json!({
                "media_id": sid, "title": title, "detail": format!("Season {season}"),
                "outcome": outcome, "until": until, "message": message,
            }))
        }
        MediaId::Season(..) | MediaId::Episode(..) => titles::quick_scope(&st, id, profile).await,
    }
}
