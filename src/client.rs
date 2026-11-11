//! The download client: qBittorrent through its WebUI API, or the embedded engine in this
//! process. Configuration selects exactly one; there is no fallback between them. Both speak
//! the same normalised view (`qbit::TorrentInfo` and friends), so acquisition, monitoring,
//! import and cleanup policies are shared.
//!
//! With the `test-client` feature (integration tests only; never in a release build) a third
//! implementation, `Fake`, replaces the embedded engine when `[test_client]` is configured.

use crate::embedded::Embedded;
use crate::qbit::{AddRequest, Preferences, Properties, Qbit, TorrentFile, TorrentInfo};
use crate::upstream::UpstreamError;

#[cfg(feature = "test-client")]
pub mod fake;

pub enum Client {
    Qbit(Qbit),
    Embedded(Embedded),
    #[cfg(feature = "test-client")]
    Fake(fake::Fake),
}

impl Client {
    /// `qbittorrent` or `embedded`.
    pub fn kind(&self) -> &'static str {
        match self {
            Client::Qbit(_) => "qbittorrent",
            Client::Embedded(_) => "embedded",
            #[cfg(feature = "test-client")]
            Client::Fake(_) => "embedded",
        }
    }

    /// How messages name the client.
    pub fn label(&self) -> &'static str {
        match self {
            Client::Qbit(_) => "qbittorrent",
            Client::Embedded(_) => "embedded engine",
            #[cfg(feature = "test-client")]
            Client::Fake(_) => "embedded engine",
        }
    }

    /// Whether this client has the embedded engine's semantics (no qBittorrent behind it).
    pub fn embedded_semantics(&self) -> bool {
        !matches!(self, Client::Qbit(_))
    }

    pub fn embedded(&self) -> Option<&Embedded> {
        match self {
            Client::Embedded(e) => Some(e),
            _ => None,
        }
    }

    pub fn qbit(&self) -> Option<&Qbit> {
        match self {
            Client::Qbit(q) => Some(q),
            _ => None,
        }
    }

    pub fn save_path(&self) -> String {
        match self {
            Client::Qbit(q) => q.save_path().to_string(),
            Client::Embedded(e) => e.save_path().to_string(),
            #[cfg(feature = "test-client")]
            Client::Fake(f) => f.save_path(),
        }
    }

    pub async fn info(&self, hashes: &[String]) -> Result<Vec<TorrentInfo>, UpstreamError> {
        match self {
            Client::Qbit(q) => q.info(hashes).await,
            Client::Embedded(e) => e.info(hashes).await,
            #[cfg(feature = "test-client")]
            Client::Fake(f) => f.info(hashes).await,
        }
    }

    pub async fn files(&self, hash: &str) -> Result<Option<Vec<TorrentFile>>, UpstreamError> {
        match self {
            Client::Qbit(q) => q.files(hash).await,
            Client::Embedded(e) => e.files(hash).await,
            #[cfg(feature = "test-client")]
            Client::Fake(f) => f.files(hash).await,
        }
    }

    pub async fn add(&self, req: &AddRequest) -> Result<bool, UpstreamError> {
        match self {
            Client::Qbit(q) => q.add(req).await,
            Client::Embedded(e) => e.add(req).await,
            #[cfg(feature = "test-client")]
            Client::Fake(f) => f.add(req).await,
        }
    }

    /// For a season pack grabbed for some of its episodes and already in the embedded
    /// engine: selects the files of those episodes besides the ones already selected, and says
    /// which episodes the pack has no file for. An error saying `invalid` refuses the grab
    /// (no file of the pack is for it). Other clients: nothing to do.
    pub async fn settle_selection(
        &self,
        hash: &str,
        sel: &crate::qbit::FileSelection,
    ) -> Result<Vec<crate::ids::MediaId>, UpstreamError> {
        match self {
            Client::Embedded(e) => e.settle_selection(hash, sel).await,
            _ => Ok(vec![]),
        }
    }

    pub async fn pause(&self, hash: &str) -> Result<(), UpstreamError> {
        match self {
            Client::Qbit(q) => q.pause(hash).await,
            Client::Embedded(e) => e.pause(hash).await,
            #[cfg(feature = "test-client")]
            Client::Fake(f) => f.pause(hash).await,
        }
    }

    pub async fn resume(&self, hash: &str) -> Result<(), UpstreamError> {
        match self {
            Client::Qbit(q) => q.resume(hash).await,
            Client::Embedded(e) => e.resume(hash).await,
            #[cfg(feature = "test-client")]
            Client::Fake(f) => f.resume(hash).await,
        }
    }

    /// For the embedded engine the data always stays: the journaled removal deletes it.
    pub async fn delete(&self, hash: &str, delete_files: bool) -> Result<(), UpstreamError> {
        match self {
            Client::Qbit(q) => q.delete(hash, delete_files).await,
            Client::Embedded(e) => e.delete(hash, delete_files).await,
            #[cfg(feature = "test-client")]
            Client::Fake(f) => f.delete(hash).await,
        }
    }

    /// qBittorrent's force start (past its queue). The embedded engine has no client-side
    /// queue: an unpaused torrent already runs.
    pub async fn set_force_start(&self, hash: &str) -> Result<(), UpstreamError> {
        match self {
            Client::Qbit(q) => q.set_force_start(hash).await,
            _ => Ok(()),
        }
    }

    pub async fn set_category(&self, hash: &str, category: &str) -> Result<(), UpstreamError> {
        match self {
            Client::Qbit(q) => q.set_category(hash, category).await,
            Client::Embedded(e) => e.set_category(hash, category).await,
            #[cfg(feature = "test-client")]
            Client::Fake(f) => f.set_category(hash, category).await,
        }
    }

    pub async fn preferences(&self) -> Result<Preferences, UpstreamError> {
        match self {
            Client::Qbit(q) => q.preferences().await,
            Client::Embedded(e) => Ok(e.preferences()),
            #[cfg(feature = "test-client")]
            Client::Fake(f) => Ok(f.preferences()),
        }
    }

    pub async fn properties(&self, hash: &str) -> Result<Option<Properties>, UpstreamError> {
        match self {
            Client::Qbit(q) => q.properties(hash).await,
            Client::Embedded(e) => e.properties(hash).await,
            #[cfg(feature = "test-client")]
            Client::Fake(f) => f.properties(hash).await,
        }
    }
}
