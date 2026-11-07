//! The download client: qBittorrent through its WebUI API, or the embedded engine in this
//! process. Configuration selects exactly one; there is no fallback between them. Both speak
//! the same normalised view (`qbit::TorrentInfo` and friends), so acquisition, monitoring,
//! import and cleanup policies are shared.

use crate::embedded::Embedded;
use crate::qbit::{AddRequest, Preferences, Properties, Qbit, TorrentFile, TorrentInfo};
use crate::upstream::UpstreamError;

pub enum Client {
    Qbit(Qbit),
    Embedded(Embedded),
}

impl Client {
    /// `qbittorrent` or `embedded`.
    pub fn kind(&self) -> &'static str {
        match self {
            Client::Qbit(_) => "qbittorrent",
            Client::Embedded(_) => "embedded",
        }
    }

    /// How messages name the client.
    pub fn label(&self) -> &'static str {
        match self {
            Client::Qbit(_) => "qbittorrent",
            Client::Embedded(_) => "embedded engine",
        }
    }

    pub fn embedded(&self) -> Option<&Embedded> {
        match self {
            Client::Embedded(e) => Some(e),
            Client::Qbit(_) => None,
        }
    }

    pub fn qbit(&self) -> Option<&Qbit> {
        match self {
            Client::Qbit(q) => Some(q),
            Client::Embedded(_) => None,
        }
    }

    pub fn save_path(&self) -> &str {
        match self {
            Client::Qbit(q) => q.save_path(),
            Client::Embedded(e) => e.save_path(),
        }
    }

    pub async fn info(&self, hashes: &[String]) -> Result<Vec<TorrentInfo>, UpstreamError> {
        match self {
            Client::Qbit(q) => q.info(hashes).await,
            Client::Embedded(e) => e.info(hashes).await,
        }
    }

    pub async fn files(&self, hash: &str) -> Result<Option<Vec<TorrentFile>>, UpstreamError> {
        match self {
            Client::Qbit(q) => q.files(hash).await,
            Client::Embedded(e) => e.files(hash).await,
        }
    }

    pub async fn add(&self, req: &AddRequest) -> Result<bool, UpstreamError> {
        match self {
            Client::Qbit(q) => q.add(req).await,
            Client::Embedded(e) => e.add(req).await,
        }
    }

    pub async fn pause(&self, hash: &str) -> Result<(), UpstreamError> {
        match self {
            Client::Qbit(q) => q.pause(hash).await,
            Client::Embedded(e) => e.pause(hash).await,
        }
    }

    pub async fn resume(&self, hash: &str) -> Result<(), UpstreamError> {
        match self {
            Client::Qbit(q) => q.resume(hash).await,
            Client::Embedded(e) => e.resume(hash).await,
        }
    }

    /// For the embedded engine the data always stays: the journaled removal deletes it.
    pub async fn delete(&self, hash: &str, delete_files: bool) -> Result<(), UpstreamError> {
        match self {
            Client::Qbit(q) => q.delete(hash, delete_files).await,
            Client::Embedded(e) => e.delete(hash, delete_files).await,
        }
    }

    /// qBittorrent's force start (past its queue). The embedded engine has no client-side
    /// queue: an unpaused torrent already runs.
    pub async fn set_force_start(&self, hash: &str) -> Result<(), UpstreamError> {
        match self {
            Client::Qbit(q) => q.set_force_start(hash).await,
            Client::Embedded(_) => Ok(()),
        }
    }

    pub async fn set_category(&self, hash: &str, category: &str) -> Result<(), UpstreamError> {
        match self {
            Client::Qbit(q) => q.set_category(hash, category).await,
            Client::Embedded(e) => e.set_category(hash, category).await,
        }
    }

    pub async fn preferences(&self) -> Result<Preferences, UpstreamError> {
        match self {
            Client::Qbit(q) => q.preferences().await,
            Client::Embedded(e) => Ok(e.preferences()),
        }
    }

    pub async fn properties(&self, hash: &str) -> Result<Option<Properties>, UpstreamError> {
        match self {
            Client::Qbit(q) => q.properties(hash).await,
            Client::Embedded(e) => e.properties(hash).await,
        }
    }
}
