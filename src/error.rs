//! Typed API errors. Every error body is `{"error": {"code", "message", "details"?}}`.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    NotFound,
    MethodNotAllowed,
    InvalidRequest,
    InvalidJson,
    InvalidBody,
    InvalidQuery,
    InvalidMediaId,
    UnsupportedMediaType,
    PayloadTooLarge,
    IdempotencyKeyReused,
    PlanNotActive,
    Overloaded,
    RequestTimeout,
    CatalogNotImported,
    MetadataUnavailable,
    ProfileRequired,
    ProviderNotConfigured,
    NoUsableIndexers,
    UpstreamTimeout,
    UpstreamAuth,
    UpstreamRateLimited,
    UpstreamUnavailable,
    UpstreamBadResponse,
    ExecutionDisabled,
    ClientNotConfigured,
    ImportUnsupported,
    AlreadyQueued,
    Blocklisted,
    DuplicateTorrent,
    InvalidPayload,
    NoDownloadLink,
    ReleaseNotFound,
    GrabState,
    SeedCriteriaUnmet,
    CleanupBlocked,
    ForeignTorrent,
    NotManaged,
    ActiveDownloads,
    WrongPayload,
    /// Coexistence mode: the target is acquired natively until it is adopted here.
    NotAdopted,
    /// Coexistence mode: adopting a native title needs `native_handoff_confirmed`.
    NativeHandoffRequired,
    /// Coexistence mode: the qBittorrent category is also a native client category.
    CategoryShared,
    /// Coexistence mode: an imported title this server cannot acquire stays native.
    UnsupportedScope,
    /// A native indexer's configured grab limit is reached for its window.
    IndexerLimitReached,
    /// The embedded engine could find no peer for the payload (no tracker it may use, DHT
    /// off or the torrent private, no initial peers); nothing was queued.
    NoPeerDiscovery,
    /// The copy's quality is no longer allowed by the title's quality level (the list of
    /// copies is older than a change of level); nothing was queued.
    QualityChanged,
    /// A managed setup document failed validation.
    InvalidSetup,
    /// Managed setup would replace settings it must not replace (an imported native
    /// configuration, or ids that catalog items or downloads still use).
    SetupConflict,
    Internal,
}

/// All codes, for the OpenAPI drift test.
pub const ALL_ERROR_CODES: &[ErrorCode] = &[
    ErrorCode::Unauthorized,
    ErrorCode::NotFound,
    ErrorCode::MethodNotAllowed,
    ErrorCode::InvalidRequest,
    ErrorCode::InvalidJson,
    ErrorCode::InvalidBody,
    ErrorCode::InvalidQuery,
    ErrorCode::InvalidMediaId,
    ErrorCode::UnsupportedMediaType,
    ErrorCode::PayloadTooLarge,
    ErrorCode::IdempotencyKeyReused,
    ErrorCode::PlanNotActive,
    ErrorCode::Overloaded,
    ErrorCode::RequestTimeout,
    ErrorCode::CatalogNotImported,
    ErrorCode::MetadataUnavailable,
    ErrorCode::ProfileRequired,
    ErrorCode::ProviderNotConfigured,
    ErrorCode::NoUsableIndexers,
    ErrorCode::UpstreamTimeout,
    ErrorCode::UpstreamAuth,
    ErrorCode::UpstreamRateLimited,
    ErrorCode::UpstreamUnavailable,
    ErrorCode::UpstreamBadResponse,
    ErrorCode::ExecutionDisabled,
    ErrorCode::ClientNotConfigured,
    ErrorCode::ImportUnsupported,
    ErrorCode::AlreadyQueued,
    ErrorCode::Blocklisted,
    ErrorCode::DuplicateTorrent,
    ErrorCode::InvalidPayload,
    ErrorCode::NoDownloadLink,
    ErrorCode::ReleaseNotFound,
    ErrorCode::GrabState,
    ErrorCode::SeedCriteriaUnmet,
    ErrorCode::CleanupBlocked,
    ErrorCode::ForeignTorrent,
    ErrorCode::NotManaged,
    ErrorCode::ActiveDownloads,
    ErrorCode::WrongPayload,
    ErrorCode::NotAdopted,
    ErrorCode::NativeHandoffRequired,
    ErrorCode::CategoryShared,
    ErrorCode::UnsupportedScope,
    ErrorCode::IndexerLimitReached,
    ErrorCode::NoPeerDiscovery,
    ErrorCode::QualityChanged,
    ErrorCode::InvalidSetup,
    ErrorCode::SetupConflict,
    ErrorCode::Internal,
];

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: ErrorCode,
    pub message: String,
    pub details: Option<Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > 500 {
            let mut cut = 500;
            while !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
        }
        ApiError {
            status,
            code,
            message,
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn bad_request(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, message)
    }

    pub fn conflict(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    /// Internal failures: details go to the log, the client gets a generic message.
    pub fn internal(context: &str, err: impl std::fmt::Display) -> Self {
        tracing::error!(context, error = %err, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            "internal error; see server log",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "code": self.code, "message": self.message });
        if let Some(d) = self.details {
            body["details"] = d;
        }
        let mut resp = (self.status, axum::Json(json!({ "error": body }))).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        resp
    }
}
