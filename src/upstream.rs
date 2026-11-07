//! Shared upstream HTTP handling: bounded bodies, classified failures, and error messages
//! that never contain URLs, credentials or response bodies.

use crate::error::{ApiError, ErrorCode};
use axum::http::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpstreamError {
    Timeout,
    Auth {
        status: u16,
    },
    RateLimited,
    Unavailable {
        reason: String,
    },
    BadResponse {
        reason: String,
    },
    NotFound,
    /// Refused locally before any request: a backoff the provider asked for (HTTP 429 with
    /// `Retry-After`), or a configured query limit for the current window.
    Throttled {
        reason: String,
    },
}

impl UpstreamError {
    pub fn describe(&self) -> String {
        match self {
            UpstreamError::Timeout => "timed out".into(),
            UpstreamError::Auth { status } => {
                format!("rejected the configured credential (HTTP {status})")
            }
            UpstreamError::RateLimited => "rate limited the request (HTTP 429)".into(),
            UpstreamError::Unavailable { reason } => format!("unavailable: {reason}"),
            UpstreamError::BadResponse { reason } => {
                format!("returned an unusable response: {reason}")
            }
            UpstreamError::NotFound => "returned not found".into(),
            UpstreamError::Throttled { reason } => format!("is throttled: {reason}"),
        }
    }

    pub fn code(&self) -> ErrorCode {
        match self {
            UpstreamError::Timeout => ErrorCode::UpstreamTimeout,
            UpstreamError::Auth { .. } => ErrorCode::UpstreamAuth,
            UpstreamError::RateLimited | UpstreamError::Throttled { .. } => {
                ErrorCode::UpstreamRateLimited
            }
            UpstreamError::Unavailable { .. } => ErrorCode::UpstreamUnavailable,
            UpstreamError::BadResponse { .. } | UpstreamError::NotFound => {
                ErrorCode::UpstreamBadResponse
            }
        }
    }

    /// Whether a later attempt may succeed without any change (callers defer, not reject).
    pub fn transient(&self) -> bool {
        matches!(
            self,
            UpstreamError::Timeout
                | UpstreamError::RateLimited
                | UpstreamError::Throttled { .. }
                | UpstreamError::Unavailable { .. }
        )
    }

    pub fn into_api(self, provider: &str) -> ApiError {
        let status = match self {
            UpstreamError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            UpstreamError::RateLimited | UpstreamError::Throttled { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::BAD_GATEWAY,
        };
        ApiError::new(
            status,
            self.code(),
            format!("{provider} {}", self.describe()),
        )
        .with_details(serde_json::json!({ "provider": provider, "failure": self }))
    }
}

fn classify_send(e: &reqwest::Error) -> UpstreamError {
    if e.is_timeout() {
        UpstreamError::Timeout
    } else if e.is_connect() {
        UpstreamError::Unavailable {
            reason: "connection failed".into(),
        }
    } else if e.is_redirect() {
        UpstreamError::BadResponse {
            reason: "redirect not permitted".into(),
        }
    } else {
        UpstreamError::Unavailable {
            reason: "request failed".into(),
        }
    }
}

pub fn classify_status(status: reqwest::StatusCode) -> Option<UpstreamError> {
    let code = status.as_u16();
    match code {
        200..=299 => None,
        401 | 403 => Some(UpstreamError::Auth { status: code }),
        404 => Some(UpstreamError::NotFound),
        429 => Some(UpstreamError::RateLimited),
        300..=399 => Some(UpstreamError::BadResponse {
            reason: format!("unexpected redirect (HTTP {code})"),
        }),
        500..=599 => Some(UpstreamError::Unavailable {
            reason: format!("HTTP {code}"),
        }),
        _ => Some(UpstreamError::BadResponse {
            reason: format!("HTTP {code}"),
        }),
    }
}

/// `Retry-After` as a delay: delta-seconds or an HTTP-date (RFC 9110 section 10.2.3).
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = time::OffsetDateTime::parse(v, &time::format_description::well_known::Rfc2822).ok()?;
    let now = time::OffsetDateTime::now_utc();
    Some(Duration::try_from(at - now).unwrap_or(Duration::ZERO))
}

/// Sends a request and returns the raw response (status not yet checked).
pub async fn send(req: reqwest::RequestBuilder) -> Result<reqwest::Response, UpstreamError> {
    req.send()
        .await
        .map_err(|e| classify_send(&e.without_url()))
}

/// Reads a successful response body up to `max` bytes and decodes JSON.
pub async fn read_json<T: DeserializeOwned>(
    mut resp: reqwest::Response,
    max: usize,
) -> Result<T, UpstreamError> {
    if let Some(err) = classify_status(resp.status()) {
        return Err(err);
    }
    let too_large = || UpstreamError::BadResponse {
        reason: format!("response larger than {max} bytes"),
    };
    if resp.content_length().is_some_and(|l| l > max as u64) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| classify_send(&e.without_url()))?
    {
        if body.len() + chunk.len() > max {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    // serde messages can quote response fragments; report the position only.
    serde_json::from_slice(&body).map_err(|e| UpstreamError::BadResponse {
        reason: format!(
            "invalid JSON ({:?} error at line {} column {})",
            e.classify(),
            e.line(),
            e.column()
        ),
    })
}

/// Reads a successful response body up to `max` bytes.
pub async fn read_bytes(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>, UpstreamError> {
    if let Some(err) = classify_status(resp.status()) {
        return Err(err);
    }
    let too_large = || UpstreamError::BadResponse {
        reason: format!("response larger than {max} bytes"),
    };
    if resp.content_length().is_some_and(|l| l > max as u64) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| classify_send(&e.without_url()))?
    {
        if body.len() + chunk.len() > max {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub fn http_client(timeout: Duration, redirect: reqwest::redirect::Policy) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .redirect(redirect)
        .user_agent(concat!("arr/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("static reqwest configuration is valid")
}

/// Small TTL cache with a size bound (oldest entry evicted first).
pub struct TtlCache<K, V> {
    ttl: Duration,
    max: usize,
    entries: Mutex<HashMap<K, (Instant, V)>>,
}

impl<K: Eq + Hash + Clone, V: Clone> TtlCache<K, V> {
    pub fn new(ttl: Duration, max: usize) -> Self {
        TtlCache {
            ttl,
            max,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, k: &K) -> Option<V> {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(k) {
            Some((at, v)) if at.elapsed() < self.ttl => Some(v.clone()),
            Some(_) => {
                map.remove(k);
                None
            }
            None => None,
        }
    }

    pub fn insert(&self, k: K, v: V) {
        if self.ttl.is_zero() || self.max == 0 {
            return;
        }
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, (at, _)| at.elapsed() < self.ttl);
        if map.len() >= self.max
            && !map.contains_key(&k)
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        map.insert(k, (Instant::now(), v));
    }
}
