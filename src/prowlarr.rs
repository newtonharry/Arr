//! Prowlarr client: `GET /api/v1/search` and, for execution, the release's own download link.
//!
//! The API key travels in the `X-Api-Key` header, never in the URL we build. Download/magnet
//! links are kept in `PrivateLink` (redacted `Debug`, no `Serialize`) and are only persisted in
//! private state; they never appear in responses or logs. `.torrent` fetches are restricted to
//! the configured Prowlarr origin's download proxy paths, size-bounded, and follow no redirect
//! except to a `magnet:` URI (or one same-origin hop).

use crate::config::ProwlarrConfig;
use crate::upstream::{self, TtlCache, UpstreamError};
use serde::{Deserialize, Deserializer};
use std::collections::HashSet;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::Semaphore;

/// A download or magnet link from a trusted Prowlarr result. Never printed or serialised.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateLink(String);

impl PrivateLink {
    pub fn new(s: String) -> Self {
        PrivateLink(s)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PrivateLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateLink(<redacted>)")
    }
}

fn link<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PrivateLink>, D::Error> {
    Ok(Option::<String>::deserialize(d)?
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 16 * 1024)
        .map(PrivateLink))
}

/// What a release resolves to before submission.
pub enum Payload {
    Magnet(String),
    Torrent(Vec<u8>),
}

const MAX_TORRENT_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct Category {
    #[serde(default)]
    pub id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProwlarrRelease {
    #[serde(default)]
    pub guid: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub indexer_id: Option<i64>,
    #[serde(default)]
    pub indexer: Option<String>,
    #[serde(default)]
    pub seeders: Option<i64>,
    #[serde(default)]
    pub leechers: Option<i64>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub publish_date: Option<String>,
    #[serde(default)]
    pub info_hash: Option<String>,
    #[serde(default, deserialize_with = "link")]
    pub download_url: Option<PrivateLink>,
    #[serde(default, deserialize_with = "link")]
    pub magnet_url: Option<PrivateLink>,
    #[serde(default)]
    pub categories: Option<Vec<Category>>,
    /// Tracker seed requirements (Torznab `minimumratio`, `minimumseedtime` in seconds).
    #[serde(default)]
    pub minimum_ratio: Option<f64>,
    #[serde(default)]
    pub minimum_seed_time: Option<i64>,
    /// Tracker accounting factors (Torznab `downloadvolumefactor`/`uploadvolumefactor`):
    /// reported with the release; they add no obligation beyond the seed requirements.
    #[serde(default)]
    pub download_volume_factor: Option<f64>,
    #[serde(default)]
    pub upload_volume_factor: Option<f64>,
}

impl ProwlarrRelease {
    /// Identity used for de-duplication across pages and queries.
    pub fn dedupe_key(&self) -> String {
        if let Some(h) = self
            .info_hash
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
        {
            return format!("hash:{}", h.to_ascii_lowercase());
        }
        if let Some(g) = self.guid.as_deref().filter(|g| !g.is_empty()) {
            return format!("guid:{}:{g}", self.indexer_id.unwrap_or(0));
        }
        format!(
            "title:{}:{}:{}",
            self.indexer_id.unwrap_or(0),
            self.title.as_deref().unwrap_or(""),
            self.size.unwrap_or(0)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchRequest {
    pub query: String,
    pub categories: Vec<u32>,
    pub indexer_ids: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub releases: Arc<Vec<ProwlarrRelease>>,
    pub pages_fetched: u32,
    pub raw_count: usize,
    /// Set when a later page failed after earlier pages succeeded.
    pub partial_error: Option<UpstreamError>,
    pub truncated_by_page_limit: bool,
    pub fetched_at: OffsetDateTime,
    pub from_cache: bool,
    /// Results the backend dropped (unrelated category, uploader filter, unusable fields).
    pub dropped: usize,
    /// The provider explicitly answered "no results" (a native indexer's own sentinel or a
    /// valid empty feed). Prowlarr's search API cannot say this.
    pub confirmed_empty: bool,
}

pub struct ProwlarrClient {
    cfg: ProwlarrConfig,
    http: reqwest::Client,
    permits: Semaphore,
    cache: TtlCache<SearchRequest, SearchOutcome>,
}

impl ProwlarrClient {
    pub fn new(cfg: ProwlarrConfig) -> Self {
        ProwlarrClient {
            http: upstream::http_client(cfg.timeout, reqwest::redirect::Policy::none()),
            permits: Semaphore::new(cfg.max_concurrent),
            cache: TtlCache::new(cfg.cache_ttl, 256),
            cfg,
        }
    }

    /// Resolves a Prowlarr download proxy link (from `downloadUrl`, or an HTTP `magnetUrl`) to
    /// a torrent file or magnet. Only the configured origin's `/{id}/download` or
    /// `/api/v1/indexer/{id}/download` paths are fetched, without URL credentials.
    pub async fn fetch_release(&self, url: &str) -> Result<Payload, UpstreamError> {
        let bad = |reason: &str| UpstreamError::BadResponse {
            reason: reason.into(),
        };
        let mut target = reqwest::Url::parse(url).map_err(|_| bad("download link is not a URL"))?;
        for hop in 0..2 {
            if target.origin() != self.cfg.base_url.origin() {
                return Err(bad(
                    "download link is not on the configured Prowlarr origin",
                ));
            }
            let rest = target
                .path()
                .strip_prefix(self.cfg.base_url.path())
                .unwrap_or("");
            let id_path = rest.strip_prefix("api/v1/indexer/").unwrap_or(rest);
            let ok_path = id_path
                .strip_suffix("/download")
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
            if !ok_path || target.fragment().is_some() {
                return Err(bad("download link is not a Prowlarr download proxy path"));
            }
            if !target.username().is_empty() || target.password().is_some() {
                return Err(bad("download link carries credentials"));
            }
            let resp = {
                let _permit =
                    self.permits
                        .acquire()
                        .await
                        .map_err(|_| UpstreamError::Unavailable {
                            reason: "client shutting down".into(),
                        })?;
                upstream::send(
                    self.http
                        .get(target.clone())
                        .header("X-Api-Key", self.cfg.api_key.expose())
                        .header("Accept", "application/x-bittorrent"),
                )
                .await?
            };
            if resp.status().is_redirection() {
                let location = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| bad("redirect without a location"))?
                    .to_string();
                if location.starts_with("magnet:?") {
                    crate::torrent::magnet_hash(&location)
                        .map_err(|_| bad("redirect to an invalid magnet"))?;
                    return Ok(Payload::Magnet(location));
                }
                if hop == 0 {
                    target = target
                        .join(&location)
                        .map_err(|_| bad("invalid redirect"))?;
                    continue;
                }
                return Err(bad("too many redirects"));
            }
            let bytes = upstream::read_bytes(resp, MAX_TORRENT_BYTES).await?;
            crate::torrent::info_hash(&bytes)
                .map_err(|_| bad("download is not a valid .torrent"))?;
            return Ok(Payload::Torrent(bytes));
        }
        Err(bad("too many redirects"))
    }

    pub fn base_url(&self) -> &reqwest::Url {
        &self.cfg.base_url
    }

    /// One indexer's Torznab feed (`GET {base}{id}/api?t=search&cat=...&limit=...`, no query =
    /// latest releases), body bounded by `max_response_bytes`. The API key goes in the
    /// `X-Api-Key` header and, because Prowlarr's Torznab endpoint authenticates the `apikey`
    /// parameter, in the query of a URL that is never logged or returned.
    pub async fn feed(
        &self,
        indexer_id: u32,
        categories: &[u32],
        limit: u32,
    ) -> Result<Vec<u8>, UpstreamError> {
        let mut url = self
            .cfg
            .base_url
            .join(&format!("{indexer_id}/api"))
            .map_err(|_| UpstreamError::BadResponse {
                reason: "invalid base URL".into(),
            })?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("t", "search");
            if !categories.is_empty() {
                q.append_pair(
                    "cat",
                    &categories
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            q.append_pair("limit", &limit.to_string());
            q.append_pair("apikey", self.cfg.api_key.expose());
        }
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| UpstreamError::Unavailable {
                reason: "client shutting down".into(),
            })?;
        let resp = upstream::send(
            self.http
                .get(url)
                .header("X-Api-Key", self.cfg.api_key.expose())
                .header("Accept", "application/rss+xml, application/xml"),
        )
        .await?;
        upstream::read_bytes(resp, self.cfg.max_response_bytes).await
    }

    async fn page(
        &self,
        req: &SearchRequest,
        offset: u32,
    ) -> Result<Vec<ProwlarrRelease>, UpstreamError> {
        let mut url =
            self.cfg
                .base_url
                .join("api/v1/search")
                .map_err(|_| UpstreamError::BadResponse {
                    reason: "invalid base URL".into(),
                })?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("query", &req.query)
                .append_pair("type", "search");
            for id in &req.indexer_ids {
                q.append_pair("indexerIds", &id.to_string());
            }
            for c in &req.categories {
                q.append_pair("categories", &c.to_string());
            }
            q.append_pair("limit", &self.cfg.page_size.to_string())
                .append_pair("offset", &offset.to_string());
        }
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| UpstreamError::Unavailable {
                reason: "client shutting down".into(),
            })?;
        let resp = upstream::send(
            self.http
                .get(url)
                .header("X-Api-Key", self.cfg.api_key.expose())
                .header("Accept", "application/json"),
        )
        .await?;
        upstream::read_json(resp, self.cfg.max_response_bytes).await
    }

    /// Runs one query with bounded pagination. Errors only if the first page fails.
    pub async fn search(
        &self,
        req: &SearchRequest,
        refresh: bool,
    ) -> Result<SearchOutcome, UpstreamError> {
        if !refresh && let Some(mut hit) = self.cache.get(req) {
            hit.from_cache = true;
            return Ok(hit);
        }
        let mut seen = HashSet::new();
        let mut releases = Vec::new();
        let mut raw_count = 0;
        let mut partial_error = None;
        let mut pages_fetched = 0;
        let mut truncated_by_page_limit = false;
        for page in 0..self.cfg.max_pages {
            let batch = match self.page(req, page * self.cfg.page_size).await {
                Ok(b) => b,
                Err(e) if page == 0 => return Err(e),
                Err(e) => {
                    partial_error = Some(e);
                    break;
                }
            };
            pages_fetched += 1;
            raw_count += batch.len();
            let full_page = batch.len() >= self.cfg.page_size as usize;
            let mut new = 0;
            for r in batch {
                if seen.insert(r.dedupe_key()) {
                    releases.push(r);
                    new += 1;
                }
            }
            // Indexers without pagination repeat the same page; stop when nothing new arrives.
            if !full_page || new == 0 {
                break;
            }
            if page + 1 == self.cfg.max_pages {
                truncated_by_page_limit = true;
            }
        }
        let outcome = SearchOutcome {
            releases: Arc::new(releases),
            pages_fetched,
            raw_count,
            partial_error,
            truncated_by_page_limit,
            fetched_at: OffsetDateTime::now_utc(),
            from_cache: false,
            dropped: 0,
            confirmed_empty: false,
        };
        if outcome.partial_error.is_none() {
            self.cache.insert(req.clone(), outcome.clone());
        }
        Ok(outcome)
    }
}
