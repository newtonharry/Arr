//! Native indexers: the registry and provider contract that release search, download
//! resolution, RSS, diagnostics and readiness share when `[[indexer]]` entries are configured
//! (instead of `[prowlarr]`; never both, and never a fallback between them).
//!
//! Three providers exist: The Pirate Bay's JSON API (`tpb`), Knaben's JSON search API
//! (`knaben`, an aggregate of public trackers) and generic Torznab (`torznab`).
//! Any other Prowlarr/Cardigann definition is refused, never approximated.
//!
//! Every request to an indexer goes through one gate per indexer: a provider-requested backoff
//! (HTTP 429 and `Retry-After`) refuses locally until it ends; a configured query limit is
//! counted durably in SQLite; then a concurrency permit and a minimum spacing between requests.
//! Bodies are bounded, redirects are never followed automatically, and errors never quote a
//! URL, a credential or a response body. Per-indexer health is kept for diagnostics.
//!
//! Settings name their indexers by id: the setup's `[[indexers]] id = N` is served by the
//! native indexer with `id = N`, but only when the native provider implements what the
//! settings record (its Prowlarr definition, site settings and query/grab limits). Anything
//! else is reported unusable (fail closed).

use crate::config::{IndexerConfig, IndexerKind};
use crate::import::rfc3339;
use crate::prowlarr::{Payload, ProwlarrClient, ProwlarrRelease, SearchOutcome, SearchRequest};
use crate::settings::{IndexerSetting, Settings};
use crate::store::Store;
use crate::upstream::{self, TtlCache, UpstreamError};
use crate::{knaben, torznab, tpb};
use rusqlite::params;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::sync::Semaphore;

/// The normalised release record every backend produces (Prowlarr's search schema).
pub type Release = ProwlarrRelease;

/// Longest backoff a provider's `Retry-After` can impose.
const MAX_BACKOFF: Duration = Duration::from_secs(6 * 3600);
/// Backoff after an HTTP 429 without a usable `Retry-After` (doubles per repeat).
const DEFAULT_BACKOFF: Duration = Duration::from_secs(60);

/// A window of an indexer's latest releases (RSS).
#[derive(Debug, Default)]
pub struct FeedWindow {
    /// Items in the requested categories.
    pub items: Vec<Release>,
    /// Items the indexer returned in its window, before category filtering.
    pub window: usize,
    /// Oldest and newest publish dates over the whole window (before category filtering):
    /// the gap check compares them with the last sync, never the item count, because a feed
    /// may return fewer rows than its nominal limit (TPB's recent feed does) and a feed shared
    /// across categories fills faster than any one.
    pub oldest: Option<String>,
    pub newest: Option<String>,
    pub links_dropped: usize,
    pub dropped: usize,
    pub truncated: bool,
}

/// Per-indexer health, as observed by this process.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Health {
    /// `unknown` (not used yet), `ok`, `failing` or `backoff`.
    pub status: &'static str,
    pub requests: u64,
    pub failures: u64,
    pub consecutive_failures: u32,
    pub rate_limited: u64,
    pub last_success_at: Option<String>,
    pub last_failure_at: Option<String>,
    pub last_error: Option<String>,
    pub backoff_until: Option<String>,
    /// The last time the indexer explicitly answered "no results".
    pub last_confirmed_empty_at: Option<String>,
}

struct Backoff {
    until: Instant,
    wall: OffsetDateTime,
    reason: String,
    repeats: u32,
}

/// One configured native indexer.
pub struct Indexer {
    pub cfg: IndexerConfig,
    http: reqwest::Client,
    permits: Semaphore,
    next_slot: tokio::sync::Mutex<Option<Instant>>,
    backoff: Mutex<Option<Backoff>>,
    health: Mutex<Health>,
    cache: TtlCache<SearchRequest, SearchOutcome>,
    pub(crate) caps: Mutex<Option<(Instant, torznab::Caps)>>,
    store: Store,
}

impl Indexer {
    fn new(cfg: IndexerConfig, store: Store) -> Self {
        Indexer {
            http: upstream::http_client(cfg.timeout, reqwest::redirect::Policy::none()),
            permits: Semaphore::new(cfg.max_concurrent),
            next_slot: tokio::sync::Mutex::new(None),
            backoff: Mutex::new(None),
            health: Mutex::new(Health {
                status: "unknown",
                ..Default::default()
            }),
            cache: TtlCache::new(cfg.cache_ttl, 256),
            caps: Mutex::new(None),
            store,
            cfg,
        }
    }

    /// `indexer N (name)`, the provider label used in errors and logs.
    pub fn label(&self) -> String {
        format!("indexer {} ({})", self.cfg.id, self.cfg.name)
    }

    fn backoff_error(&self) -> Option<UpstreamError> {
        let b = self.backoff.lock().unwrap_or_else(|e| e.into_inner());
        b.as_ref()
            .filter(|b| b.until > Instant::now())
            .map(|b| UpstreamError::Throttled {
                reason: format!("{} until {}", b.reason, rfc3339(b.wall)),
            })
    }

    /// Counts a query against the configured limit (durably), refusing when it is used up.
    async fn admit_query(&self) -> Result<(), UpstreamError> {
        let Some(limit) = self.cfg.query_limit else {
            return Ok(());
        };
        let (id, window) = (self.cfg.id, self.cfg.limits_window);
        let now = OffsetDateTime::now_utc();
        let since = rfc3339(now - window);
        let now_s = rfc3339(now);
        let prune = rfc3339(now - Duration::from_secs(2 * 86_400));
        let admitted = self
            .store
            .with(move |c| {
                let used: i64 = c.query_row(
                    "SELECT COUNT(*) FROM indexer_usage WHERE indexer_id = ?1 AND kind = 'query' AND at > ?2",
                    params![id, since],
                    |r| r.get(0),
                )?;
                if used >= i64::from(limit) {
                    return Ok(false);
                }
                c.execute(
                    "INSERT INTO indexer_usage (indexer_id, kind, at) VALUES (?1, 'query', ?2)",
                    params![id, now_s],
                )?;
                c.execute("DELETE FROM indexer_usage WHERE at < ?1", [prune])?;
                Ok(true)
            })
            .await
            .map_err(|_| UpstreamError::Unavailable {
                reason: "query limit accounting failed".into(),
            })?;
        if admitted {
            Ok(())
        } else {
            Err(UpstreamError::Throttled {
                reason: format!(
                    "the configured query limit ({limit} per {}) is used up",
                    window_name(window)
                ),
            })
        }
    }

    /// One request through the gate. A provider backoff refuses at once; otherwise the
    /// request waits for a concurrency permit and its spacing slot, then the backoff is
    /// checked again (another request may have been told to wait meanwhile), and only then is
    /// a query counted against the configured limit (a request that never goes out is not
    /// charged). An HTTP 429 records a backoff (from `Retry-After` when present) and fails.
    pub(crate) async fn send(
        &self,
        req: reqwest::RequestBuilder,
        query: bool,
    ) -> Result<reqwest::Response, UpstreamError> {
        if let Some(e) = self.backoff_error() {
            return Err(e);
        }
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| UpstreamError::Unavailable {
                reason: "client shutting down".into(),
            })?;
        {
            let mut next = self.next_slot.lock().await;
            if let Some(at) = *next
                && at > Instant::now()
            {
                tokio::time::sleep_until(at.into()).await;
            }
            *next = Some(Instant::now() + self.cfg.min_interval);
        }
        if let Some(e) = self.backoff_error() {
            return Err(e);
        }
        if query {
            self.admit_query().await?;
        }
        self.bump(|h| h.requests += 1);
        let resp = upstream::send(req).await?;
        if resp.status().as_u16() == 429 {
            let after = upstream::retry_after(resp.headers());
            let mut b = self.backoff.lock().unwrap_or_else(|e| e.into_inner());
            let repeats = b.as_ref().map_or(0, |b| b.repeats + 1);
            let wait = after
                .unwrap_or(DEFAULT_BACKOFF * (1u32 << repeats.min(6)))
                .min(MAX_BACKOFF);
            let wall = OffsetDateTime::now_utc() + wait;
            *b = Some(Backoff {
                until: Instant::now() + wait,
                wall,
                reason: if after.is_some() {
                    "the indexer asked to wait (HTTP 429 Retry-After)".into()
                } else {
                    "the indexer rate limited a request (HTTP 429)".into()
                },
                repeats,
            });
            drop(b);
            self.bump(|h| {
                h.rate_limited += 1;
                h.backoff_until = Some(rfc3339(wall));
            });
            return Err(UpstreamError::RateLimited);
        }
        Ok(resp)
    }

    fn bump(&self, f: impl FnOnce(&mut Health)) {
        let mut h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut h);
    }

    /// Records the outcome of one operation (search, feed, caps or download).
    pub(crate) fn record(&self, r: Result<bool, &UpstreamError>) {
        let now = rfc3339(OffsetDateTime::now_utc());
        let backoff = self.backoff_error().is_some();
        self.bump(|h| match r {
            Ok(confirmed_empty) => {
                h.consecutive_failures = 0;
                h.last_success_at = Some(now.clone());
                if confirmed_empty {
                    h.last_confirmed_empty_at = Some(now);
                }
                h.status = "ok";
            }
            Err(e) => {
                h.failures += 1;
                h.consecutive_failures += 1;
                h.last_failure_at = Some(now);
                h.last_error = Some(e.describe());
                h.status = if backoff { "backoff" } else { "failing" };
            }
        });
    }

    pub fn health(&self) -> Health {
        let mut h = self
            .health
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if self.backoff_error().is_some() {
            h.status = "backoff";
        } else {
            h.backoff_until = None;
            if h.status == "backoff" {
                h.status = "failing";
            }
        }
        h
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// One query (cached unless `refresh`; failures and partial answers are never cached).
    pub async fn search(
        &self,
        req: &SearchRequest,
        refresh: bool,
    ) -> Result<SearchOutcome, UpstreamError> {
        if !refresh && let Some(mut hit) = self.cache.get(req) {
            hit.from_cache = true;
            return Ok(hit);
        }
        let r = match self.cfg.kind {
            IndexerKind::ThePirateBay => tpb::search(self, req).await,
            IndexerKind::Knaben => knaben::search(self, req).await,
            IndexerKind::Torznab => torznab::search(self, req).await,
        };
        self.record(r.as_ref().map(|o| o.confirmed_empty));
        let outcome = r?;
        if outcome.partial_error.is_none() {
            self.cache.insert(req.clone(), outcome.clone());
        }
        Ok(outcome)
    }

    /// The latest releases in `categories` (no query), for RSS.
    pub async fn feed(&self, categories: &[u32], limit: u32) -> Result<FeedWindow, UpstreamError> {
        let r = match self.cfg.kind {
            IndexerKind::ThePirateBay => tpb::feed(self, categories).await,
            IndexerKind::Knaben => knaben::feed(self, categories).await,
            IndexerKind::Torznab => torznab::feed(self, categories, limit).await,
        };
        self.record(r.as_ref().map(|f| f.window == 0));
        r
    }

    /// Resolves one of this indexer's own download links (never a URL a caller chose).
    pub async fn fetch(&self, url: &str) -> Result<Payload, UpstreamError> {
        let r = match self.cfg.kind {
            IndexerKind::ThePirateBay => Err(UpstreamError::BadResponse {
                reason: "The Pirate Bay releases are magnets built from their info-hash; no download link is fetched".into(),
            }),
            IndexerKind::Knaben => Err(UpstreamError::BadResponse {
                reason: "Knaben releases are magnets built from their info-hash; no download link is fetched".into(),
            }),
            IndexerKind::Torznab => torznab::fetch(self, url).await,
        };
        self.record(r.as_ref().map(|_| false));
        r
    }

    /// Whether this indexer can serve RSS (the reason when not).
    pub fn rss_unsupported(&self) -> Option<String> {
        match self.cfg.kind {
            IndexerKind::ThePirateBay if !self.cfg.recent_feed => Some(
                "recent_feed = false: the top-100 recent feed is not read for this indexer".into(),
            ),
            IndexerKind::Knaben if !self.cfg.recent_feed => Some(
                "recent_feed = false: the latest releases are not read for this indexer".into(),
            ),
            _ => None,
        }
    }

    /// Diagnostics view (no credential, no path beyond the configured origin).
    pub fn status(&self) -> Value {
        let caps = self
            .caps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(_, c)| c.summary());
        json!({
            "id": self.cfg.id,
            "name": self.cfg.name,
            "kind": self.cfg.kind,
            "origin": self.cfg.base_url.origin().ascii_serialization(),
            "api_key": self.cfg.api_key.is_some(),
            "download_origins": self.cfg.download_origins.iter().map(|u| u.origin().ascii_serialization()).collect::<Vec<_>>(),
            "recent_feed": self.cfg.kind.builds_magnets() && self.cfg.recent_feed,
            "magnet_trackers": self.cfg.magnet_trackers.len(),
            "uploaders_filter": self.cfg.uploaders.len(),
            "limits": {
                "max_concurrent": self.cfg.max_concurrent,
                "min_interval_ms": self.cfg.min_interval.as_millis() as u64,
                "timeout_secs": self.cfg.timeout.as_secs(),
                "max_response_bytes": self.cfg.max_response_bytes,
                "cache_ttl_secs": self.cfg.cache_ttl.as_secs(),
                "query_limit": self.cfg.query_limit,
                "grab_limit": self.cfg.grab_limit,
                "limits_unit": window_name(self.cfg.limits_window),
            },
            "caps": caps,
            "health": self.health(),
        })
    }
}

fn window_name(w: Duration) -> &'static str {
    if w <= Duration::from_secs(3600) {
        "hour"
    } else {
        "day"
    }
}

/// Whether imported settings for an id can be served natively, and why not.
#[derive(Debug, Clone, Serialize)]
pub struct Mapping {
    pub indexer_id: u32,
    /// Imported names (Prowlarr name; Radarr/Sonarr entries) for the id.
    pub imported: Vec<String>,
    pub definition: Option<String>,
    /// `usable`, `not_configured` (no native indexer has this id) or `unsupported`.
    pub status: &'static str,
    pub native: Option<String>,
    pub reason: Option<String>,
}

/// Every configured native indexer.
pub struct Registry {
    indexers: BTreeMap<u32, Arc<Indexer>>,
}

impl Registry {
    pub fn new(cfgs: &[IndexerConfig], store: Store) -> Self {
        Registry {
            indexers: cfgs
                .iter()
                .map(|c| (c.id, Arc::new(Indexer::new(c.clone(), store.clone()))))
                .collect(),
        }
    }

    pub fn get(&self, id: u32) -> Option<&Arc<Indexer>> {
        self.indexers.get(&id)
    }

    pub fn all(&self) -> impl Iterator<Item = &Arc<Indexer>> {
        self.indexers.values()
    }

    /// Why imported setting `ix` cannot be served natively (`None` when it can).
    pub fn unusable(&self, ix: &IndexerSetting) -> Option<(&'static str, String)> {
        let Some(native) = self.indexers.get(&ix.prowlarr_indexer_id) else {
            return Some((
                "not_configured",
                format!(
                    "no [[indexer]] with id {} is configured: '{}' is not searched",
                    ix.prowlarr_indexer_id, ix.name
                ),
            ));
        };
        validate_source(native, ix).map(|r| ("unsupported", r))
    }

    /// The native mapping of every imported indexer id.
    pub fn mapping(&self, settings: &Settings) -> Vec<Mapping> {
        let mut by_id: BTreeMap<u32, Vec<&IndexerSetting>> = BTreeMap::new();
        for ix in &settings.indexers {
            by_id.entry(ix.prowlarr_indexer_id).or_default().push(ix);
        }
        by_id
            .into_iter()
            .map(|(id, list)| {
                let mut reasons: Vec<(&'static str, String)> =
                    list.iter().filter_map(|ix| self.unusable(ix)).collect();
                reasons.dedup_by(|a, b| a.1 == b.1);
                let mut imported: Vec<String> = vec![list[0].name.clone()];
                imported.extend(
                    list.iter()
                        .map(|ix| format!("{} {}", ix.native.app, ix.native.name)),
                );
                Mapping {
                    indexer_id: id,
                    imported,
                    definition: list
                        .iter()
                        .find_map(|ix| ix.source.as_ref().and_then(|s| s.definition.clone())),
                    status: reasons.first().map_or("usable", |r| r.0),
                    native: self.indexers.get(&id).map(|n| n.label()),
                    reason: (!reasons.is_empty()).then(|| {
                        reasons
                            .into_iter()
                            .map(|r| r.1)
                            .collect::<Vec<_>>()
                            .join("; ")
                    }),
                }
            })
            .collect()
    }

    /// Grabs this indexer allows now (its configured grab limit), counted from the outbox.
    pub async fn admit_grab(&self, id: u32) -> Result<(), String> {
        let Some(ix) = self.indexers.get(&id) else {
            return Ok(());
        };
        let Some(limit) = ix.cfg.grab_limit else {
            return Ok(());
        };
        let window = ix.cfg.limits_window;
        let since = rfc3339(OffsetDateTime::now_utc() - window);
        let used: i64 = ix
            .store
            .with(move |c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM grabs WHERE indexer_id = ?1 AND created_at > ?2",
                    params![id, since],
                    |r| r.get(0),
                )?)
            })
            .await
            .map_err(|e| format!("grab limit accounting failed: {e:#}"))?;
        if used >= i64::from(limit) {
            return Err(format!(
                "{}: the configured grab limit ({limit} per {}) is used up",
                ix.label(),
                window_name(window)
            ));
        }
        Ok(())
    }

    pub fn status(&self, settings: Option<&Settings>) -> Value {
        json!({
            "backend": "native",
            "indexers": self.indexers.values().map(|i| i.status()).collect::<Vec<_>>(),
            "imported_mapping": settings.map(|s| self.mapping(s)),
        })
    }
}

/// The release search backend: native indexers or Prowlarr, exactly one (configuration
/// refuses both), with no fallback from one to the other.
#[derive(Clone)]
pub enum Backend {
    Native(Arc<Registry>),
    Prowlarr(Arc<ProwlarrClient>),
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Native(_) => "native",
            Backend::Prowlarr(_) => "prowlarr",
        }
    }

    /// The name the person configured for the indexer a release came from, if it can be told
    /// from trusted configuration: a native indexer's own configured name, found by its id.
    /// A release's own `indexer` text is upstream text and is never used for this; Prowlarr's
    /// indexer names are not read by Arr, so a Prowlarr release has none.
    pub fn configured_name(&self, indexer_id: Option<i64>) -> Option<String> {
        match self {
            Backend::Prowlarr(_) => None,
            Backend::Native(r) => indexer_id
                .and_then(|i| u32::try_from(i).ok())
                .and_then(|i| r.get(i))
                .map(|ix| ix.cfg.name.clone()),
        }
    }

    /// The provider label for errors about a release from `indexer_id`.
    pub fn label(&self, indexer_id: Option<i64>) -> String {
        match self {
            Backend::Prowlarr(_) => "prowlarr".into(),
            Backend::Native(r) => indexer_id
                .and_then(|i| u32::try_from(i).ok())
                .and_then(|i| r.get(i))
                .map_or_else(|| "native indexer".into(), |ix| ix.label()),
        }
    }

    /// Fetches a release's HTTP download link through the backend that produced it: the
    /// configured Prowlarr's download proxy, or the native indexer with the release's id.
    pub async fn fetch(
        &self,
        indexer_id: Option<i64>,
        url: &str,
    ) -> Result<Payload, UpstreamError> {
        match self {
            Backend::Prowlarr(p) => p.fetch_release(url).await,
            Backend::Native(r) => {
                let ix = indexer_id
                    .and_then(|i| u32::try_from(i).ok())
                    .and_then(|i| r.get(i))
                    .ok_or_else(|| UpstreamError::BadResponse {
                        reason: "the release's indexer is not a configured native indexer".into(),
                    })?;
                ix.fetch(url).await
            }
        }
    }

    pub fn native(&self) -> Option<&Arc<Registry>> {
        match self {
            Backend::Native(r) => Some(r),
            Backend::Prowlarr(_) => None,
        }
    }
}

/// Prowlarr fields every indexer has; harmless for a native provider (seed goals and app
/// sync values reach the settings through Radarr/Sonarr, limits are checked separately).
const BASE_FIELDS: &[&str] = &[
    "definitionFile",
    "baseUrl",
    "baseSettings.limitsUnit",
    "baseSettings.queryLimit",
    "baseSettings.grabLimit",
    "torrentBaseSettings.appMinimumSeeders",
    "torrentBaseSettings.seedRatio",
    "torrentBaseSettings.seedTime",
    "torrentBaseSettings.packSeedTime",
    "torrentBaseSettings.preferMagnetUrl",
];

/// Site settings of the Cardigann `thepiratebay` definition this provider checks:
/// - `apiurl`: the API host (must be `base_url`'s host);
/// - `uploader`: an uploader filter (empty = none; otherwise `uploaders` must say the same);
/// - `top100`: which list a query-less request reads; 6 ("All") is the recent feed, the only
///   one implemented (0-5 are per-category top-100 popularity lists).
const TPB_FIELDS: &[&str] = &["apiurl", "uploader", "top100"];
/// The `top100` option that means "All" (`precompiled/data_top100_recent.json`).
pub const TPB_TOP100_RECENT: i64 = 6;

fn is_default(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

fn as_limit(v: Option<&Value>) -> Option<u64> {
    v.and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    })
    .filter(|n| *n > 0)
}

fn as_int(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// The TPB site settings that cannot be honoured, with the reason.
fn tpb_site_problems(native: &Indexer, settings: &BTreeMap<String, Value>) -> Vec<String> {
    let mut out = vec![];
    if let Some(v) = settings.get("definitionFile")
        && !is_default(v)
        && v.as_str() != Some("thepiratebay")
    {
        out.push("definitionFile is not thepiratebay".to_string());
    }
    match settings.get("apiurl") {
        None => {}
        Some(v) if is_default(v) => {}
        Some(Value::String(host)) => {
            let want = native.cfg.base_url.host_str().unwrap_or_default();
            if !host.trim().eq_ignore_ascii_case(want) {
                out.push(format!(
                    "the imported API host differs from base_url's host {want:?}; set base_url to https://{}/ to keep it",
                    host.trim()
                ));
            }
        }
        Some(_) => out.push("apiurl is not a host name this build can compare".into()),
    }
    match settings.get("uploader") {
        None => {}
        Some(v) if is_default(v) => {}
        Some(Value::String(u)) if u != crate::import::WITHHELD => {
            let want: Vec<String> = u
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect();
            let mut have = native.cfg.uploaders.clone();
            have.sort();
            let mut want_sorted = want.clone();
            want_sorted.sort();
            if have != want_sorted {
                out.push(format!(
                    "the import filters results by uploader {want:?}; set uploaders = {want:?} to keep that filter"
                ));
            }
        }
        Some(_) => out.push("the uploader filter could not be copied".into()),
    }
    match settings.get("top100").map(|v| (v, as_int(v))) {
        None => {}
        Some((Value::Null, _)) => {}
        Some((_, Some(TPB_TOP100_RECENT))) => {}
        Some((_, n)) => out.push(format!(
            "top100 = {} selects a per-category top-100 popularity list; only option {TPB_TOP100_RECENT} (All, the recent feed) is implemented",
            n.map_or_else(|| "?".to_string(), |n| n.to_string())
        )),
    }
    out
}

/// Whether the native indexer implements what the import recorded for `ix`.
fn validate_source(native: &Indexer, ix: &IndexerSetting) -> Option<String> {
    let src = ix.source.as_ref()?;
    let def = src.definition.as_deref().map(str::to_ascii_lowercase);
    match (native.cfg.kind, def.as_deref()) {
        (_, None) => {}
        (IndexerKind::ThePirateBay, Some("thepiratebay")) => {}
        (IndexerKind::ThePirateBay, Some(d)) => {
            return Some(format!(
                "imported indexer {} is the Prowlarr definition '{d}', which the native thepiratebay provider does not implement",
                ix.prowlarr_indexer_id
            ));
        }
        (IndexerKind::Knaben, Some(d)) => {
            return Some(format!(
                "imported indexer {} is the Prowlarr definition '{d}', which the native knaben provider does not implement",
                ix.prowlarr_indexer_id
            ));
        }
        (IndexerKind::Torznab, Some(d)) if native.cfg.prowlarr_definition.as_deref() == Some(d) => {
        }
        (IndexerKind::Torznab, Some(d)) => {
            return Some(format!(
                "imported indexer {} is the Prowlarr definition '{d}'; a Torznab endpoint serves it only with prowlarr_definition = \"{d}\" (an explicit statement that the endpoint stands in for it)",
                ix.prowlarr_indexer_id
            ));
        }
    }
    if let Some(imp) = src.implementation.as_deref()
        && native.cfg.kind == IndexerKind::ThePirateBay
        && !imp.eq_ignore_ascii_case("cardigann")
    {
        return Some(format!(
            "imported indexer {} uses the Prowlarr implementation {imp}, not the Cardigann thepiratebay definition",
            ix.prowlarr_indexer_id
        ));
    }
    let site: &[&str] = match native.cfg.kind {
        IndexerKind::ThePirateBay => TPB_FIELDS,
        IndexerKind::Knaben | IndexerKind::Torznab => &[],
    };
    let unknown: Vec<String> = src
        .settings
        .iter()
        .filter(|(k, v)| {
            !BASE_FIELDS.contains(&k.as_str()) && !site.contains(&k.as_str()) && !is_default(v)
        })
        .map(|(k, _)| k.clone())
        .collect();
    if !unknown.is_empty() {
        return Some(format!(
            "imported Prowlarr settings {unknown:?} of indexer {} are not implemented natively (fail closed; checked site settings: {site:?})",
            ix.prowlarr_indexer_id
        ));
    }
    if native.cfg.kind == IndexerKind::ThePirateBay {
        let problems = tpb_site_problems(native, &src.settings);
        if !problems.is_empty() {
            return Some(format!(
                "imported indexer {}: {}",
                ix.prowlarr_indexer_id,
                problems.join("; ")
            ));
        }
    }
    let unit = match src.settings.get("baseSettings.limitsUnit") {
        Some(Value::Number(n)) if n.as_u64() == Some(1) => Duration::from_secs(3600),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("hour") => Duration::from_secs(3600),
        _ => Duration::from_secs(86_400),
    };
    for (key, configured, what) in [
        (
            "baseSettings.queryLimit",
            native.cfg.query_limit,
            "query_limit",
        ),
        (
            "baseSettings.grabLimit",
            native.cfg.grab_limit,
            "grab_limit",
        ),
    ] {
        let Some(imported) = as_limit(src.settings.get(key)) else {
            continue;
        };
        // Preserved when the native limit is at least as strict: no more per window, and a
        // window at least as long.
        let ok = configured
            .is_some_and(|c| u64::from(c) <= imported && native.cfg.limits_window >= unit);
        if !ok {
            return Some(format!(
                "the import limits indexer {} to {imported} per {} ({key}); set {what} = {imported} (or lower) with limits_unit = \"{}\" to preserve it",
                ix.prowlarr_indexer_id,
                window_name(unit),
                window_name(unit)
            ));
        }
    }
    None
}

/// Seed goals for releases from imported setting `ix`: Radarr/Sonarr's own seed criteria,
/// else, with native indexers, the Prowlarr indexer's own `torrentBaseSettings` that Prowlarr
/// would sync to the apps (ratio, time, season-pack time; negative = unset).
pub fn seed_goals(ix: &IndexerSetting, native: bool) -> (Option<f64>, Option<i64>, Option<i64>) {
    let from_source = |key: &str| {
        native
            .then(|| ix.source.as_ref()?.settings.get(key).cloned())
            .flatten()
    };
    let ratio = ix
        .seed_ratio
        .or_else(|| from_source("torrentBaseSettings.seedRatio").and_then(|v| v.as_f64()));
    let time = ix
        .seed_time_minutes
        .or_else(|| from_source("torrentBaseSettings.seedTime").and_then(|v| as_int(&v)));
    let pack = ix
        .season_pack_seed_time_minutes
        .or_else(|| from_source("torrentBaseSettings.packSeedTime").and_then(|v| as_int(&v)));
    (ratio, time, pack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{IndexerSource, MediaKind, NativeRef};

    fn cfg(kind: IndexerKind) -> IndexerConfig {
        IndexerConfig {
            id: 1,
            name: "The Pirate Bay".into(),
            kind,
            base_url: reqwest::Url::parse("https://apibay.example/").unwrap(),
            api_key: None,
            api_path: "api".into(),
            download_origins: vec![],
            key_origins: vec![],
            prowlarr_definition: None,
            magnet_trackers: vec![],
            uploaders: vec![],
            recent_feed: true,
            timeout: Duration::from_secs(5),
            max_concurrent: 1,
            min_interval: Duration::ZERO,
            cache_ttl: Duration::ZERO,
            max_response_bytes: 1 << 20,
            max_pages: 1,
            page_size: 100,
            query_limit: None,
            grab_limit: None,
            limits_window: Duration::from_secs(86_400),
        }
    }

    fn setting(source: Option<IndexerSource>) -> IndexerSetting {
        IndexerSetting {
            media: MediaKind::Movie,
            prowlarr_indexer_id: 1,
            name: "The Pirate Bay".into(),
            native: NativeRef {
                app: "radarr".into(),
                id: 3,
                name: "The Pirate Bay (Prowlarr)".into(),
            },
            protocol: "torrent".into(),
            categories: vec![2000],
            priority: 25,
            minimum_seeders: 1,
            enable_automatic_search: true,
            enable_interactive_search: true,
            enable_rss: true,
            seed_ratio: None,
            seed_time_minutes: None,
            season_pack_seed_time_minutes: None,
            unsupported: vec![],
            source,
        }
    }

    fn src(def: &str, settings: Value) -> Option<IndexerSource> {
        Some(IndexerSource {
            implementation: Some("Cardigann".into()),
            definition: Some(def.into()),
            settings: serde_json::from_value(settings).unwrap(),
        })
    }

    fn reg(c: IndexerConfig) -> Registry {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("s.sqlite")).unwrap();
        // The store keeps its own connection; the directory may go.
        Registry::new(&[c], store)
    }

    #[test]
    fn the_current_live_import_shape_maps_to_native_tpb() {
        // A typical Prowlarr TPB indexer definition, as the importer copies it.
        let mut live = cfg(IndexerKind::ThePirateBay);
        live.base_url = reqwest::Url::parse("https://apibay.org/").unwrap();
        let r = reg(live);
        let s = src(
            "thepiratebay",
            json!({
                "definitionFile": "thepiratebay", "baseUrl": "https://thepiratebay.org/", "apiurl": "apibay.org",
                "uploader": "", "top100": 6, "baseSettings.queryLimit": null, "baseSettings.grabLimit": null,
                "baseSettings.limitsUnit": 0, "torrentBaseSettings.seedRatio": 2, "torrentBaseSettings.seedTime": null,
                "torrentBaseSettings.packSeedTime": null, "torrentBaseSettings.preferMagnetUrl": false,
            }),
        );
        let ix = setting(s.clone());
        assert!(r.unusable(&ix).is_none(), "{:?}", r.unusable(&ix));
        // Ratio 2 comes from Prowlarr's own settings when the app-side criteria are unset,
        // with native indexers only (legacy Prowlarr mode keeps the app-side value).
        assert_eq!(seed_goals(&ix, true), (Some(2.0), None, None));
        assert_eq!(seed_goals(&ix, false), (None, None, None));
        let mut app_side = setting(s);
        app_side.seed_ratio = Some(3.0);
        assert_eq!(
            seed_goals(&app_side, true).0,
            Some(3.0),
            "the app-side value wins"
        );
        // Every other top100 option is a popularity list: refused.
        for (field, value, needle) in [
            ("top100", json!(1), "top100 = 1"),
            ("top100", json!("0"), "top100 = 0"),
            ("uploader", json!("someone"), "uploaders = [\"someone\"]"),
            (
                "apiurl",
                json!("tpb.mirror.example"),
                "set base_url to https://tpb.mirror.example/",
            ),
            (
                "uploader",
                json!(crate::import::WITHHELD),
                "could not be copied",
            ),
        ] {
            let ix = setting(src("thepiratebay", json!({ field: value })));
            let why = r.unusable(&ix).map(|x| x.1).unwrap_or_default();
            assert!(why.contains(needle), "{field}: {why}");
        }
        let mut filtered = cfg(IndexerKind::ThePirateBay);
        filtered.base_url = reqwest::Url::parse("https://apibay.org/").unwrap();
        filtered.uploaders = vec!["someone".into()];
        assert!(
            reg(filtered)
                .unusable(&setting(src(
                    "thepiratebay",
                    json!({ "uploader": "someone" })
                )))
                .is_none()
        );
    }

    #[test]
    fn imported_definitions_and_site_settings_fail_closed() {
        let r = reg(cfg(IndexerKind::ThePirateBay));
        assert!(r.unusable(&setting(None)).is_none(), "no source recorded");
        let ok = src(
            "thepiratebay",
            json!({ "definitionFile": "thepiratebay", "apiurl": 0, "baseUrl": "https://thepiratebay.org/", "torrentBaseSettings.preferMagnetUrl": false, "unused": null }),
        );
        assert!(r.unusable(&setting(ok)).is_none());
        let other = r.unusable(&setting(src("1337x", json!({})))).unwrap();
        assert_eq!(other.0, "unsupported");
        assert!(other.1.contains("1337x"), "{}", other.1);
        let filter = r
            .unusable(&setting(src(
                "thepiratebay",
                json!({ "uploader": "someone" }),
            )))
            .unwrap();
        assert!(filter.1.contains("uploader"), "{}", filter.1);
        let limit = r
            .unusable(&setting(src(
                "thepiratebay",
                json!({ "baseSettings.queryLimit": 50, "baseSettings.limitsUnit": 0 }),
            )))
            .unwrap();
        assert!(limit.1.contains("query_limit = 50"), "{}", limit.1);
        let mut strict = cfg(IndexerKind::ThePirateBay);
        strict.query_limit = Some(40);
        let r2 = reg(strict);
        assert!(
            r2.unusable(&setting(src(
                "thepiratebay",
                json!({ "baseSettings.queryLimit": 50 })
            )))
            .is_none(),
            "a stricter native limit preserves the imported one"
        );
        let mut missing = setting(None);
        missing.prowlarr_indexer_id = 7;
        assert_eq!(r.unusable(&missing).unwrap().0, "not_configured");
        // A Torznab endpoint stands in for a definition only when the operator says so.
        let t = reg(cfg(IndexerKind::Torznab));
        assert!(t.unusable(&setting(src("1337x", json!({})))).is_some());
        let mut said = cfg(IndexerKind::Torznab);
        said.prowlarr_definition = Some("1337x".into());
        assert!(
            reg(said)
                .unusable(&setting(src("1337x", json!({}))))
                .is_none()
        );
    }
}
