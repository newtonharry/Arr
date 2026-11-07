//! Normalised settings produced by the importer (`settings.json`). No secrets.

use crate::quality::Quality;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SETTINGS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Movie,
    Series,
}

impl MediaKind {
    pub fn native_app(self) -> &'static str {
        match self {
            MediaKind::Movie => "radarr",
            MediaKind::Series => "sonarr",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub schema_version: u32,
    pub profiles: Vec<Profile>,
    pub size_limits: Vec<SizeLimit>,
    pub indexers: Vec<IndexerSetting>,
    pub release_policies: Vec<ReleasePolicy>,
    pub download_clients: Vec<DownloadClient>,
    pub root_folders: Vec<RootFolder>,
    /// Imported semantics that cannot be honoured; plans in scope are blocked.
    pub blocking_issues: Vec<BlockingIssue>,
    /// File-import conventions per media kind. Absent in imports made by earlier builds
    /// (re-import to enable completed-download import).
    #[serde(default)]
    pub media_management: Vec<MediaManagement>,
}

/// The native media-management/naming settings this build implements for completed-download
/// import. Anything else that would change file handling is an `Import`-scoped blocking issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaManagement {
    pub media: MediaKind,
    /// Hardlink from the download directory when on the same filesystem (else copy).
    pub copy_using_hardlinks: bool,
    /// Free space that must remain on the library filesystem after an import (MiB).
    pub minimum_free_space_mb: u64,
    /// Characters illegal in file names are replaced (native `replaceIllegalCharacters`).
    pub replace_illegal_characters: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// `radarr:<native id>` or `sonarr:<native id>`.
    pub id: String,
    pub media: MediaKind,
    pub name: String,
    pub upgrade_allowed: bool,
    /// Allowed tiers in ascending preference; qualities in a tier rank equally.
    pub tiers: Vec<Tier>,
    pub cutoff_tier: Option<String>,
    pub language: LanguageRule,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    pub name: String,
    pub qualities: Vec<Quality>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LanguageRequirement {
    Any,
    /// The media's original language.
    Original,
    OneOf {
        languages: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnmarkedLanguage {
    /// Radarr semantics: a release without language markers is in the original language.
    AssumeOriginal,
    /// Accept releases that name no language (only explicit markers are checked).
    Accept,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageRule {
    pub requirement: LanguageRequirement,
    pub unmarked: UnmarkedLanguage,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeLimit {
    pub media: MediaKind,
    pub quality: Quality,
    /// MiB per minute of runtime (native semantics).
    pub min_mib_per_minute: f64,
    /// `None` = unlimited.
    pub max_mib_per_minute: Option<f64>,
    pub preferred_mib_per_minute: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRef {
    pub app: String,
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerSetting {
    pub media: MediaKind,
    pub prowlarr_indexer_id: u32,
    pub name: String,
    pub native: NativeRef,
    pub protocol: String,
    pub categories: Vec<u32>,
    pub priority: i32,
    pub minimum_seeders: u32,
    pub enable_automatic_search: bool,
    pub enable_interactive_search: bool,
    pub enable_rss: bool,
    pub seed_ratio: Option<f64>,
    pub seed_time_minutes: Option<i64>,
    pub season_pack_seed_time_minutes: Option<i64>,
    /// Per-indexer rules that cannot be evaluated; releases from this indexer are rejected.
    pub unsupported: Vec<String>,
    /// The Prowlarr indexer this setting referenced: implementation, definition and its
    /// non-secret settings, so a native indexer serving the same id can be checked against
    /// it. Absent in imports made by earlier builds and in managed setup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<IndexerSource>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerSource {
    pub implementation: Option<String>,
    pub definition: Option<String>,
    /// Non-secret field values (bounded). Credential fields are never copied.
    pub settings: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePolicy {
    pub media: MediaKind,
    /// 0 = unlimited (native semantics, MB).
    pub maximum_size_mb: i64,
    pub allow_hardcoded_subs: bool,
    pub prefer_propers_and_repacks: bool,
    pub torrent_enabled: bool,
    pub torrent_delay_minutes: i64,
    pub bypass_delay_if_highest_quality: bool,
    /// `downloadPropersAndRepacks = preferAndUpgrade`: an exact-quality higher revision may
    /// replace an owned file even with profile upgrades disabled (native semantics). Absent
    /// in imports made by earlier builds, where it reads as false (fail closed).
    #[serde(default)]
    pub revision_upgrades: bool,
    /// `autoRedownloadFailed`: search again after a failed automatic download.
    #[serde(default)]
    pub redownload_failed: bool,
    /// `autoRedownloadFailedFromInteractiveSearch`: the same for manually selected releases.
    #[serde(default)]
    pub redownload_failed_interactive: bool,
    /// Native `rssSyncInterval` in minutes (0 = RSS disabled). Absent in imports made by
    /// earlier builds, where the 15 minute native default applies.
    #[serde(default)]
    pub rss_sync_interval_minutes: Option<u32>,
    /// Radarr `availabilityDelay` in days (movies).
    #[serde(default)]
    pub availability_delay_days: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadClient {
    pub media: MediaKind,
    pub native: NativeRef,
    pub implementation: String,
    pub protocol: String,
    pub enabled: bool,
    pub priority: i64,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub use_ssl: Option<bool>,
    pub url_base: Option<String>,
    pub category: Option<String>,
    pub post_import_category: Option<String>,
    pub initial_state: Option<i64>,
    pub sequential_order: Option<bool>,
    pub first_and_last: Option<bool>,
    pub content_layout: Option<i64>,
    pub remove_completed_downloads: Option<bool>,
    pub remove_failed_downloads: Option<bool>,
    /// Names of credential fields present in the snapshot. Values are never copied.
    pub credential_fields_in_snapshot: Vec<String>,
    /// `server_config`: submission is governed by the server's `[execution]` and `[qbittorrent]`
    /// config, never by the snapshot (earlier imports recorded `disabled`).
    pub execution: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootFolder {
    pub media: MediaKind,
    pub path: String,
    pub accessible_at_snapshot: Option<bool>,
    pub free_space_bytes_at_snapshot: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IssueScope {
    Media {
        media: MediaKind,
    },
    Profile {
        profile: String,
    },
    /// Blocks completed-download import (file handling) only; searches and plans proceed.
    Import {
        media: MediaKind,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockingIssue {
    pub scope: IssueScope,
    pub code: String,
    pub message: String,
}

impl Settings {
    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// The only profile for a media kind, if exactly one exists.
    pub fn sole_profile(&self, media: MediaKind) -> Option<&Profile> {
        let mut it = self.profiles.iter().filter(|p| p.media == media);
        match (it.next(), it.next()) {
            (Some(p), None) => Some(p),
            _ => None,
        }
    }

    pub fn size_limit(&self, media: MediaKind, quality: Quality) -> Option<&SizeLimit> {
        self.size_limits
            .iter()
            .find(|s| s.media == media && s.quality == quality)
    }

    pub fn policy(&self, media: MediaKind) -> Option<&ReleasePolicy> {
        self.release_policies.iter().find(|p| p.media == media)
    }

    pub fn indexer(&self, media: MediaKind, prowlarr_id: u32) -> Option<&IndexerSetting> {
        self.indexers
            .iter()
            .find(|i| i.media == media && i.prowlarr_indexer_id == prowlarr_id)
    }

    /// Indexers used for automatic (plan) or interactive (search) queries.
    pub fn search_indexers(&self, media: MediaKind, automatic: bool) -> Vec<&IndexerSetting> {
        let mut v: Vec<_> = self
            .indexers
            .iter()
            .filter(|i| i.media == media)
            .filter(|i| {
                if automatic {
                    i.enable_automatic_search
                } else {
                    i.enable_interactive_search
                }
            })
            .collect();
        v.sort_by_key(|i| (i.priority, i.prowlarr_indexer_id));
        v
    }

    /// Blocking issues for a media kind and (optionally) profile.
    pub fn blocking_for(&self, media: MediaKind, profile: Option<&str>) -> Vec<&BlockingIssue> {
        self.blocking_issues
            .iter()
            .filter(|b| match &b.scope {
                IssueScope::Media { media: m } => *m == media,
                IssueScope::Profile { profile: p } => Some(p.as_str()) == profile,
                IssueScope::Import { .. } => false,
            })
            .collect()
    }

    /// Issues that block completed-download import for a media kind.
    pub fn import_blockers(&self, media: MediaKind) -> Vec<&BlockingIssue> {
        self.blocking_issues
            .iter()
            .filter(|b| b.scope == IssueScope::Import { media })
            .collect()
    }

    pub fn media_management(&self, media: MediaKind) -> Option<&MediaManagement> {
        self.media_management.iter().find(|m| m.media == media)
    }

    pub fn download_client(&self, media: MediaKind) -> Option<&DownloadClient> {
        let mut it = self.download_clients.iter().filter(|c| {
            c.media == media && c.enabled && c.protocol.eq_ignore_ascii_case("torrent")
        });
        let first = it.next()?;
        // Several enabled torrent clients: the lowest native priority value wins (native order).
        Some(it.fold(
            first,
            |best, c| if c.priority < best.priority { c } else { best },
        ))
    }
}

impl Profile {
    /// Index of the tier containing `quality` (higher = preferred), if allowed.
    pub fn tier_of(&self, quality: Quality) -> Option<usize> {
        self.tiers
            .iter()
            .position(|t| t.qualities.contains(&quality))
    }
}
