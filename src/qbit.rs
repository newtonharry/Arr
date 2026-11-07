//! qBittorrent WebUI API v2 client (4.3.9 = WebAPI 2.8.2 through 5.x = 2.11+).
//!
//! Cookie session from `auth/login` (`Ok.`/`Fails.`; 403 = banned), one re-login on 403, a
//! same-origin `Referer` for CSRF protection, bounded responses, no redirects. Version
//! differences: 2.11+ uses `stop`/`start` and the `stopped` add flag, earlier versions
//! `pause`/`resume` and `paused`; `contentLayout` exists from 2.7. Only parameters present in
//! 4.3.9 are sent (no `inactiveSeedingTimeLimit`, `shareLimitAction` or `info?tag=`).
//! Credentials and the session cookie never reach logs or responses.

use crate::config::QbitConfig;
use crate::prowlarr::Payload;
use crate::upstream::{self, UpstreamError};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const MAX_RESPONSE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ApiVersion(pub u32, pub u32, pub u32);

impl ApiVersion {
    fn parse(s: &str) -> Option<Self> {
        let mut it = s
            .trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.parse::<u32>().ok());
        Some(ApiVersion(
            it.next()??,
            it.next()??,
            it.next().flatten().unwrap_or(0),
        ))
    }

    /// qBittorrent 5.x naming (`stop`/`start`, `stopped`).
    pub fn stop_start(self) -> bool {
        self >= ApiVersion(2, 11, 0)
    }

    pub fn content_layout(self) -> bool {
        self >= ApiVersion(2, 7, 0)
    }
}

impl std::fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Normalised torrent state across 4.x and 5.x names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientState {
    Queued,
    Downloading,
    FetchingMetadata,
    Checking,
    Seeding,
    Paused,
    CompletedPaused,
    Stalled,
    Moving,
    Error,
    MissingFiles,
    Unknown,
}

impl ClientState {
    pub fn parse(s: &str) -> Self {
        match s {
            "queuedDL" => ClientState::Queued,
            "downloading" | "forcedDL" | "allocating" => ClientState::Downloading,
            "metaDL" | "forcedMetaDL" => ClientState::FetchingMetadata,
            "checkingDL" | "checkingUP" | "checkingResumeData" => ClientState::Checking,
            "uploading" | "stalledUP" | "forcedUP" | "queuedUP" => ClientState::Seeding,
            "pausedDL" | "stoppedDL" => ClientState::Paused,
            "pausedUP" | "stoppedUP" => ClientState::CompletedPaused,
            "stalledDL" => ClientState::Stalled,
            "moving" => ClientState::Moving,
            "error" => ClientState::Error,
            "missingFiles" => ClientState::MissingFiles,
            _ => ClientState::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ClientState::Queued => "queued",
            ClientState::Downloading => "downloading",
            ClientState::FetchingMetadata => "fetching_metadata",
            ClientState::Checking => "checking",
            ClientState::Seeding => "seeding",
            ClientState::Paused => "paused",
            ClientState::CompletedPaused => "completed_paused",
            ClientState::Stalled => "stalled",
            ClientState::Moving => "moving",
            ClientState::Error => "error",
            ClientState::MissingFiles => "missing_files",
            ClientState::Unknown => "unknown",
        }
    }

    /// Download finished and data verified (seeding or stopped after completion).
    pub fn complete(self) -> bool {
        matches!(self, ClientState::Seeding | ClientState::CompletedPaused)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TorrentInfo {
    pub hash: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub tags: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub amount_left: i64,
    #[serde(default)]
    pub save_path: String,
    #[serde(default)]
    pub dlspeed: i64,
    #[serde(default)]
    pub eta: i64,
    #[serde(default)]
    pub content_path: Option<String>,
    /// Share ratio (`-1` above qBittorrent's maximum).
    #[serde(default)]
    pub ratio: Option<f64>,
    /// Per-torrent ratio limit: `>= 0` limit, `-2` use the global limit, `-1` unlimited.
    #[serde(default)]
    pub ratio_limit: Option<f64>,
    /// Effective ratio limit (`-1` = none).
    #[serde(default)]
    pub max_ratio: Option<f64>,
    /// Seconds spent seeding after completion (WebAPI 2.8.1+).
    #[serde(default)]
    pub seeding_time: Option<i64>,
    /// Per-torrent seeding time limit in minutes: `>= 0`, `-2` global, `-1` unlimited.
    #[serde(default)]
    pub seeding_time_limit: Option<i64>,
    /// Effective seeding time limit in minutes (`-1` = none).
    #[serde(default)]
    pub max_seeding_time: Option<i64>,
    #[serde(default)]
    pub downloaded: Option<i64>,
    #[serde(default)]
    pub uploaded: Option<i64>,
}

/// Global share limits (`app/preferences`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Preferences {
    #[serde(default)]
    pub max_ratio_enabled: Option<bool>,
    #[serde(default)]
    pub max_ratio: Option<f64>,
    #[serde(default)]
    pub max_seeding_time_enabled: Option<bool>,
    /// Minutes.
    #[serde(default)]
    pub max_seeding_time: Option<i64>,
}

/// `torrents/properties` (share ratio and seeding time when `info` lacks them).
#[derive(Debug, Clone, Deserialize)]
pub struct Properties {
    #[serde(default)]
    pub share_ratio: Option<f64>,
    /// Seconds.
    #[serde(default)]
    pub seeding_time: Option<i64>,
    #[serde(default)]
    pub total_downloaded: Option<i64>,
    #[serde(default)]
    pub total_uploaded: Option<i64>,
    #[serde(default)]
    pub save_path: Option<String>,
}

impl TorrentInfo {
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.split(',').any(|t| t.trim() == tag)
    }

    pub fn state(&self) -> ClientState {
        ClientState::parse(&self.state)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TorrentFile {
    #[serde(default)]
    pub index: Option<i64>,
    pub name: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub priority: i64,
}

/// Parameters for `torrents/add`, from the imported client and indexer settings.
pub struct AddRequest {
    pub payload: Payload,
    pub save_path: String,
    pub category: String,
    pub tags: String,
    pub paused: bool,
    pub ratio_limit: Option<f64>,
    pub seeding_time_limit_minutes: Option<i64>,
    pub sequential: bool,
    pub first_last_piece: bool,
    /// `Original` or `Subfolder`; `None` = the client's default.
    pub content_layout: Option<&'static str>,
}

struct Session {
    cookie: Option<String>,
    api: ApiVersion,
}

pub struct Qbit {
    cfg: QbitConfig,
    http: reqwest::Client,
    session: Mutex<Option<Session>>,
}

fn bad(reason: &str) -> UpstreamError {
    UpstreamError::BadResponse {
        reason: reason.into(),
    }
}

impl Qbit {
    pub fn new(cfg: QbitConfig) -> Self {
        Qbit {
            http: upstream::http_client(cfg.timeout, reqwest::redirect::Policy::none()),
            session: Mutex::new(None),
            cfg,
        }
    }

    pub fn save_path(&self) -> &str {
        &self.cfg.save_path
    }

    fn url(&self, path: &str) -> Result<Url, UpstreamError> {
        self.cfg
            .base_url
            .join(path)
            .map_err(|_| bad("invalid path"))
    }

    fn referer(&self) -> String {
        self.cfg.base_url.origin().ascii_serialization()
    }

    async fn text(resp: reqwest::Response) -> Result<String, UpstreamError> {
        let bytes = upstream::read_bytes(resp, 64 * 1024).await?;
        String::from_utf8(bytes).map_err(|_| bad("response is not UTF-8"))
    }

    async fn login(&self) -> Result<Session, UpstreamError> {
        let mut cookie = None;
        if let (Some(u), Some(p)) = (&self.cfg.username, &self.cfg.password) {
            let resp = upstream::send(
                self.http
                    .post(self.url("api/v2/auth/login")?)
                    .header("Referer", self.referer())
                    .form(&[("username", u.expose()), ("password", p.expose())]),
            )
            .await?;
            if resp.status().as_u16() == 403 {
                return Err(UpstreamError::Auth { status: 403 });
            }
            cookie = resp
                .headers()
                .get_all(reqwest::header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .filter_map(|v| v.split(';').next())
                .map(str::trim)
                .find(|kv| {
                    kv.split_once('=').is_some_and(|(k, v)| {
                        !k.is_empty()
                            && k.len() <= 128
                            && k.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                            && !v.is_empty()
                            && v.bytes().all(|b| b.is_ascii_graphic() && b != b';')
                    })
                })
                .map(str::to_owned);
            let body = Self::text(resp).await?;
            match body.trim() {
                "Ok." => {}
                "Fails." => return Err(UpstreamError::Auth { status: 200 }),
                _ => return Err(bad("unexpected login response")),
            }
        }
        let mut req = self
            .http
            .get(self.url("api/v2/app/webapiVersion")?)
            .header("Referer", self.referer());
        if let Some(c) = &cookie {
            req = req.header(reqwest::header::COOKIE, c);
        }
        let resp = upstream::send(req).await?;
        let version = Self::text(resp).await?;
        let api = ApiVersion::parse(&version).ok_or_else(|| bad("unparseable WebAPI version"))?;
        Ok(Session { cookie, api })
    }

    /// Sends a request with the session cookie; on 403 logs in again once and retries.
    async fn call(
        &self,
        build: impl Fn(
            &Self,
            &Option<String>,
            ApiVersion,
        ) -> Result<reqwest::RequestBuilder, UpstreamError>,
    ) -> Result<(reqwest::Response, ApiVersion), UpstreamError> {
        let mut guard = self.session.lock().await;
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(self.login().await?);
            }
            let s = guard.as_ref().expect("session");
            let req = build(self, &s.cookie, s.api)?.header("Referer", self.referer());
            let resp = upstream::send(req).await?;
            if resp.status().as_u16() == 403 && attempt == 0 {
                *guard = None; // expired or revoked session: re-authenticate once
                continue;
            }
            if resp.status().as_u16() == 403 {
                return Err(UpstreamError::Auth { status: 403 });
            }
            return Ok((resp, s.api));
        }
        Err(UpstreamError::Auth { status: 403 })
    }

    fn with_cookie(
        req: reqwest::RequestBuilder,
        cookie: &Option<String>,
    ) -> reqwest::RequestBuilder {
        match cookie {
            Some(c) => req.header(reqwest::header::COOKIE, c),
            None => req,
        }
    }

    pub async fn version(&self) -> Result<ApiVersion, UpstreamError> {
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            *guard = Some(self.login().await?);
        }
        Ok(guard.as_ref().expect("session").api)
    }

    /// Torrents with the given hashes (absent hashes are simply missing from the result).
    pub async fn info(&self, hashes: &[String]) -> Result<Vec<TorrentInfo>, UpstreamError> {
        if hashes.is_empty() {
            return Ok(vec![]);
        }
        let joined = hashes.join("|");
        let (resp, _) = self
            .call(|q, c, _| {
                let mut url = q.url("api/v2/torrents/info")?;
                url.query_pairs_mut().append_pair("hashes", &joined);
                Ok(Self::with_cookie(q.http.get(url), c))
            })
            .await?;
        if let Some(e) = upstream::classify_status(resp.status()) {
            return Err(e);
        }
        let list: Vec<TorrentInfo> = upstream::read_json(resp, MAX_RESPONSE).await?;
        if list
            .iter()
            .any(|t| !crate::torrent::is_info_hash(&t.hash.to_ascii_lowercase()))
        {
            return Err(bad("torrent list contains an invalid hash"));
        }
        Ok(list)
    }

    /// Files of a torrent; `Ok(None)` when the client does not know the hash.
    pub async fn files(&self, hash: &str) -> Result<Option<Vec<TorrentFile>>, UpstreamError> {
        let (resp, _) = self
            .call(|q, c, _| {
                let mut url = q.url("api/v2/torrents/files")?;
                url.query_pairs_mut().append_pair("hash", hash);
                Ok(Self::with_cookie(q.http.get(url), c))
            })
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if let Some(e) = upstream::classify_status(resp.status()) {
            return Err(e);
        }
        Ok(Some(upstream::read_json(resp, MAX_RESPONSE).await?))
    }

    /// Submits a torrent. `Ok(true)` only when the client answered exactly `Ok.`; callers must
    /// still verify by hash (HTTP 200 alone proves nothing).
    pub async fn add(&self, req: &AddRequest) -> Result<bool, UpstreamError> {
        let (resp, _) = self
            .call(|q, c, api| {
                let mut form = reqwest::multipart::Form::new()
                    .text("savepath", req.save_path.clone())
                    .text("category", req.category.clone())
                    .text("tags", req.tags.clone())
                    .text("autoTMM", "false")
                    .text(
                        if api.stop_start() {
                            "stopped"
                        } else {
                            "paused"
                        },
                        req.paused.to_string(),
                    )
                    .text("sequentialDownload", req.sequential.to_string())
                    .text("firstLastPiecePrio", req.first_last_piece.to_string());
                if let Some(layout) = req.content_layout {
                    if !api.content_layout() {
                        return Err(bad(
                            "the imported content layout needs qBittorrent WebAPI 2.7 or later",
                        ));
                    }
                    form = form.text("contentLayout", layout);
                }
                if let Some(r) = req.ratio_limit {
                    form = form.text("ratioLimit", r.to_string());
                }
                if let Some(t) = req.seeding_time_limit_minutes {
                    form = form.text("seedingTimeLimit", t.to_string());
                }
                form = match &req.payload {
                    Payload::Magnet(m) => form.text("urls", m.clone()),
                    Payload::Torrent(bytes) => form.part(
                        "torrents",
                        reqwest::multipart::Part::bytes(bytes.clone())
                            .file_name("release.torrent")
                            .mime_str("application/x-bittorrent")
                            .map_err(|_| bad("invalid mime"))?,
                    ),
                };
                Ok(Self::with_cookie(
                    q.http.post(q.url("api/v2/torrents/add")?).multipart(form),
                    c,
                ))
            })
            .await?;
        if resp.status().as_u16() == 415 {
            return Err(bad("qBittorrent rejected the torrent file as invalid"));
        }
        let body = Self::text(resp).await?;
        Ok(body.trim() == "Ok.")
    }

    async fn simple_post(
        &self,
        v4: &str,
        v5: &str,
        fields: &[(&str, String)],
    ) -> Result<(), UpstreamError> {
        let (resp, _) = self
            .call(|q, c, api| {
                let path = if api.stop_start() { v5 } else { v4 };
                Ok(Self::with_cookie(q.http.post(q.url(path)?).form(fields), c))
            })
            .await?;
        if let Some(e) = upstream::classify_status(resp.status()) {
            return Err(e);
        }
        Ok(())
    }

    pub async fn pause(&self, hash: &str) -> Result<(), UpstreamError> {
        self.simple_post(
            "api/v2/torrents/pause",
            "api/v2/torrents/stop",
            &[("hashes", hash.to_string())],
        )
        .await
    }

    pub async fn resume(&self, hash: &str) -> Result<(), UpstreamError> {
        self.simple_post(
            "api/v2/torrents/resume",
            "api/v2/torrents/start",
            &[("hashes", hash.to_string())],
        )
        .await
    }

    /// Removes a torrent entry. `delete_files` also removes its downloaded data; callers use it
    /// only for owned, confined, journaled downloads after a verified import (or a failed
    /// download) when the imported client policy requests removal.
    pub async fn delete(&self, hash: &str, delete_files: bool) -> Result<(), UpstreamError> {
        if !crate::torrent::is_info_hash(hash) {
            return Err(bad("refusing to delete an invalid hash"));
        }
        self.simple_post(
            "api/v2/torrents/delete",
            "api/v2/torrents/delete",
            &[
                ("hashes", hash.to_string()),
                ("deleteFiles", delete_files.to_string()),
            ],
        )
        .await
    }

    pub async fn set_force_start(&self, hash: &str) -> Result<(), UpstreamError> {
        self.simple_post(
            "api/v2/torrents/setForceStart",
            "api/v2/torrents/setForceStart",
            &[("hashes", hash.to_string()), ("value", "true".into())],
        )
        .await
    }

    pub async fn set_category(&self, hash: &str, category: &str) -> Result<(), UpstreamError> {
        self.simple_post(
            "api/v2/torrents/setCategory",
            "api/v2/torrents/setCategory",
            &[
                ("hashes", hash.to_string()),
                ("category", category.to_string()),
            ],
        )
        .await
    }

    pub async fn preferences(&self) -> Result<Preferences, UpstreamError> {
        let (resp, _) = self
            .call(|q, c, _| {
                Ok(Self::with_cookie(
                    q.http.get(q.url("api/v2/app/preferences")?),
                    c,
                ))
            })
            .await?;
        if let Some(e) = upstream::classify_status(resp.status()) {
            return Err(e);
        }
        upstream::read_json(resp, MAX_RESPONSE).await
    }

    /// `Ok(None)` when the client does not know the hash.
    pub async fn properties(&self, hash: &str) -> Result<Option<Properties>, UpstreamError> {
        let (resp, _) = self
            .call(|q, c, _| {
                let mut url = q.url("api/v2/torrents/properties")?;
                url.query_pairs_mut().append_pair("hash", hash);
                Ok(Self::with_cookie(q.http.get(url), c))
            })
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if let Some(e) = upstream::classify_status(resp.status()) {
            return Err(e);
        }
        Ok(Some(upstream::read_json(resp, MAX_RESPONSE).await?))
    }

    pub async fn categories(
        &self,
    ) -> Result<serde_json::Map<String, serde_json::Value>, UpstreamError> {
        let (resp, _) = self
            .call(|q, c, _| {
                Ok(Self::with_cookie(
                    q.http.get(q.url("api/v2/torrents/categories")?),
                    c,
                ))
            })
            .await?;
        upstream::read_json(resp, MAX_RESPONSE).await
    }
}
