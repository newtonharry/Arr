//! Managed setup: settings for a standalone deployment, without a Radarr/Sonarr/Prowlarr
//! snapshot.
//!
//! A setup document (TOML for the CLI, JSON for `POST /api/v1/setup`) names policy only:
//! quality profiles, size limits, per-media release and file-import policy, root folders,
//! the download policy (categories, initial state, cleanup) and per-indexer policy (media,
//! categories, priority, minimum seeders, search/RSS switches and explicit seed goals).
//! Connections and credentials stay in `config.toml` (credentials only as file paths). Every
//! key is optional: the defaults are listed in `config/setup.example.toml`.
//!
//! The document is normalised into the same `Settings` contract a snapshot import produces,
//! plus an empty catalog and a report, and written as an immutable, digest-verified import
//! (`source-setup.json` instead of `source-native.json`), then activated. Ids are stable:
//! profiles are `setup:<id>`, indexer policies keep the configured indexer id. Setup never
//! enables execution, never queues a search and never replaces a native snapshot import; a
//! later setup must keep every profile and root folder the catalog uses.

use crate::catalog::{CATALOG_SCHEMA_VERSION, Catalog, CatalogIndex};
use crate::config::{PlexConfig, RuntimeConfig};
use crate::import::{
    self, IMPORT_SCHEMA_VERSION, ItemStatus, MigrationReport, Normalized, ReportItem,
    ReportSummary, SourceSummary,
};
use crate::plex::Section;
use crate::quality::Quality;
use crate::settings::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The source file a managed setup import carries instead of `source-native.json`.
pub const SOURCE_FILE: &str = "source-setup.json";
pub const SETUP_SCHEMA_VERSION: u32 = 1;
/// Profile ids are `setup:<id>`.
pub const PROFILE_PREFIX: &str = "setup:";

const DEFAULT_SEED_RATIO: f64 = 1.0;
const DEFAULT_SEED_MINUTES: i64 = 4320;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupDoc {
    /// `1` (the default).
    #[serde(default)]
    pub schema_version: Option<u32>,
    #[serde(default)]
    pub movies: MediaPolicy,
    #[serde(default)]
    pub tv: MediaPolicy,
    /// Quality profiles. Default: one HD (720p/1080p) profile per enabled media kind.
    #[serde(default)]
    pub profiles: Vec<ProfileDoc>,
    /// Size-limit overrides (MiB per minute of runtime); defaults cover every quality.
    #[serde(default)]
    pub size_limits: Vec<SizeLimitDoc>,
    #[serde(default)]
    pub download: DownloadDoc,
    /// Per-indexer policy. Default: every configured `[[indexer]]`, for movies and TV.
    #[serde(default)]
    pub indexers: Option<Vec<IndexerDoc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaPolicy {
    /// Default: true when the matching `[library]` root is configured (or `root` is set).
    pub enabled: Option<bool>,
    /// Root folder (absolute). Default: the `[library]` root itself.
    pub root: Option<String>,
    /// MB; 0 = unlimited (default).
    pub maximum_size_mb: Option<i64>,
    pub allow_hardcoded_subs: Option<bool>,
    /// Rank PROPER/REPACK above the same quality (default true).
    pub prefer_propers_and_repacks: Option<bool>,
    /// Replace an owned file with a higher revision of exactly the same quality (default
    /// false: nothing owned is replaced unless asked).
    pub revision_upgrades: Option<bool>,
    pub torrent_delay_minutes: Option<i64>,
    pub bypass_delay_if_highest_quality: Option<bool>,
    /// Search again after a failed automatic download (default true).
    pub redownload_failed: Option<bool>,
    pub redownload_failed_interactive: Option<bool>,
    /// RSS interval in minutes (default 15; 0 disables RSS for this media kind). RSS runs
    /// only with execution enabled.
    pub rss_sync_interval_minutes: Option<u32>,
    /// Movies: days after release before a movie is available (default 0).
    pub availability_delay_days: Option<i64>,
    pub copy_using_hardlinks: Option<bool>,
    /// Free space that must remain after an import (MiB, default 100).
    pub minimum_free_space_mb: Option<u64>,
    pub replace_illegal_characters: Option<bool>,
}

/// `"original"`, `"any"` or a list of language names.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LanguageDoc {
    Mode(String),
    OneOf(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDoc {
    /// Stored as `setup:<id>`; letters, digits, `-` and `_`.
    pub id: String,
    /// `movie` or `series`.
    pub media: String,
    pub name: Option<String>,
    /// Allowed qualities in tiers of ascending preference (qualities in one tier rank
    /// equally), e.g. `[["webdl_720p", "webrip_720p"], ["webdl_1080p", "webrip_1080p"]]`.
    pub tiers: Vec<Vec<String>>,
    /// Default: `original` for movies, `any` for series.
    pub language: Option<LanguageDoc>,
    /// Releases without a language marker: `assume_original` (default for movies) or
    /// `accept` (default for series).
    pub unmarked: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeLimitDoc {
    /// `movie`, `series` or `both`.
    pub media: String,
    pub quality: String,
    pub min_mib_per_minute: f64,
    pub max_mib_per_minute: Option<f64>,
    pub preferred_mib_per_minute: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadDoc {
    /// Default `um-movies` (a download client's own category setting overrides it).
    pub movie_category: Option<String>,
    /// Default `um-tv`.
    pub tv_category: Option<String>,
    /// `start` (default), `force_start` or `paused`.
    pub initial_state: Option<String>,
    /// Remove a download and its data after a verified import once its seed goal and share
    /// limit are met (default true; `[execution] download_cleanup = false` overrides).
    pub remove_completed: Option<bool>,
    /// Remove failed downloads (default true).
    pub remove_failed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerDoc {
    /// The `[[indexer]]` id (or, with `[prowlarr]`, the Prowlarr indexer id).
    pub id: u32,
    pub name: Option<String>,
    /// Search it for movies / TV (default true for enabled media kinds).
    pub movies: Option<bool>,
    pub tv: Option<bool>,
    /// Default `[2000]` / `[5000]` (each covers its subcategories).
    pub movie_categories: Option<Vec<u32>>,
    pub tv_categories: Option<Vec<u32>>,
    /// 1 (first) to 50; default 25.
    pub priority: Option<i32>,
    /// Default 1.
    pub minimum_seeders: Option<u32>,
    pub automatic_search: Option<bool>,
    pub interactive_search: Option<bool>,
    pub rss: Option<bool>,
    /// Seed goal: ratio (default 1.0) or seeding time (default 4320 min), whichever comes
    /// first; a negative value leaves that half to the download client's own limits.
    pub seed_ratio: Option<f64>,
    pub seed_time_minutes: Option<i64>,
    /// Default: `seed_time_minutes`.
    pub season_pack_seed_time_minutes: Option<i64>,
}

fn qualities_named(tiers: &[(&str, &[Quality])]) -> Vec<Tier> {
    tiers
        .iter()
        .map(|(name, q)| Tier {
            name: (*name).into(),
            qualities: q.to_vec(),
        })
        .collect()
}

/// The default HD profile tiers (ascending preference, like the native "HD - 720p/1080p").
fn hd_tiers() -> Vec<Tier> {
    use Quality::*;
    qualities_named(&[
        ("HDTV-720p", &[Hdtv720]),
        ("WEB 720p", &[Webdl720, Webrip720]),
        ("Bluray-720p", &[Bluray720]),
        ("HDTV-1080p", &[Hdtv1080]),
        ("WEB 1080p", &[Webdl1080, Webrip1080]),
        ("Bluray-1080p", &[Bluray1080]),
    ])
}

/// Default size limits (MiB per minute of runtime): a floor against fake or broken
/// releases, no ceiling.
fn default_min(q: Quality) -> f64 {
    let s = q.as_str();
    if s.contains("remux") {
        50.0
    } else if s.contains("2160") {
        10.0
    } else if s.contains("1080") {
        4.0
    } else if s.contains("720") {
        3.0
    } else {
        1.0
    }
}

fn quality(raw: &str) -> Option<Quality> {
    serde_json::from_value::<Quality>(json!(raw.trim().to_ascii_lowercase()))
        .ok()
        .or_else(|| Quality::from_native_name(raw.trim()))
        .filter(|q| *q != Quality::Unknown)
}

fn media_kind(raw: &str) -> Option<MediaKind> {
    match raw {
        "movie" | "movies" => Some(MediaKind::Movie),
        "series" | "tv" => Some(MediaKind::Series),
        _ => None,
    }
}

fn plain_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn category(raw: Option<&str>, default: &str, what: &str, errs: &mut Vec<String>) -> String {
    let c = raw.unwrap_or(default).trim().to_string();
    if c.is_empty()
        || c.len() > 64
        || c.chars()
            .any(|ch| ch.is_control() || matches!(ch, '/' | '\\' | ','))
    {
        errs.push(format!(
            "{what} must be 1-64 printable characters without '/', '\\' or ','"
        ));
    }
    c
}

fn absolute(raw: &str, what: &str, errs: &mut Vec<String>) -> Option<String> {
    let p = Path::new(raw.trim());
    if !p.is_absolute()
        || p.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        errs.push(format!(
            "{what} must be an absolute path without '.' or '..' components"
        ));
        return None;
    }
    let s = p.to_string_lossy().trim_end_matches('/').to_string();
    Some(if s.is_empty() { "/".into() } else { s })
}

struct Report {
    items: Vec<ReportItem>,
}

impl Report {
    fn item(&mut self, area: &str, setting: &str, value: Value, effect: impl Into<String>) {
        self.items.push(ReportItem {
            app: "setup".into(),
            area: area.into(),
            setting: setting.into(),
            value,
            status: ItemStatus::Enforced,
            effect: effect.into(),
        });
    }
}

/// The normalised settings for `doc` under the running configuration, with the content
/// digest the import id derives from. Every validation problem is returned at once.
pub fn normalize(
    doc: &SetupDoc,
    cfg: &RuntimeConfig,
) -> Result<(Settings, Vec<ReportItem>, Vec<String>), Vec<String>> {
    let mut errs: Vec<String> = vec![];
    let mut warnings: Vec<String> = vec![];
    let mut rep = Report { items: vec![] };
    if doc
        .schema_version
        .is_some_and(|v| v != SETUP_SCHEMA_VERSION)
    {
        errs.push(format!(
            "schema_version must be {SETUP_SCHEMA_VERSION} (this build's setup schema)"
        ));
    }
    let lib_root = |m: MediaKind| {
        cfg.library.as_ref().and_then(|l| match m {
            MediaKind::Movie => l.movies.as_ref(),
            MediaKind::Series => l.tv.as_ref(),
        })
    };
    // Media kinds.
    let mut enabled: BTreeMap<MediaKind, &MediaPolicy> = BTreeMap::new();
    let mut roots: Vec<RootFolder> = vec![];
    for (m, p, key) in [
        (MediaKind::Movie, &doc.movies, "movies"),
        (MediaKind::Series, &doc.tv, "tv"),
    ] {
        let root = match (&p.root, lib_root(m)) {
            (Some(r), _) => absolute(r, &format!("{key}.root"), &mut errs),
            (None, Some(l)) => Some(l.to_string_lossy().trim_end_matches('/').to_string()),
            (None, None) => None,
        };
        let on = p.enabled.unwrap_or(root.is_some());
        if !on {
            rep.item(
                key,
                "enabled",
                json!(false),
                "not managed: no profile, indexer or download policy for this media kind",
            );
            continue;
        }
        enabled.insert(m, p);
        match root {
            Some(path) => {
                if let Some(l) = lib_root(m)
                    && Path::new(&path) != l.as_path()
                {
                    warnings.push(format!(
                        "{key}.root {path} differs from [library] {key} {}: set [library] {key}_native_root = \"{path}\" so imports map it",
                        l.display()
                    ));
                }
                rep.item(key, "root folder", json!(path), "new titles are filed here (Root/Folder)");
                roots.push(RootFolder {
                    media: m,
                    path,
                    accessible_at_snapshot: None,
                    free_space_bytes_at_snapshot: None,
                });
            }
            None => warnings.push(format!(
                "{key} has no root folder: configure [library] {key} (or {key}.root); execution for {key} stays blocked until then"
            )),
        }
        for (v, what, lo, hi) in [
            (p.maximum_size_mb, "maximum_size_mb", 0, 10_000_000),
            (p.torrent_delay_minutes, "torrent_delay_minutes", 0, 100_000),
            (
                p.availability_delay_days,
                "availability_delay_days",
                0,
                3650,
            ),
        ] {
            if v.is_some_and(|v| !(lo..=hi).contains(&v)) {
                errs.push(format!("{key}.{what} must be between {lo} and {hi}"));
            }
        }
        if m == MediaKind::Series && p.availability_delay_days.is_some() {
            errs.push("tv.availability_delay_days applies to movies only".into());
        }
        if p.rss_sync_interval_minutes
            .is_some_and(|v| v != 0 && !(10..=1440).contains(&v))
        {
            errs.push(format!(
                "{key}.rss_sync_interval_minutes must be 0 (off) or 10 to 1440"
            ));
        }
        if p.minimum_free_space_mb.is_some_and(|v| v > 10_000_000) {
            errs.push(format!("{key}.minimum_free_space_mb is too large"));
        }
    }
    if enabled.is_empty() {
        errs.push("no media kind is enabled: configure [library] movies and/or tv, or set movies.enabled / tv.enabled with a root".into());
    }

    // Profiles.
    let mut profiles: Vec<Profile> = vec![];
    let defaults_needed: Vec<MediaKind> = enabled
        .keys()
        .copied()
        .filter(|m| {
            !doc.profiles
                .iter()
                .any(|p| media_kind(&p.media) == Some(*m))
        })
        .collect();
    for m in defaults_needed {
        let (id, name) = match m {
            MediaKind::Movie => ("movies-hd", "HD 720p/1080p (movies)"),
            MediaKind::Series => ("tv-hd", "HD 720p/1080p (TV)"),
        };
        profiles.push(Profile {
            id: format!("{PROFILE_PREFIX}{id}"),
            media: m,
            name: name.into(),
            upgrade_allowed: false,
            tiers: hd_tiers(),
            cutoff_tier: None,
            language: default_language(m),
        });
    }
    let mut ids = BTreeSet::new();
    for p in &doc.profiles {
        let what = format!("profile {:?}", p.id);
        if !plain_id(&p.id) {
            errs.push(format!(
                "{what}: id must be 1-64 letters, digits, '-' or '_'"
            ));
            continue;
        }
        if !ids.insert(p.id.clone()) {
            errs.push(format!("{what} is defined twice"));
        }
        let Some(m) = media_kind(&p.media) else {
            errs.push(format!("{what}: media must be \"movie\" or \"series\""));
            continue;
        };
        if !enabled.contains_key(&m) {
            errs.push(format!(
                "{what} is for {}, which is not enabled",
                m.native_app()
            ));
        }
        if p.tiers.is_empty() || p.tiers.iter().any(Vec::is_empty) {
            errs.push(format!(
                "{what}: tiers must be non-empty lists of qualities"
            ));
        }
        let mut seen = BTreeSet::new();
        let mut tiers = vec![];
        for tier in &p.tiers {
            let mut qs = vec![];
            for q in tier {
                match quality(q) {
                    Some(q) if seen.insert(q) => qs.push(q),
                    Some(q) => errs.push(format!("{what}: {} appears twice", q.as_str())),
                    None => errs.push(format!("{what}: unknown quality {q:?}")),
                }
            }
            tiers.push(Tier {
                name: qs.iter().map(|q| q.as_str()).collect::<Vec<_>>().join("/"),
                qualities: qs,
            });
        }
        let mut language = default_language(m);
        match &p.language {
            None => {}
            Some(LanguageDoc::Mode(s)) if s == "original" => {
                language.requirement = LanguageRequirement::Original
            }
            Some(LanguageDoc::Mode(s)) if s == "any" => {
                language.requirement = LanguageRequirement::Any
            }
            Some(LanguageDoc::OneOf(list))
                if !list.is_empty()
                    && list.len() <= 20
                    && list.iter().all(|l| {
                        !l.trim().is_empty() && l.len() <= 40 && l.chars().all(char::is_alphabetic)
                    }) =>
            {
                language.requirement = LanguageRequirement::OneOf {
                    languages: list.iter().map(|l| l.trim().to_string()).collect(),
                }
            }
            Some(_) => errs.push(format!(
                "{what}: language must be \"original\", \"any\" or a list of language names"
            )),
        }
        match p.unmarked.as_deref() {
            None => {}
            Some("assume_original") => language.unmarked = UnmarkedLanguage::AssumeOriginal,
            Some("accept") => language.unmarked = UnmarkedLanguage::Accept,
            Some(_) => errs.push(format!(
                "{what}: unmarked must be \"assume_original\" or \"accept\""
            )),
        }
        language.source = "setup".into();
        let name = p.name.clone().unwrap_or_else(|| p.id.clone());
        if name.chars().count() > 100 || name.chars().any(char::is_control) {
            errs.push(format!(
                "{what}: name must be at most 100 printable characters"
            ));
        }
        profiles.push(Profile {
            id: format!("{PROFILE_PREFIX}{}", p.id),
            media: m,
            name,
            upgrade_allowed: false,
            tiers,
            cutoff_tier: None,
            language,
        });
    }
    for m in enabled.keys() {
        let n = profiles.iter().filter(|p| p.media == *m).count();
        if n > 1 {
            warnings.push(format!(
                "{} has {n} profiles: adds must name quality_profile (the MCP add_media tool cannot)",
                m.native_app()
            ));
        }
    }
    for p in &profiles {
        rep.item(
            &format!("profile {}", p.id),
            "tiers",
            json!(p.tiers),
            "releases must match an allowed quality; a higher tier ranks first; upgrades of owned files are not performed",
        );
        rep.item(
            &format!("profile {}", p.id),
            "language",
            json!(p.language),
            "language rule for releases and payload names",
        );
    }

    // Size limits: defaults for every quality, then overrides.
    let mut limits: BTreeMap<(MediaKind, Quality), SizeLimit> = BTreeMap::new();
    for m in enabled.keys() {
        for q in Quality::ALL
            .iter()
            .copied()
            .filter(|q| *q != Quality::Unknown)
        {
            limits.insert(
                (*m, q),
                SizeLimit {
                    media: *m,
                    quality: q,
                    min_mib_per_minute: default_min(q),
                    max_mib_per_minute: None,
                    preferred_mib_per_minute: None,
                },
            );
        }
    }
    for s in &doc.size_limits {
        let media: Vec<MediaKind> = match s.media.as_str() {
            "both" => enabled.keys().copied().collect(),
            other => match media_kind(other) {
                Some(m) => vec![m],
                None => {
                    errs.push(format!(
                        "size_limits: media must be \"movie\", \"series\" or \"both\" (got {other:?})"
                    ));
                    continue;
                }
            },
        };
        let Some(q) = quality(&s.quality) else {
            errs.push(format!("size_limits: unknown quality {:?}", s.quality));
            continue;
        };
        let ok = |v: f64| v.is_finite() && (0.0..=100_000.0).contains(&v);
        if !ok(s.min_mib_per_minute)
            || s.max_mib_per_minute
                .is_some_and(|v| !ok(v) || v < s.min_mib_per_minute)
            || s.preferred_mib_per_minute.is_some_and(|v| !ok(v))
        {
            errs.push(format!(
                "size_limits {}: limits must be 0 to 100000 MiB/min with max >= min",
                q.as_str()
            ));
            continue;
        }
        for m in media {
            limits.insert(
                (m, q),
                SizeLimit {
                    media: m,
                    quality: q,
                    min_mib_per_minute: s.min_mib_per_minute,
                    max_mib_per_minute: s.max_mib_per_minute,
                    preferred_mib_per_minute: s.preferred_mib_per_minute,
                },
            );
        }
    }
    let size_limits: Vec<SizeLimit> = limits.into_values().collect();
    rep.item(
        "size_limits",
        "MiB per minute of runtime",
        json!(size_limits.iter().map(|l| json!({ "media": l.media, "quality": l.quality, "min": l.min_mib_per_minute, "max": l.max_mib_per_minute, "preferred": l.preferred_mib_per_minute })).collect::<Vec<_>>()),
        "size must fall within min/max x runtime (x episodes for TV); an unknown runtime with a minimum rejects the release",
    );

    // Release, file-import and download policy per media kind.
    let mut release_policies = vec![];
    let mut media_management = vec![];
    let mut download_clients = vec![];
    let d = &doc.download;
    let initial_state = match d.initial_state.as_deref() {
        None | Some("start") => 0,
        Some("force_start") => 1,
        Some("paused") => 2,
        Some(other) => {
            errs.push(format!(
                "download.initial_state must be \"start\", \"force_start\" or \"paused\" (got {other:?})"
            ));
            0
        }
    };
    for (m, p) in &enabled {
        let rp = ReleasePolicy {
            media: *m,
            maximum_size_mb: p.maximum_size_mb.unwrap_or(0),
            allow_hardcoded_subs: p.allow_hardcoded_subs.unwrap_or(false),
            prefer_propers_and_repacks: p.prefer_propers_and_repacks.unwrap_or(true),
            torrent_enabled: true,
            torrent_delay_minutes: p.torrent_delay_minutes.unwrap_or(0),
            bypass_delay_if_highest_quality: p.bypass_delay_if_highest_quality.unwrap_or(true),
            revision_upgrades: p.revision_upgrades.unwrap_or(false),
            redownload_failed: p.redownload_failed.unwrap_or(true),
            redownload_failed_interactive: p.redownload_failed_interactive.unwrap_or(false),
            rss_sync_interval_minutes: Some(p.rss_sync_interval_minutes.unwrap_or(15)),
            availability_delay_days: p.availability_delay_days.unwrap_or(0),
        };
        rep.item(
            m.native_app(),
            "release policy",
            json!(rp),
            "decisions, delay, revision handling, failed-download re-search and RSS interval",
        );
        release_policies.push(rp);
        let mm = MediaManagement {
            media: *m,
            copy_using_hardlinks: p.copy_using_hardlinks.unwrap_or(true),
            minimum_free_space_mb: p.minimum_free_space_mb.unwrap_or(100),
            replace_illegal_characters: p.replace_illegal_characters.unwrap_or(true),
        };
        rep.item(m.native_app(), "file import", json!(mm), "completed-download import: hardlink when possible, free-space floor, safe names (rename off)");
        media_management.push(mm);
        let (key, default) = match m {
            MediaKind::Movie => ("download.movie_category", "um-movies"),
            MediaKind::Series => ("download.tv_category", "um-tv"),
        };
        let raw = match m {
            MediaKind::Movie => d.movie_category.as_deref(),
            MediaKind::Series => d.tv_category.as_deref(),
        };
        let cat = category(raw, default, key, &mut errs);
        let dc = DownloadClient {
            media: *m,
            native: NativeRef {
                app: "setup".into(),
                id: 0,
                name: "managed download policy".into(),
            },
            implementation: MANAGED_CLIENT.into(),
            protocol: "torrent".into(),
            enabled: true,
            priority: 1,
            host: None,
            port: None,
            use_ssl: None,
            url_base: None,
            category: Some(cat),
            post_import_category: None,
            initial_state: Some(initial_state),
            sequential_order: Some(false),
            first_and_last: Some(false),
            content_layout: Some(0),
            remove_completed_downloads: Some(d.remove_completed.unwrap_or(true)),
            remove_failed_downloads: Some(d.remove_failed.unwrap_or(true)),
            credential_fields_in_snapshot: vec![],
            execution: "server_config".into(),
        };
        rep.item(m.native_app(), "download policy", json!({ "category": dc.category, "initial_state": initial_state, "remove_completed": dc.remove_completed_downloads, "remove_failed": dc.remove_failed_downloads }), "applied through the configured download client ([embedded] or [qbittorrent]); cleanup waits for the seed goal and share limit");
        download_clients.push(dc);
    }

    // Indexer policy.
    let native_ids: Vec<u32> = cfg.indexers.iter().map(|i| i.id).collect();
    let docs: Vec<IndexerDoc> = match &doc.indexers {
        Some(list) => list.clone(),
        None => cfg
            .indexers
            .iter()
            .map(|i| IndexerDoc {
                id: i.id,
                name: None,
                movies: None,
                tv: None,
                movie_categories: None,
                tv_categories: None,
                priority: None,
                minimum_seeders: None,
                automatic_search: None,
                interactive_search: None,
                rss: None,
                seed_ratio: None,
                seed_time_minutes: None,
                season_pack_seed_time_minutes: None,
            })
            .collect(),
    };
    if docs.is_empty() {
        warnings.push(
            "no indexer policy: configure [[indexer]] entries (or list indexers with their Prowlarr ids); searches fail with no_usable_indexers until then".into(),
        );
    }
    let mut indexers = vec![];
    let mut seen_ids = BTreeSet::new();
    for ix in &docs {
        let what = format!("indexer {}", ix.id);
        if ix.id == 0 || ix.id > i32::MAX as u32 {
            errs.push(format!("{what}: id must be positive"));
            continue;
        }
        if !seen_ids.insert(ix.id) {
            errs.push(format!("{what} is listed twice"));
            continue;
        }
        let native = cfg.indexers.iter().find(|n| n.id == ix.id);
        if !native_ids.is_empty() && native.is_none() {
            errs.push(format!(
                "{what} is not a configured [[indexer]] (configured: {native_ids:?})"
            ));
            continue;
        }
        if native_ids.is_empty() && cfg.prowlarr.is_none() {
            warnings.push(format!(
                "{what}: no search backend is configured; add [[indexer]] id = {} (or [prowlarr])",
                ix.id
            ));
        }
        let name = ix
            .name
            .clone()
            .or_else(|| native.map(|n| n.name.clone()))
            .unwrap_or_else(|| format!("indexer {}", ix.id));
        if name.trim().is_empty()
            || name.chars().count() > 100
            || name.chars().any(char::is_control)
        {
            errs.push(format!("{what}: name must be 1-100 printable characters"));
        }
        let priority = ix.priority.unwrap_or(25);
        if !(1..=50).contains(&priority) {
            errs.push(format!("{what}: priority must be 1 to 50"));
        }
        let min_seeders = ix.minimum_seeders.unwrap_or(1);
        if min_seeders > 100_000 {
            errs.push(format!("{what}: minimum_seeders is too large"));
        }
        let ratio = ix.seed_ratio.unwrap_or(DEFAULT_SEED_RATIO);
        let minutes = ix.seed_time_minutes.unwrap_or(DEFAULT_SEED_MINUTES);
        let pack = ix.season_pack_seed_time_minutes.unwrap_or(minutes);
        if !ratio.is_finite() || ratio > 1000.0 {
            errs.push(format!(
                "{what}: seed_ratio must be at most 1000 (negative: the client's limit)"
            ));
        }
        for (v, k) in [
            (minutes, "seed_time_minutes"),
            (pack, "season_pack_seed_time_minutes"),
        ] {
            if v > 5_256_000 {
                errs.push(format!(
                    "{what}: {k} must be at most 5256000 (negative: the client's limit)"
                ));
            }
        }
        if ratio < 0.0 && minutes < 0 {
            warnings.push(format!(
                "{what}: no seed goal (ratio and time are both left to the download client); cleanup then waits for the client's own share limits"
            ));
        }
        for (m, on, cats, default, range, key) in [
            (
                MediaKind::Movie,
                ix.movies,
                &ix.movie_categories,
                2000u32,
                2000..=2999u32,
                "movie_categories",
            ),
            (
                MediaKind::Series,
                ix.tv,
                &ix.tv_categories,
                5000,
                5000..=5999,
                "tv_categories",
            ),
        ] {
            let on = on.unwrap_or(true);
            if on && !enabled.contains_key(&m) {
                if (m == MediaKind::Movie && ix.movies == Some(true))
                    || (m == MediaKind::Series && ix.tv == Some(true))
                {
                    errs.push(format!(
                        "{what} is enabled for {}, which is not enabled",
                        m.native_app()
                    ));
                }
                continue;
            }
            if !on {
                continue;
            }
            let cats = cats.clone().unwrap_or_else(|| vec![default]);
            if cats.is_empty() || cats.len() > 50 || cats.iter().any(|c| !range.contains(c)) {
                errs.push(format!(
                    "{what}: {key} must list 1-50 categories in {}-{}",
                    range.start(),
                    range.end()
                ));
                continue;
            }
            let setting = IndexerSetting {
                media: m,
                prowlarr_indexer_id: ix.id,
                name: name.clone(),
                native: NativeRef {
                    app: "setup".into(),
                    id: i64::from(ix.id),
                    name: name.clone(),
                },
                protocol: "torrent".into(),
                categories: cats,
                priority,
                minimum_seeders: min_seeders,
                enable_automatic_search: ix.automatic_search.unwrap_or(true),
                enable_interactive_search: ix.interactive_search.unwrap_or(true),
                enable_rss: ix.rss.unwrap_or(true),
                seed_ratio: Some(ratio),
                seed_time_minutes: Some(minutes),
                season_pack_seed_time_minutes: Some(pack),
                unsupported: vec![],
                source: None,
            };
            rep.item(
                &format!("indexer[{}] {name}", ix.id),
                m.native_app(),
                json!({ "categories": setting.categories, "priority": priority, "minimum_seeders": min_seeders, "automatic_search": setting.enable_automatic_search, "interactive_search": setting.enable_interactive_search, "rss": setting.enable_rss, "seed_ratio": ratio, "seed_time_minutes": minutes, "season_pack_seed_time_minutes": pack }),
                "searched with these categories; priority and minimum seeders applied; seed goal: ratio or seeding time, whichever comes first (negative = the client's limit)",
            );
            indexers.push(setting);
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    let settings = Settings {
        schema_version: SETTINGS_SCHEMA_VERSION,
        profiles,
        size_limits,
        indexers,
        release_policies,
        download_clients,
        root_folders: roots,
        blocking_issues: vec![],
        media_management,
    };
    Ok((settings, rep.items, warnings))
}

/// The implementation name of the managed download policy.
pub const MANAGED_CLIENT: &str = "managed";

fn default_language(m: MediaKind) -> LanguageRule {
    match m {
        MediaKind::Movie => LanguageRule {
            requirement: LanguageRequirement::Original,
            unmarked: UnmarkedLanguage::AssumeOriginal,
            source: "setup default (movies: original language)".into(),
        },
        MediaKind::Series => LanguageRule {
            requirement: LanguageRequirement::Any,
            unmarked: UnmarkedLanguage::Accept,
            source: "setup default (series: any language)".into(),
        },
    }
}

/// A normalised setup: the import to write (unless it exists) and what it says.
pub struct Prepared {
    pub normalized: Normalized,
    /// The canonical document kept as `source-setup.json`.
    pub source: Vec<u8>,
    pub warnings: Vec<String>,
}

/// Normalises `doc` into a complete import (settings, empty catalog, report).
pub fn prepare(
    doc: &SetupDoc,
    cfg: &RuntimeConfig,
    now: time::OffsetDateTime,
) -> Result<Prepared, Vec<String>> {
    let (settings, items, warnings) = normalize(doc, cfg)?;
    let source =
        serde_json::to_vec_pretty(doc).map_err(|e| vec![format!("setup document: {e}")])?;
    let digest = import::sha256_hex(
        [
            import::sha256_hex(&source),
            serde_json::to_string(&settings).unwrap_or_default(),
        ]
        .join("\n")
        .as_bytes(),
    );
    let import_id = format!("imp-{}-s{IMPORT_SCHEMA_VERSION}", &digest[..16]);
    let at = import::rfc3339(now);
    let summary = ReportSummary {
        profiles: settings.profiles.len(),
        size_limits: settings.size_limits.len(),
        indexers_usable: settings.indexers.len(),
        download_clients: settings.download_clients.len(),
        root_folders: settings.root_folders.len(),
        ..Default::default()
    };
    let report = MigrationReport {
        schema_version: IMPORT_SCHEMA_VERSION,
        import_id: import_id.clone(),
        snapshot_sha256: import::sha256_hex(&source),
        snapshot_bytes: source.len() as u64,
        imported_at: at.clone(),
        observed_at: at.clone(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        sources: vec![SourceSummary {
            app: "setup".into(),
            sections_present: [
                "movies",
                "tv",
                "profiles",
                "size_limits",
                "download",
                "indexers",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            sections_missing: vec![],
            sections_ignored: vec![],
        }],
        summary,
        blocking_issues: vec![],
        items,
        credentials: vec![],
        catalog_notes: vec![
            "managed setup: the catalog starts empty; titles are added with POST /api/v1/library"
                .into(),
        ],
        catalog_conflicts: vec![],
        catalog_skipped: vec![],
        cutover_blockers: vec![],
    };
    let catalog = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        import_id,
        observed_at: at,
        movies: vec![],
        series: vec![],
    };
    Ok(Prepared {
        normalized: Normalized {
            settings,
            catalog,
            report,
        },
        source,
        warnings,
    })
}

/// An active download, as replacement checks see it.
pub struct ActiveGrab {
    pub media: MediaKind,
    pub indexer_id: Option<u32>,
    pub category: String,
}

/// Active downloads in the state database.
pub fn active_grabs(c: &rusqlite::Connection) -> anyhow::Result<Vec<ActiveGrab>> {
    let mut stmt =
        c.prepare("SELECT media_id, indexer_id, category FROM grabs WHERE active = 1")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(m, i, category)| {
            let id: crate::ids::MediaId = m.parse().ok()?;
            Some(ActiveGrab {
                media: crate::downloads::media_kind(id),
                indexer_id: i.and_then(|i| u32::try_from(i).ok()),
                category,
            })
        })
        .collect())
}

/// Why `new` may not replace the active settings: a native snapshot import is never
/// replaced; every profile and root folder the managed catalog uses must remain; and while
/// downloads are active, their indexers and download categories must remain too (a grab's
/// seed goal, category and cleanup policy are recorded with it, so nothing in flight is
/// reinterpreted).
pub fn replace_conflicts(
    active_origin: Option<&str>,
    new: &Settings,
    index: &CatalogIndex,
    grabs: &[ActiveGrab],
) -> Vec<String> {
    let mut out = vec![];
    if active_origin == Some("native_snapshot") {
        out.push("a native snapshot import is active: managed setup never replaces imported settings (use a fresh state directory for a standalone deployment)".into());
        return out;
    }
    for g in grabs {
        if let Some(id) = g.indexer_id
            && !new
                .indexers
                .iter()
                .any(|i| i.prowlarr_indexer_id == id && i.media == g.media)
        {
            let msg = format!(
                "indexer {id} has active {} downloads and must remain until they finish",
                g.media.native_app()
            );
            if !out.contains(&msg) {
                out.push(msg);
            }
        }
        let cat = new
            .download_clients
            .iter()
            .find(|d| d.media == g.media)
            .and_then(|d| d.category.as_deref());
        if active_origin.is_some() && cat.is_some_and(|c| c != g.category) {
            let msg = format!(
                "the {} download category {} has active downloads and must not change until they finish",
                g.media.native_app(),
                g.category
            );
            if !out.contains(&msg) {
                out.push(msg);
            }
        }
    }
    let mut used_profiles: BTreeMap<String, MediaKind> = BTreeMap::new();
    let mut used_roots: BTreeSet<(MediaKind, String)> = BTreeSet::new();
    for m in &index.catalog.movies {
        if let Some(p) = &m.quality_profile {
            used_profiles.insert(p.clone(), MediaKind::Movie);
        }
        if let Some(r) = &m.root_folder {
            used_roots.insert((MediaKind::Movie, r.trim_end_matches('/').to_string()));
        }
    }
    for s in &index.catalog.series {
        if let Some(p) = &s.quality_profile {
            used_profiles.insert(p.clone(), MediaKind::Series);
        }
        if let Some(r) = &s.root_folder {
            used_roots.insert((MediaKind::Series, r.trim_end_matches('/').to_string()));
        }
    }
    for (p, m) in used_profiles {
        if !new.profiles.iter().any(|x| x.id == p && x.media == m) {
            out.push(format!(
                "profile {p} is used by {} titles in the catalog and must remain",
                m.native_app()
            ));
        }
    }
    for (m, r) in used_roots {
        if !new
            .root_folders
            .iter()
            .any(|x| x.media == m && x.path.trim_end_matches('/') == r)
        {
            out.push(format!(
                "root folder {r} is used by {} titles in the catalog and must remain",
                m.native_app()
            ));
        }
    }
    out
}

/// Parses a setup document (TOML) from a file the operator wrote.
pub fn read_doc(path: &Path) -> anyhow::Result<SetupDoc> {
    let bytes = crate::fsutil::read_bounded(path, 256 * 1024)?;
    let text =
        String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("setup document is not UTF-8"))?;
    toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("invalid setup document {}: {e}", path.display()))
}

/// Read-only Plex mapping check: each configured section exists, has the right type, and
/// one of its folders covers the library root as Plex sees it.
pub fn plex_check(plex: &PlexConfig, cfg: &RuntimeConfig, sections: &[Section]) -> Value {
    let lib = cfg.library.as_ref();
    let rows: Vec<Value> = [
        (MediaKind::Movie, plex.movie_section, plex.movies_path.as_deref(), lib.and_then(|l| l.movies.as_ref()), "movie"),
        (MediaKind::Series, plex.tv_section, plex.tv_path.as_deref(), lib.and_then(|l| l.tv.as_ref()), "show"),
    ]
    .into_iter()
    .map(|(m, section, plex_path, local, kind)| {
        let candidates: Vec<Value> = sections
            .iter()
            .filter(|s| s.kind == kind)
            .map(|s| json!({ "section": s.key, "title": s.title, "locations": s.locations }))
            .collect();
        let Some(id) = section else {
            return json!({ "media": m, "status": "not_configured", "local_root": local, "candidates": candidates,
                "hint": format!("set [plex] {} to one of the candidate section ids", if m == MediaKind::Movie { "movie_section" } else { "tv_section" }) });
        };
        let Some(s) = sections.iter().find(|s| s.key == id) else {
            return json!({ "media": m, "section": id, "status": "section_not_found", "candidates": candidates });
        };
        if s.kind != kind {
            return json!({ "media": m, "section": id, "title": s.title, "status": "wrong_type", "type": s.kind, "expected": kind, "candidates": candidates });
        }
        let covered = plex_path.and_then(|p| {
            s.locations
                .iter()
                .find(|l| {
                    let l = l.trim_end_matches('/');
                    p == l || p.starts_with(&format!("{l}/"))
                })
                .cloned()
        });
        let (status, hint) = match (plex_path, &covered) {
            (None, _) => ("path_unmapped", Some(format!("set [plex] {} to the {} root as Plex sees it (one of {:?}); refreshes then target the imported folder", if m == MediaKind::Movie { "movies_path" } else { "tv_path" }, m.native_app(), s.locations))),
            (Some(_), None) => ("path_not_in_section", Some(format!("the configured Plex path is not inside the section's folders {:?}", s.locations))),
            (Some(_), Some(_)) => ("ok", None),
        };
        json!({ "media": m, "section": id, "title": s.title, "status": status, "plex_path": plex_path, "covered_by": covered, "local_root": local, "hint": hint })
    })
    .collect();
    json!({ "sections": sections.iter().map(|s| json!({ "section": s.key, "title": s.title, "type": s.kind, "locations": s.locations })).collect::<Vec<_>>(), "mapping": rows })
}

/// Where a setup import for `state_dir` goes and the preflight a CLI run reports.
pub fn summary(prepared: &Prepared, status: &str, dir: Option<PathBuf>) -> Value {
    let s = &prepared.normalized.settings;
    json!({
        "status": status,
        "import_id": prepared.normalized.report.import_id,
        "import_dir": dir,
        "profiles": s.profiles.iter().map(|p| json!({ "id": p.id, "media": p.media, "name": p.name })).collect::<Vec<_>>(),
        "root_folders": s.root_folders,
        "indexers": s.indexers.iter().map(|i| json!({ "id": i.prowlarr_indexer_id, "media": i.media, "name": i.name, "categories": i.categories, "rss": i.enable_rss, "seed_ratio": i.seed_ratio, "seed_time_minutes": i.seed_time_minutes })).collect::<Vec<_>>(),
        "download": s.download_clients.iter().map(|d| json!({ "media": d.media, "category": d.category, "remove_completed": d.remove_completed_downloads, "remove_failed": d.remove_failed_downloads })).collect::<Vec<_>>(),
        "warnings": prepared.warnings,
        "execution": "unchanged: setup never enables execution, RSS or backlog searches",
    })
}

#[cfg(all(test, unix))] // fixtures use Unix absolute paths
mod tests {
    use super::*;
    use crate::fsutil;

    fn cfg(extra: &str) -> (tempfile::TempDir, RuntimeConfig) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        fsutil::ensure_private_dir(&dir).unwrap();
        fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
        fsutil::write_new_private_file(
            &dir.join("secrets/api_token"),
            b"0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let text = format!(
            "[server]\napi_token_file = \"secrets/api_token\"\n[[indexer]]\nid = 1\nname = \"The Pirate Bay\"\nkind = \"thepiratebay\"\nbase_url = \"https://apibay.example/\"\n[library]\nmovies = \"/srv/movies\"\ntv = \"/srv/tv\"\n{extra}"
        );
        let (c, _) =
            crate::config::parse_config(&text, &dir, "test", crate::config::Secrets::Read).unwrap();
        (tmp, c)
    }

    #[test]
    fn defaults_are_complete_explicit_and_stable() {
        let (_t, c) = cfg("");
        let (s, items, warnings) = normalize(&SetupDoc::default(), &c).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            s.profiles.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["setup:movies-hd", "setup:tv-hd"]
        );
        assert_eq!(
            s.root_folders
                .iter()
                .map(|r| r.path.as_str())
                .collect::<Vec<_>>(),
            ["/srv/movies", "/srv/tv"]
        );
        assert_eq!(
            s.indexers.len(),
            2,
            "the configured indexer, for movies and TV"
        );
        let ix = &s.indexers[0];
        assert_eq!(
            (ix.prowlarr_indexer_id, ix.seed_ratio, ix.seed_time_minutes),
            (1, Some(1.0), Some(4320))
        );
        assert_eq!(s.indexers[1].categories, [5000]);
        assert!(
            s.release_policies
                .iter()
                .all(|p| !p.revision_upgrades && p.redownload_failed)
        );
        assert!(
            s.download_clients
                .iter()
                .all(|d| d.implementation == MANAGED_CLIENT
                    && d.remove_completed_downloads == Some(true))
        );
        // Every allowed quality has a size limit (decisions never meet size_limits_missing).
        for p in &s.profiles {
            for q in p.tiers.iter().flat_map(|t| &t.qualities) {
                assert!(s.size_limit(p.media, *q).is_some(), "{q:?}");
            }
        }
        assert!(!items.is_empty());
        // The same document gives the same import id; a different one, another.
        let now = time::OffsetDateTime::now_utc();
        let a = prepare(&SetupDoc::default(), &c, now).unwrap();
        let b = prepare(&SetupDoc::default(), &c, now + time::Duration::hours(1)).unwrap();
        assert_eq!(a.normalized.report.import_id, b.normalized.report.import_id);
        let doc: SetupDoc = toml::from_str("[tv]\nrss_sync_interval_minutes = 30\n").unwrap();
        assert_ne!(
            prepare(&doc, &c, now).unwrap().normalized.report.import_id,
            a.normalized.report.import_id
        );
        // The documented example is a valid document.
        let example: SetupDoc =
            toml::from_str(include_str!("../config/setup.example.toml")).unwrap();
        normalize(&example, &c).unwrap();
    }

    #[test]
    fn invalid_documents_report_every_problem() {
        let (_t, c) = cfg("");
        let doc: SetupDoc = toml::from_str(
            "[movies]\nrss_sync_interval_minutes = 5\n[[profiles]]\nid = \"bad id\"\nmedia = \"movie\"\ntiers = [[\"webdl_1080p\"]]\n[[indexers]]\nid = 7\n[[indexers]]\nid = 1\nseed_ratio = 5000.0\n",
        )
        .unwrap();
        let errs = normalize(&doc, &c).unwrap_err();
        assert!(errs.len() >= 4, "{errs:?}");
        assert!(
            errs.iter()
                .any(|e| e.contains("indexer 7 is not a configured")),
            "{errs:?}"
        );
        assert!(
            toml::from_str::<SetupDoc>("[movies]\napi_key = \"x\"\n").is_err(),
            "no stray keys (credentials have no place here)"
        );
    }

    #[test]
    fn replacements_keep_what_the_catalog_and_downloads_use() {
        let (_t, c) = cfg("");
        let (old, _, _) = normalize(&SetupDoc::default(), &c).unwrap();
        let empty = CatalogIndex::new(Catalog {
            schema_version: 1,
            import_id: String::new(),
            observed_at: String::new(),
            movies: vec![],
            series: vec![],
        });
        assert!(replace_conflicts(Some("managed_setup"), &old, &empty, &[]).is_empty());
        assert!(!replace_conflicts(Some("native_snapshot"), &old, &empty, &[]).is_empty());
        let grabs = [ActiveGrab {
            media: MediaKind::Movie,
            indexer_id: Some(1),
            category: "um-movies".into(),
        }];
        assert!(replace_conflicts(Some("managed_setup"), &old, &empty, &grabs).is_empty());
        let moved: SetupDoc =
            toml::from_str("[download]\nmovie_category = \"elsewhere\"\n").unwrap();
        let (new, _, _) = normalize(&moved, &c).unwrap();
        let why = replace_conflicts(Some("managed_setup"), &new, &empty, &grabs);
        assert!(why.iter().any(|w| w.contains("um-movies")), "{why:?}");
        let dropped: SetupDoc = toml::from_str("[[indexers]]\nid = 1\nmovies = false\n").unwrap();
        let (new, _, _) = normalize(&dropped, &c).unwrap();
        let why = replace_conflicts(Some("managed_setup"), &new, &empty, &grabs);
        assert!(
            why.iter()
                .any(|w| w.contains("indexer 1 has active radarr downloads")),
            "{why:?}"
        );
    }
}
