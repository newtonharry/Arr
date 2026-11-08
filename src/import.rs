//! Offline importer: native Radarr/Sonarr/Prowlarr snapshot -> private normalised state.
//!
//! No network access. Values are copied from an allowlist of non-secret fields only; credential
//! fields are recorded by name. The original snapshot is preserved byte-for-byte in the
//! private import directory.

use crate::catalog::{
    AltTitle, CATALOG_SCHEMA_VERSION, Catalog, Episode, FileEvidence, Movie, Provenance,
    SeasonInfo, Series,
};
use crate::fsutil;
use crate::ids::{MediaId, SeriesKey};
use crate::quality::Quality;
use crate::settings::*;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const IMPORT_SCHEMA_VERSION: u32 = 1;
pub const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_IMPORT_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const ACTIVE_POINTER: &str = "active-import.json";

// ---------------------------------------------------------------- native shapes

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
struct NQuality {
    id: i64,
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NProfileItem {
    id: Option<i64>,
    name: Option<String>,
    quality: Option<NQuality>,
    items: Vec<NProfileItem>,
    allowed: bool,
    min_size: Option<f64>,
    max_size: Option<f64>,
    preferred_size: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NFormatItem {
    name: Option<String>,
    score: i64,
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase", default)]
struct NLanguage {
    id: i64,
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NQualityProfile {
    id: i64,
    name: String,
    upgrade_allowed: bool,
    cutoff: i64,
    items: Vec<NProfileItem>,
    min_format_score: i64,
    cutoff_format_score: i64,
    min_upgrade_format_score: i64,
    format_items: Vec<NFormatItem>,
    language: Option<NLanguage>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NQualityDefinition {
    quality: NQuality,
    min_size: Option<f64>,
    max_size: Option<f64>,
    preferred_size: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NField {
    name: String,
    value: Option<Value>,
    privacy: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NIndexer {
    id: i64,
    name: String,
    protocol: String,
    priority: i64,
    enable_rss: bool,
    enable_automatic_search: bool,
    enable_interactive_search: bool,
    implementation: String,
    fields: Vec<NField>,
    tags: Vec<i64>,
    download_client_id: i64,
    season_search_maximum_single_episode_age: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NDownloadClient {
    id: i64,
    name: String,
    enable: bool,
    protocol: String,
    priority: i64,
    implementation: String,
    fields: Vec<NField>,
    tags: Vec<i64>,
    remove_completed_downloads: Option<bool>,
    remove_failed_downloads: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NDelayProfile {
    id: i64,
    enable_usenet: bool,
    enable_torrent: bool,
    preferred_protocol: String,
    usenet_delay: i64,
    torrent_delay: i64,
    bypass_if_highest_quality: bool,
    bypass_if_above_custom_format_score: bool,
    minimum_custom_format_score: i64,
    tags: Vec<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NRootFolder {
    path: String,
    accessible: Option<bool>,
    free_space: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NProwlarrIndexer {
    id: i64,
    name: String,
    enable: bool,
    protocol: String,
    privacy: Option<String>,
    priority: i64,
    definition_name: Option<String>,
    implementation: Option<String>,
    tags: Vec<i64>,
    fields: Vec<NField>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NLanguageProfile {
    name: String,
    languages: Vec<NLanguageAllowed>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NLanguageAllowed {
    language: NLanguage,
    allowed: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NTitle {
    title: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NRevision {
    version: Option<i64>,
    real: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NQualityModel {
    quality: Option<NQuality>,
    revision: Option<NRevision>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NFile {
    quality: Option<NQualityModel>,
    relative_path: Option<String>,
    size: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NMovieStatistics {
    movie_file_count: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NMovie {
    id: i64,
    title: String,
    original_title: Option<String>,
    original_language: Option<NLanguage>,
    alternate_titles: Vec<NTitle>,
    year: Option<i64>,
    secondary_year: Option<i64>,
    runtime: Option<i64>,
    imdb_id: Option<String>,
    tmdb_id: i64,
    has_file: Option<bool>,
    monitored: bool,
    quality_profile_id: i64,
    root_folder_path: Option<String>,
    path: Option<String>,
    status: Option<String>,
    is_available: Option<bool>,
    minimum_availability: Option<String>,
    in_cinemas: Option<String>,
    physical_release: Option<String>,
    digital_release: Option<String>,
    movie_file: Option<NFile>,
    statistics: Option<NMovieStatistics>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NSeriesAltTitle {
    title: String,
    season_number: Option<i64>,
    scene_season_number: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NSeasonStats {
    episode_file_count: Option<i64>,
    episode_count: Option<i64>,
    total_episode_count: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NSeason {
    season_number: i64,
    monitored: bool,
    statistics: Option<NSeasonStats>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NSeries {
    id: i64,
    title: String,
    alternate_titles: Vec<NSeriesAltTitle>,
    year: Option<i64>,
    tvdb_id: i64,
    tv_maze_id: Option<i64>,
    tmdb_id: Option<i64>,
    imdb_id: Option<String>,
    series_type: Option<String>,
    use_scene_numbering: bool,
    runtime: Option<i64>,
    original_language: Option<NLanguage>,
    status: Option<String>,
    monitored: bool,
    quality_profile_id: i64,
    root_folder_path: Option<String>,
    path: Option<String>,
    seasons: Vec<NSeason>,
    monitor_new_items: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct NEpisode {
    season_number: i64,
    episode_number: i64,
    title: Option<String>,
    air_date_utc: Option<String>,
    runtime: Option<i64>,
    has_file: bool,
    monitored: bool,
    episode_file: Option<NFile>,
    scene_season_number: Option<i64>,
    scene_episode_number: Option<i64>,
}

// ---------------------------------------------------------------- report

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// Honoured with native semantics.
    Enforced,
    /// Honoured more strictly than native (never approves something native would reject).
    EnforcedStricter,
    /// Recorded for reference; does not influence decisions or execution.
    ObservedOnly,
    /// Has no effect given the rest of the configuration (e.g. usenet settings, torrent-only).
    NotApplicable,
    /// Cannot be honoured; plans in scope are blocked.
    UnsupportedBlocksPlans,
    /// Cannot be honoured by completed-download import; file import for that media kind is
    /// blocked (searches and plans continue).
    UnsupportedBlocksImport,
    /// Behaviour this build does not have (e.g. quality upgrades, tag routing); cutover blocker.
    NotImplemented,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportItem {
    pub app: String,
    pub area: String,
    pub setting: String,
    pub value: Value,
    pub status: ItemStatus,
    pub effect: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialNote {
    pub app: String,
    pub section: String,
    pub native_id: i64,
    pub name: String,
    /// `field: present|masked_or_empty`; values are never copied.
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSummary {
    pub app: String,
    pub sections_present: Vec<String>,
    pub sections_missing: Vec<String>,
    pub sections_ignored: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReportSummary {
    pub profiles: usize,
    pub size_limits: usize,
    pub indexers_usable: usize,
    pub indexers_unusable: usize,
    pub download_clients: usize,
    pub root_folders: usize,
    pub movies: usize,
    pub series: usize,
    pub series_with_episode_samples: usize,
    pub catalog_skipped: usize,
    pub catalog_identity_conflicts: usize,
    pub blocking_issues: usize,
    pub unsupported_items: usize,
    pub not_implemented_items: usize,
    pub credential_entries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationReport {
    pub schema_version: u32,
    pub import_id: String,
    pub snapshot_sha256: String,
    pub snapshot_bytes: u64,
    pub imported_at: String,
    pub observed_at: String,
    pub tool_version: String,
    pub sources: Vec<SourceSummary>,
    pub summary: ReportSummary,
    pub blocking_issues: Vec<BlockingIssue>,
    pub items: Vec<ReportItem>,
    pub credentials: Vec<CredentialNote>,
    pub catalog_notes: Vec<String>,
    pub catalog_conflicts: Vec<String>,
    pub catalog_skipped: Vec<String>,
    pub cutover_blockers: Vec<String>,
}

/// Statements about replacing the native apps that hold for every import. Whether a stage is
/// ready is computed live by `GET /api/v1/readiness` (configuration, acquisition gates,
/// ownership and operator-recorded evidence), not by this list.
pub const CUTOVER_BLOCKERS: &[&str] = &[
    "replacement needs every migration stage in GET /api/v1/readiness to hold (configuration, operator-recorded production evidence for the capabilities in use, the native handoff outside this server) and an explicit user decision",
    "in coexistence mode imported titles are acquired only after adoption through POST /api/v1/library; legacy mode acquires every imported item with its native monitoring",
    "quality upgrades beyond native revision upgrades (cutoff upgrades), renaming, custom formats, tag-scoped routing, usenet and anime/daily/scene numbering are not supported and fail closed",
    "Torznab RSS covers only the latest releases per feed window, and Prowlarr does not report provider completeness (accepted limitations shared with the native apps)",
];

// ---------------------------------------------------------------- normalisation

pub struct Normalized {
    pub settings: Settings,
    pub catalog: Catalog,
    pub report: MigrationReport,
}

struct Ctx {
    import_id: String,
    observed_at: String,
    items: Vec<ReportItem>,
    credentials: Vec<CredentialNote>,
    blocking: Vec<BlockingIssue>,
}

impl Ctx {
    fn item(
        &mut self,
        app: &str,
        area: &str,
        setting: impl Into<String>,
        value: Value,
        status: ItemStatus,
        effect: impl Into<String>,
    ) {
        self.items.push(ReportItem {
            app: app.into(),
            area: area.into(),
            setting: setting.into(),
            value,
            status,
            effect: effect.into(),
        });
    }

    fn block(&mut self, scope: IssueScope, code: &str, message: impl Into<String>) {
        let message = message.into();
        if !self
            .blocking
            .iter()
            .any(|b| b.scope == scope && b.code == code && b.message == message)
        {
            self.blocking.push(BlockingIssue {
                scope,
                code: code.into(),
                message,
            });
        }
    }
}

const SETTINGS_SECTIONS: &[&str] = &[
    "indexer",
    "downloadclient",
    "qualityprofile",
    "qualitydefinition",
    "rootfolder",
    "delayprofile",
    "customformat",
    "tag",
    "remotepathmapping",
    "releaseprofile",
    "config/downloadclient",
    "config/indexer",
    "config/mediamanagement",
    "config/naming",
];
const OBSERVATION_SECTIONS: &[&str] = &["health", "system/status", "applications"];

fn section_list<T: for<'de> Deserialize<'de>>(
    app: &Map<String, Value>,
    app_name: &str,
    key: &str,
    required: bool,
) -> Result<Vec<T>> {
    match app.get(key) {
        None | Some(Value::Null) if !required => Ok(vec![]),
        None | Some(Value::Null) => bail!("{app_name}.{key} is required"),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                T::deserialize(v)
                    .with_context(|| format!("{app_name}.{key}[{i}] has an unexpected shape"))
            })
            .collect(),
        Some(_) => bail!("{app_name}.{key} must be an array"),
    }
}

fn section_object<'a>(
    app: &'a Map<String, Value>,
    app_name: &str,
    key: &str,
) -> Result<Option<&'a Map<String, Value>>> {
    match app.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => Ok(Some(m)),
        Some(_) => bail!("{app_name}.{key} must be an object"),
    }
}

fn is_secret_field(f: &NField) -> bool {
    let privacy = f.privacy.as_deref().unwrap_or("normal");
    if !privacy.eq_ignore_ascii_case("normal") {
        return true;
    }
    if f.kind
        .as_deref()
        .is_some_and(|k| k.eq_ignore_ascii_case("password"))
    {
        return true;
    }
    is_secret_name(&f.name)
}

fn is_secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "apikey", "password", "passkey", "username", "token", "secret", "cookie", "rsskey",
        "authkey",
    ]
    .iter()
    .any(|s| n.contains(s))
}

fn secret_state(v: &Option<Value>) -> &'static str {
    match v {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() || t.eq_ignore_ascii_case("redacted") || t.chars().all(|c| c == '*') {
                "masked_or_empty"
            } else {
                "present"
            }
        }
        None | Some(Value::Null) => "masked_or_empty",
        Some(_) => "present",
    }
}

fn note_credentials(
    ctx: &mut Ctx,
    app: &str,
    section: &str,
    id: i64,
    name: &str,
    fields: &[NField],
) -> Vec<String> {
    let secret: BTreeMap<String, String> = fields
        .iter()
        .filter(|f| is_secret_field(f))
        .map(|f| (f.name.clone(), secret_state(&f.value).to_string()))
        .collect();
    let names = secret.keys().cloned().collect();
    if !secret.is_empty() {
        ctx.credentials.push(CredentialNote {
            app: app.into(),
            section: section.into(),
            native_id: id,
            name: name.into(),
            fields: secret,
        });
    }
    names
}

/// Non-secret field value; `None` for secrets or absent values.
fn field<'a>(fields: &'a [NField], name: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find(|f| f.name == name && !is_secret_field(f))
        .and_then(|f| f.value.as_ref())
        .filter(|v| !v.is_null())
}

fn field_str(fields: &[NField], name: &str) -> Option<String> {
    field(fields, name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn field_i64(fields: &[NField], name: &str) -> Option<i64> {
    field(fields, name).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

fn field_f64(fields: &[NField], name: &str) -> Option<f64> {
    field(fields, name).and_then(Value::as_f64)
}

fn field_bool(fields: &[NField], name: &str) -> Option<bool> {
    field(fields, name).and_then(Value::as_bool)
}

fn field_ints(fields: &[NField], name: &str) -> Vec<i64> {
    field(fields, name)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// Strips userinfo, query and fragment so URLs in reports never carry credentials.
pub fn sanitize_url(raw: &str) -> String {
    match reqwest::Url::parse(raw) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => "<unparseable url>".into(),
    }
}

fn normalize_language(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    match lower.find(" (") {
        Some(i) => lower[..i].to_string(),
        None => lower,
    }
}

fn year(v: Option<i64>) -> Option<u16> {
    v.filter(|y| (1800..=2200).contains(y)).map(|y| y as u16)
}

fn positive_u32(v: Option<i64>) -> Option<u32> {
    v.filter(|&n| n > 0 && n <= i64::from(u32::MAX))
        .map(|n| n as u32)
}

fn file_quality(f: &Option<NFile>) -> Option<Quality> {
    f.as_ref()
        .and_then(|f| f.quality.as_ref())
        .and_then(|q| q.quality.as_ref())
        .and_then(|q| Quality::from_native_name(&q.name))
}

/// A safe relative path: non-empty `/`-separated components, no `.`/`..`, NUL or backslash.
pub fn safe_relative(p: &str) -> Option<String> {
    let p = p.trim_matches('/');
    let ok = !p.is_empty()
        && !p.contains(['\0', '\\'])
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..");
    ok.then(|| p.to_string())
}

/// Folder of a native item relative to its root (`/movies/The Matrix (1999)` -> `The Matrix (1999)`).
fn relative_folder(path: Option<&str>, root: Option<&str>) -> Option<String> {
    let root = root?.trim_end_matches('/');
    let rest = path?.strip_prefix(root)?.strip_prefix('/')?;
    safe_relative(rest)
}

/// Snapshot evidence about an owned file. The revision is kept only when the native
/// export carries it; it is never guessed.
fn file_evidence(f: &Option<NFile>, folder: Option<&str>, observed_at: &str) -> FileEvidence {
    let f = f.as_ref();
    let model = f.and_then(|f| f.quality.as_ref());
    let revision = model.and_then(|m| m.revision.as_ref()).and_then(|r| {
        let version = u8::try_from(r.version?).ok().filter(|v| *v >= 1)?;
        let real = u8::try_from(r.real.unwrap_or(0)).ok()?;
        Some(crate::release::Revision { version, real })
    });
    FileEvidence {
        path: folder
            .zip(
                f.and_then(|f| f.relative_path.as_deref())
                    .and_then(safe_relative),
            )
            .map(|(folder, rel)| format!("{folder}/{rel}")),
        size: f
            .and_then(|f| f.size)
            .and_then(|s| u64::try_from(s).ok())
            .filter(|s| *s > 0),
        quality: model
            .and_then(|m| m.quality.as_ref())
            .and_then(|q| Quality::from_native_name(&q.name)),
        revision,
        source: "snapshot".into(),
        observed_at: observed_at.into(),
        probe: None,
    }
}

fn root_for(path: Option<&str>, explicit: Option<&str>, roots: &[String]) -> Option<String> {
    if let Some(r) = explicit
        .map(|r| r.trim_end_matches('/'))
        .filter(|r| !r.is_empty())
    {
        return Some(r.to_string());
    }
    let path = path?;
    roots
        .iter()
        .filter(|r| path.starts_with(&format!("{}/", r.trim_end_matches('/'))))
        .max_by_key(|r| r.len())
        .map(|r| r.trim_end_matches('/').to_string())
}

fn profiles(
    ctx: &mut Ctx,
    app: &str,
    media: MediaKind,
    list: Vec<NQualityProfile>,
    defs: &[NQualityDefinition],
    language: &LanguageRule,
) -> Vec<Profile> {
    let mut out = vec![];
    for p in list {
        let id = format!("{app}:{}", p.id);
        let scope = IssueScope::Profile {
            profile: id.clone(),
        };
        let area = format!("qualityprofile[{}] {}", p.id, p.name);
        let mut tiers: Vec<Tier> = vec![];
        let mut cutoff_tier = None;
        let mut unknown_allowed = vec![];
        let mut unknown_disallowed = vec![];
        let mut size_overrides = false;
        for item in &p.items {
            let members: Vec<&NProfileItem> = if item.items.is_empty() {
                vec![item]
            } else {
                item.items.iter().collect()
            };
            let tier_name = item
                .name
                .clone()
                .filter(|n| !n.is_empty())
                .or_else(|| item.quality.as_ref().map(|q| q.name.clone()))
                .unwrap_or_else(|| "unnamed".into());
            let is_cutoff = (!item.items.is_empty() && item.id == Some(p.cutoff))
                || (item.items.is_empty()
                    && item.quality.as_ref().is_some_and(|q| q.id == p.cutoff));
            let mut qualities = vec![];
            for m in members {
                size_overrides |=
                    m.min_size.is_some() || m.max_size.is_some() || m.preferred_size.is_some();
                let allowed = m.allowed && item.allowed;
                let Some(q) = m.quality.as_ref() else {
                    continue;
                };
                match Quality::from_native_name(&q.name) {
                    Some(quality) if allowed => qualities.push(quality),
                    Some(_) => {}
                    None if allowed => unknown_allowed.push(q.name.clone()),
                    None => unknown_disallowed.push(q.name.clone()),
                }
            }
            if is_cutoff {
                cutoff_tier = Some(tier_name.clone());
            }
            if !qualities.is_empty() {
                tiers.push(Tier {
                    name: tier_name,
                    qualities,
                });
            }
        }
        let allowed: Vec<Value> = tiers
            .iter()
            .map(|t| json!({ "tier": t.name, "qualities": t.qualities }))
            .collect();
        ctx.item(
            app,
            &area,
            "allowed quality tiers (ascending preference)",
            Value::Array(allowed),
            ItemStatus::Enforced,
            "releases outside these tiers are rejected; higher tiers rank first",
        );
        if tiers.is_empty() {
            ctx.item(
                app,
                &area,
                "allowed qualities",
                json!([]),
                ItemStatus::Enforced,
                "profile allows no quality: every release is rejected",
            );
        }
        for name in &unknown_allowed {
            ctx.block(
                scope.clone(),
                "unknown_allowed_quality",
                format!("profile {id} allows quality '{name}', which this build cannot recognise"),
            );
        }
        if !unknown_allowed.is_empty() {
            ctx.item(
                app,
                &area,
                "unrecognised allowed qualities",
                json!(unknown_allowed),
                ItemStatus::UnsupportedBlocksPlans,
                "plans using this profile are blocked",
            );
        }
        if !unknown_disallowed.is_empty() {
            ctx.item(
                app,
                &area,
                "unrecognised disallowed qualities",
                json!(unknown_disallowed),
                ItemStatus::NotApplicable,
                "the parser never produces them, so they are never approved",
            );
        }
        if size_overrides {
            ctx.block(
                scope.clone(),
                "profile_size_overrides",
                format!("profile {id} carries per-quality size overrides, which are not supported"),
            );
            ctx.item(
                app,
                &area,
                "per-item size overrides",
                json!(true),
                ItemStatus::UnsupportedBlocksPlans,
                "plans using this profile are blocked",
            );
        }
        if p.upgrade_allowed {
            ctx.item(app, &area, "upgradeAllowed", json!(true), ItemStatus::NotImplemented, "quality/cutoff upgrades are not implemented: owned items are re-planned only for an exact-quality higher revision (downloadPropersAndRepacks preferAndUpgrade); errs towards under-grabbing");
        } else {
            ctx.item(
                app,
                &area,
                "upgradeAllowed",
                json!(false),
                ItemStatus::Enforced,
                "a release overlapping an owned movie/episode is rejected",
            );
        }
        ctx.item(
            app,
            &area,
            "cutoff",
            json!({ "native_id": p.cutoff, "tier": cutoff_tier }),
            if p.upgrade_allowed {
                ItemStatus::NotImplemented
            } else {
                ItemStatus::NotApplicable
            },
            "cutoff only governs upgrades",
        );
        let scored: Vec<&NFormatItem> = p.format_items.iter().filter(|f| f.score != 0).collect();
        if p.min_format_score != 0 || !scored.is_empty() {
            ctx.block(scope.clone(), "custom_format_scoring", format!("profile {id} scores custom formats (minFormatScore {}, {} scored formats); custom formats are not supported", p.min_format_score, scored.len()));
            ctx.item(app, &area, "custom format scoring", json!({ "minFormatScore": p.min_format_score, "scoredFormats": scored.iter().map(|f| f.name.clone()).collect::<Vec<_>>() }), ItemStatus::UnsupportedBlocksPlans, "plans using this profile are blocked");
        } else {
            ctx.item(app, &area, "custom format scoring", json!({ "minFormatScore": p.min_format_score, "cutoffFormatScore": p.cutoff_format_score, "minUpgradeFormatScore": p.min_upgrade_format_score, "scoredFormats": 0 }), ItemStatus::NotApplicable, "no formats are scored, so scores never change a decision");
        }
        let rule = match (media, &p.language) {
            (MediaKind::Movie, None) => LanguageRule {
                requirement: LanguageRequirement::Any,
                unmarked: UnmarkedLanguage::Accept,
                source: "radarr profile has no language".into(),
            },
            (MediaKind::Movie, Some(l)) => match l.id {
                -1 => LanguageRule {
                    requirement: LanguageRequirement::Any,
                    unmarked: UnmarkedLanguage::Accept,
                    source: "radarr profile language Any".into(),
                },
                -2 => LanguageRule {
                    requirement: LanguageRequirement::Original,
                    unmarked: UnmarkedLanguage::AssumeOriginal,
                    source: "radarr profile language Original".into(),
                },
                0 => {
                    ctx.block(
                        scope.clone(),
                        "unsupported_language_rule",
                        format!("profile {id} requires language 'Unknown'"),
                    );
                    LanguageRule {
                        requirement: LanguageRequirement::OneOf {
                            languages: vec!["unknown".into()],
                        },
                        unmarked: UnmarkedLanguage::AssumeOriginal,
                        source: "radarr profile language Unknown (unsupported)".into(),
                    }
                }
                _ => LanguageRule {
                    requirement: LanguageRequirement::OneOf {
                        languages: vec![normalize_language(&l.name)],
                    },
                    unmarked: UnmarkedLanguage::AssumeOriginal,
                    source: format!("radarr profile language {}", l.name),
                },
            },
            (MediaKind::Series, _) => language.clone(),
        };
        ctx.item(app, &area, "language", serde_json::to_value(&rule).unwrap_or(Value::Null), ItemStatus::Enforced, match media {
            MediaKind::Movie => "Radarr semantics: unmarked releases are assumed to be in the original language",
            MediaKind::Series => "Sonarr v4 applies language rules only through custom formats; none are scored here, so no language filter applies (native parity)",
        });
        for t in &tiers {
            for q in &t.qualities {
                if !defs
                    .iter()
                    .any(|d| Quality::from_native_name(&d.quality.name) == Some(*q))
                {
                    ctx.block(
                        scope.clone(),
                        "missing_size_definition",
                        format!("profile {id} allows {q} but no quality definition exists for it"),
                    );
                }
            }
        }
        out.push(Profile {
            id,
            media,
            name: p.name,
            upgrade_allowed: p.upgrade_allowed,
            tiers,
            cutoff_tier,
            language: rule,
        });
    }
    out
}

fn size_limits(
    ctx: &mut Ctx,
    app: &str,
    media: MediaKind,
    defs: &[NQualityDefinition],
) -> Vec<SizeLimit> {
    let mut out = vec![];
    let mut table = vec![];
    for d in defs {
        let Some(quality) = Quality::from_native_name(&d.quality.name) else {
            ctx.item(
                app,
                "qualitydefinition",
                d.quality.name.clone(),
                json!({ "minSize": d.min_size, "maxSize": d.max_size }),
                ItemStatus::NotApplicable,
                "unrecognised quality; never produced by the parser",
            );
            continue;
        };
        let min = d.min_size.unwrap_or(0.0).max(0.0);
        let max = d.max_size.filter(|m| *m > 0.0);
        table.push(json!({ "quality": quality, "min_mib_per_minute": min, "max_mib_per_minute": max, "preferred_mib_per_minute": d.preferred_size }));
        out.push(SizeLimit {
            media,
            quality,
            min_mib_per_minute: min,
            max_mib_per_minute: max,
            preferred_mib_per_minute: d.preferred_size.filter(|p| *p > 0.0),
        });
    }
    ctx.item(app, "qualitydefinition", "size limits (MiB per minute of runtime)", Value::Array(table), ItemStatus::Enforced, "size must fall within min/max x runtime (x episodes for TV); unknown runtime with a limit rejects the release; preferred size is a ranking tie-breaker");
    out
}

/// `http://prowlarr:9696/1/` -> 1 (Prowlarr's per-indexer Torznab/Newznab proxy URL).
fn prowlarr_proxy_id(base_url: &str) -> Option<i64> {
    let url = reqwest::Url::parse(base_url).ok()?;
    let last = url.path_segments()?.rfind(|s| !s.is_empty())?;
    last.parse().ok().filter(|&n: &i64| n > 0)
}

fn indexers(
    ctx: &mut Ctx,
    app: &str,
    media: MediaKind,
    list: Vec<NIndexer>,
    prowlarr: &[NProwlarrIndexer],
    tags_in_use: &mut bool,
) -> Vec<IndexerSetting> {
    let mut out = vec![];
    let (cat_lo, cat_hi) = match media {
        MediaKind::Movie => (2000, 2999),
        MediaKind::Series => (5000, 5999),
    };
    for ix in list {
        let area = format!("indexer[{}] {}", ix.id, ix.name);
        note_credentials(ctx, app, "indexer", ix.id, &ix.name, &ix.fields);
        let base = field_str(&ix.fields, "baseUrl");
        let target = base.as_deref().and_then(prowlarr_proxy_id);
        let px = target.and_then(|id| prowlarr.iter().find(|p| p.id == id));
        let unusable = if !matches!(ix.implementation.as_str(), "Torznab" | "Newznab") {
            Some(format!(
                "implementation {} is not a Prowlarr proxy",
                ix.implementation
            ))
        } else if !ix.protocol.eq_ignore_ascii_case("torrent") {
            Some(format!(
                "protocol {} is not supported (torrent only)",
                ix.protocol
            ))
        } else if px.is_none() {
            Some(
                "baseUrl does not reference an indexer present in the Prowlarr snapshot"
                    .to_string(),
            )
        } else if px.is_some_and(|p| !p.enable) {
            Some("the Prowlarr indexer is disabled".to_string())
        } else {
            None
        };
        let value = json!({
            "baseUrl": base.as_deref().map(sanitize_url),
            "prowlarr_indexer_id": target,
            "priority": ix.priority,
            "enableAutomaticSearch": ix.enable_automatic_search,
            "enableInteractiveSearch": ix.enable_interactive_search,
        });
        if let Some(reason) = unusable {
            ctx.item(
                app,
                &area,
                "mapping",
                value,
                ItemStatus::ObservedOnly,
                format!("not used for searches: {reason}"),
            );
            continue;
        }
        let px = px.expect("checked above");
        let all_categories = field_ints(&ix.fields, "categories");
        let (categories, dropped): (Vec<i64>, Vec<i64>) = all_categories
            .iter()
            .partition(|c| (cat_lo..=cat_hi).contains(*c));
        if !dropped.is_empty() {
            ctx.item(
                app,
                &area,
                "categories outside the media range",
                json!(dropped),
                ItemStatus::EnforcedStricter,
                format!("only {cat_lo}-{cat_hi} categories are searched for {media:?}"),
            );
        }
        let categories: Vec<u32> = if categories.is_empty() {
            ctx.item(
                app,
                &area,
                "categories",
                json!([]),
                ItemStatus::EnforcedStricter,
                format!("no categories configured; searching the base category {cat_lo}"),
            );
            vec![cat_lo as u32]
        } else {
            categories.iter().map(|&c| c as u32).collect()
        };
        let minimum_seeders = field_i64(&ix.fields, "minimumSeeders").unwrap_or(1).max(0) as u32;
        let mut unsupported = vec![];
        let flags = field_ints(&ix.fields, "requiredFlags");
        if !flags.is_empty() {
            unsupported.push("requiredFlags".to_string());
            ctx.item(
                app,
                &area,
                "requiredFlags",
                json!(flags),
                ItemStatus::UnsupportedBlocksPlans,
                "indexer flags are not evaluated; releases from this indexer are rejected",
            );
        }
        let multi = field_ints(&ix.fields, "multiLanguages");
        ctx.item(
            app,
            &area,
            "multiLanguages",
            json!(multi),
            if multi.is_empty() {
                ItemStatus::Enforced
            } else {
                ItemStatus::EnforcedStricter
            },
            "MULTI releases without an explicit allowed language are rejected as ambiguous",
        );
        if !ix.tags.is_empty() {
            *tags_in_use = true;
            ctx.block(
                IssueScope::Media { media },
                "tag_routing",
                format!(
                    "{app} indexer {} is tag-scoped; tag routing is not supported",
                    ix.name
                ),
            );
            ctx.item(
                app,
                &area,
                "tags",
                json!(ix.tags),
                ItemStatus::UnsupportedBlocksPlans,
                "tag-scoped indexers make routing ambiguous; plans for this media kind are blocked",
            );
        }
        if field_str(&ix.fields, "additionalParameters").is_some() {
            ctx.item(
                app,
                &area,
                "additionalParameters",
                json!("<withheld>"),
                ItemStatus::ObservedOnly,
                "not applied to Prowlarr searches (value withheld from the report)",
            );
        }
        if ix.download_client_id != 0 {
            ctx.item(
                app,
                &area,
                "downloadClientId",
                json!(ix.download_client_id),
                ItemStatus::ObservedOnly,
                "client pinning is not applied: grabs go to the imported enabled torrent client through the configured [qbittorrent] connection",
            );
        }
        if let Some(age) = ix
            .season_search_maximum_single_episode_age
            .filter(|a| *a > 0)
        {
            ctx.item(
                app,
                &area,
                "seasonSearchMaximumSingleEpisodeAge",
                json!(age),
                ItemStatus::ObservedOnly,
                "season plans always search missing episodes individually",
            );
        }
        if field_bool(&ix.fields, "removeYear") == Some(true) {
            ctx.item(
                app,
                &area,
                "removeYear",
                json!(true),
                ItemStatus::ObservedOnly,
                "movie queries always include the year",
            );
        }
        let anime = field_ints(&ix.fields, "animeCategories");
        if !anime.is_empty() {
            ctx.item(
                app,
                &area,
                "animeCategories",
                json!(anime),
                ItemStatus::NotApplicable,
                "anime numbering is not supported",
            );
        }
        if !field_ints(&ix.fields, "failDownloads").is_empty() {
            ctx.item(
                app,
                &area,
                "failDownloads",
                json!(field_ints(&ix.fields, "failDownloads")),
                ItemStatus::NotImplemented,
                "failing downloads that contain these file types is not implemented; import takes only ffprobe-validated video files and leaves other files in place",
            );
        }
        ctx.item(
            app,
            &area,
            "enableRss",
            json!(ix.enable_rss),
            ItemStatus::Enforced,
            if ix.enable_rss {
                "with execution enabled, RSS sync reads this indexer's Torznab feed through Prowlarr every rssSyncInterval ([automation] rss_interval_minutes overrides; 0 disables) and grabs only managed, monitored, wanted items; never a backlog search"
            } else {
                "RSS sync does not read this indexer's feed"
            },
        );
        let seed_ratio = field_f64(&ix.fields, "seedCriteria.seedRatio");
        let seed_time = field_i64(&ix.fields, "seedCriteria.seedTime");
        let pack_seed_time = field_i64(&ix.fields, "seedCriteria.seasonPackSeedTime");
        let seed_goal = seed_ratio.is_some_and(|r| r >= 0.0)
            || seed_time.is_some_and(|t| t >= 0)
            || pack_seed_time.is_some_and(|t| t >= 0);
        ctx.item(
            app,
            &area,
            "seed criteria",
            json!({ "seedRatio": seed_ratio, "seedTime": seed_time, "seasonPackSeedTime": pack_seed_time }),
            ItemStatus::Enforced,
            if seed_goal {
                "with execution enabled, sent to qBittorrent as the torrent's ratio/seeding-time limits (seasonPackSeedTime for season packs; unset values use the client's global limits); download cleanup removes a torrent only after a verified import once these are met"
            } else {
                "no seed goal: submitted torrents use the client's global share limits, which download cleanup also waits for"
            },
        );
        ctx.item(app, &area, "mapping", json!({
            "baseUrl": base.as_deref().map(sanitize_url),
            "prowlarr_indexer_id": px.id,
            "prowlarr_name": px.name,
            "categories": categories,
            "priority": ix.priority,
            "minimumSeeders": minimum_seeders,
            "enableAutomaticSearch": ix.enable_automatic_search,
            "enableInteractiveSearch": ix.enable_interactive_search,
        }), ItemStatus::Enforced, "searched via Prowlarr GET /api/v1/search with these categories; priority and minimum seeders applied");
        out.push(IndexerSetting {
            media,
            prowlarr_indexer_id: px.id as u32,
            name: px.name.clone(),
            native: NativeRef {
                app: app.into(),
                id: ix.id,
                name: ix.name.clone(),
            },
            protocol: "torrent".into(),
            categories,
            priority: ix.priority.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            minimum_seeders,
            enable_automatic_search: ix.enable_automatic_search,
            enable_interactive_search: ix.enable_interactive_search,
            enable_rss: ix.enable_rss,
            seed_ratio,
            seed_time_minutes: seed_time,
            season_pack_seed_time_minutes: pack_seed_time,
            unsupported,
            source: Some(indexer_source(px)),
        });
    }
    out
}

/// The Prowlarr indexer behind an imported setting: implementation, definition and its
/// non-secret field values (bounded), for checking a native indexer that serves the same id.
fn indexer_source(px: &NProwlarrIndexer) -> IndexerSource {
    let definition = px
        .definition_name
        .clone()
        .or_else(|| field_str(&px.fields, "definitionFile"))
        .map(|d| d.chars().take(100).collect());
    let mut settings = std::collections::BTreeMap::new();
    for f in &px.fields {
        if is_secret_field(f) || f.name.is_empty() || f.name.len() > 100 || settings.len() >= 64 {
            continue;
        }
        settings.insert(f.name.clone(), source_value(&f.name, &f.value));
    }
    IndexerSource {
        implementation: px
            .implementation
            .clone()
            .map(|i| i.chars().take(100).collect()),
        definition,
        settings,
    }
}

/// Marker for a copied value that is not kept verbatim. It is never a default, so a native
/// provider that does not know the field refuses it (fail closed).
pub const WITHHELD: &str = "<withheld>";

/// How one copied Prowlarr field value is kept. Credential fields never reach this (they are
/// skipped by name and privacy). Empty and null stay empty (the field's default); numbers and
/// booleans are kept; URL fields lose credentials, query and fragment; short plain tokens
/// (host names, definition ids, uploader names, select keys) are kept as they are; anything
/// else becomes a non-default withheld marker.
fn source_value(name: &str, v: &Option<Value>) -> Value {
    let plain = |s: &str| {
        s.len() <= 100
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | ':'))
    };
    match v {
        None | Some(Value::Null) => Value::Null,
        Some(v @ (Value::Bool(_) | Value::Number(_))) => v.clone(),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                Value::String(String::new())
            } else if name.eq_ignore_ascii_case("baseUrl") || t.contains("://") {
                Value::String(match reqwest::Url::parse(t) {
                    Ok(_) => sanitize_url(t),
                    Err(_) => WITHHELD.into(),
                })
            } else if plain(t) {
                Value::String(t.to_string())
            } else {
                Value::String(WITHHELD.into())
            }
        }
        Some(Value::Array(a)) if a.is_empty() => Value::Array(vec![]),
        Some(Value::Array(a)) if a.len() <= 50 && a.iter().all(Value::is_number) => {
            Value::Array(a.clone())
        }
        Some(Value::Object(o)) if o.is_empty() => json!({}),
        Some(_) => Value::String(WITHHELD.into()),
    }
}

fn download_clients(
    ctx: &mut Ctx,
    app: &str,
    media: MediaKind,
    list: Vec<NDownloadClient>,
) -> Vec<DownloadClient> {
    let (cat, imported_cat) = match media {
        MediaKind::Movie => ("movieCategory", "movieImportedCategory"),
        MediaKind::Series => ("tvCategory", "tvImportedCategory"),
    };
    list.into_iter()
        .map(|c| {
            let creds = note_credentials(ctx, app, "downloadclient", c.id, &c.name, &c.fields);
            let dc = DownloadClient {
                media,
                native: NativeRef {
                    app: app.into(),
                    id: c.id,
                    name: c.name.clone(),
                },
                implementation: c.implementation.clone(),
                protocol: c.protocol.clone(),
                enabled: c.enable,
                priority: c.priority,
                host: field_str(&c.fields, "host"),
                port: field_i64(&c.fields, "port"),
                use_ssl: field_bool(&c.fields, "useSsl"),
                url_base: field_str(&c.fields, "urlBase"),
                category: field_str(&c.fields, cat),
                post_import_category: field_str(&c.fields, imported_cat),
                initial_state: field_i64(&c.fields, "initialState"),
                sequential_order: field_bool(&c.fields, "sequentialOrder"),
                first_and_last: field_bool(&c.fields, "firstAndLast"),
                content_layout: field_i64(&c.fields, "contentLayout"),
                remove_completed_downloads: c.remove_completed_downloads,
                remove_failed_downloads: c.remove_failed_downloads,
                credential_fields_in_snapshot: creds,
                execution: "server_config".into(),
            };
            let mut value = serde_json::to_value(&dc).unwrap_or(Value::Null);
            if let Some(m) = value.as_object_mut() {
                m.remove("native");
                m.remove("media");
            }
            let (status, effect) = if !c.enable || !c.protocol.eq_ignore_ascii_case("torrent") {
                (ItemStatus::ObservedOnly, "not used: only enabled torrent clients receive grabs")
            } else if !c.implementation.eq_ignore_ascii_case("QBittorrent") {
                (ItemStatus::ObservedOnly, "not qBittorrent: if selected, execution for this media kind is refused")
            } else {
                (ItemStatus::Enforced, "when this is the enabled torrent client with the lowest priority value and execution is enabled: category, initial state, sequential/first-last, content layout, post-import category and remove completed/failed downloads are applied through the configured [qbittorrent] connection (host and credentials come from the server config, never the snapshot)")
            };
            ctx.item(
                app,
                &format!("downloadclient[{}] {}", c.id, c.name),
                "client",
                value,
                status,
                effect,
            );
            if !c.tags.is_empty() {
                ctx.item(
                    app,
                    &format!("downloadclient[{}] {}", c.id, c.name),
                    "tags",
                    json!(c.tags),
                    ItemStatus::NotImplemented,
                    "tag-scoped client routing is not implemented; the enabled torrent client with the lowest priority value is used",
                );
            }
            dc
        })
        .collect()
}

const STANDARD_FOLDER_FORMATS: &[&str] = &[
    "{Movie Title} ({Release Year})",
    "{Series Title}",
    "Season {season}",
];

/// Radarr `smart`, Sonarr `4` (Smart).
fn is_smart_colon(v: &Value) -> bool {
    v.as_str() == Some("smart") || v.as_i64() == Some(4)
}

/// The media-management/naming conventions completed-download import implements; anything
/// else blocks file import for that media kind (searches and plans continue).
fn media_management(
    ctx: &mut Ctx,
    app: &str,
    media: MediaKind,
    obj: &Map<String, Value>,
) -> Option<MediaManagement> {
    let scope = IssueScope::Import { media };
    let section = |k: &str| obj.get(k).and_then(Value::as_object);
    let (Some(mm), Some(naming)) = (section("config/mediamanagement"), section("config/naming"))
    else {
        ctx.block(scope, "import_settings_missing", format!("{app} config/mediamanagement or config/naming is missing from the snapshot; re-export it to enable file import"));
        return None;
    };
    let flag = |o: &Map<String, Value>, k: &str| o.get(k).and_then(Value::as_bool);
    let (rename, folders): (&str, &[(&str, &str)]) = match media {
        MediaKind::Movie => (
            "renameMovies",
            &[("movieFolderFormat", "{Movie Title} ({Release Year})")],
        ),
        MediaKind::Series => (
            "renameEpisodes",
            &[
                ("seriesFolderFormat", "{Series Title}"),
                ("seasonFolderFormat", "Season {season}"),
            ],
        ),
    };
    let mut problems: Vec<(&str, String)> = vec![];
    if flag(naming, rename) != Some(false) {
        problems.push((
            "rename_unsupported",
            format!("{app} {rename} is not false; renaming is not implemented"),
        ));
    }
    for (key, want) in folders {
        if naming.get(*key).and_then(Value::as_str) != Some(*want) {
            problems.push((
                "folder_format_unsupported",
                format!("{app} {key} is not '{want}'"),
            ));
        }
    }
    if !naming
        .get("colonReplacementFormat")
        .is_some_and(is_smart_colon)
    {
        problems.push((
            "colon_replacement_unsupported",
            format!("{app} colonReplacementFormat is not smart"),
        ));
    }
    for key in ["importExtraFiles", "setPermissionsLinux", "useScriptImport"] {
        if flag(mm, key) != Some(false) {
            problems.push((
                "media_management_unsupported",
                format!("{app} {key} is enabled or missing"),
            ));
        }
    }
    if mm
        .get("recycleBin")
        .and_then(Value::as_str)
        .is_some_and(|r| !r.trim().is_empty())
    {
        problems.push((
            "recycle_bin_unsupported",
            format!("{app} uses a recycle bin"),
        ));
    }
    if mm
        .get("fileDate")
        .and_then(Value::as_str)
        .is_some_and(|d| d != "none")
    {
        problems.push(("file_date_unsupported", format!("{app} changes file dates")));
    }
    if section("config/downloadclient").and_then(|d| flag(d, "enableCompletedDownloadHandling"))
        == Some(false)
    {
        problems.push((
            "completed_download_handling_disabled",
            format!("{app} completed download handling is off natively"),
        ));
    }
    for (code, message) in problems {
        ctx.block(scope.clone(), code, message);
    }
    Some(MediaManagement {
        media,
        copy_using_hardlinks: flag(mm, "copyUsingHardlinks").unwrap_or(true),
        minimum_free_space_mb: mm
            .get("minimumFreeSpaceWhenImporting")
            .and_then(Value::as_u64)
            .unwrap_or(100),
        replace_illegal_characters: flag(naming, "replaceIllegalCharacters").unwrap_or(true),
    })
}

fn config_items(
    ctx: &mut Ctx,
    app: &str,
    section: &str,
    obj: &Map<String, Value>,
    policy: &mut ReleasePolicy,
) {
    for (k, v) in obj {
        if k == "id" {
            continue;
        }
        let (status, effect): (ItemStatus, String) = match (section, k.as_str()) {
            ("config/indexer", "maximumSize") => {
                policy.maximum_size_mb = v.as_i64().unwrap_or(0).max(0);
                (
                    ItemStatus::Enforced,
                    "releases larger than this many MB are rejected (0 = unlimited)".into(),
                )
            }
            ("config/indexer", "allowHardcodedSubs") => {
                policy.allow_hardcoded_subs = v.as_bool().unwrap_or(false);
                (
                    ItemStatus::Enforced,
                    "releases with hardcoded-subtitle markers are rejected when false".into(),
                )
            }
            ("config/indexer", "whitelistedHardcodedSubs") => {
                if v.as_str().unwrap_or("").trim().is_empty() {
                    (ItemStatus::NotApplicable, "empty whitelist".into())
                } else {
                    (
                        ItemStatus::EnforcedStricter,
                        "whitelist is not evaluated; all hardcoded subs are rejected".into(),
                    )
                }
            }
            ("config/indexer", "minimumAge" | "retention") => (
                ItemStatus::NotApplicable,
                "usenet-only in native; this build searches torrents only".into(),
            ),
            ("config/indexer", "rssSyncInterval") => {
                let minutes = v.as_i64().filter(|m| *m >= 0).map(|m| m.min(1440) as u32);
                policy.rss_sync_interval_minutes = minutes;
                (
                    ItemStatus::Enforced,
                    match minutes {
                        Some(0) => "RSS sync is disabled natively; no Torznab feed is read for this media kind".into(),
                        Some(m) => format!("Torznab RSS sync through Prowlarr every {m} min for RSS-enabled indexers"),
                        None => "unreadable value; the default 15 min RSS interval applies".into(),
                    },
                )
            }
            ("config/indexer", "preferIndexerFlags") => (
                ItemStatus::ObservedOnly,
                "indexer flags are not used for ranking".into(),
            ),
            ("config/indexer", "availabilityDelay") => {
                policy.availability_delay_days = v.as_i64().unwrap_or(0).clamp(-365, 365);
                (
                    ItemStatus::Enforced,
                    "days added to a movie's release date (minimum availability released) before it may be grabbed".into(),
                )
            }
            ("config/mediamanagement", "downloadPropersAndRepacks") => {
                let s = v.as_str().unwrap_or("");
                policy.prefer_propers_and_repacks = s != "doNotPrefer";
                policy.revision_upgrades = s == "preferAndUpgrade";
                (ItemStatus::Enforced, "propers/repacks rank above originals unless doNotPrefer; with preferAndUpgrade an exact-quality higher revision replaces the owned managed file (journaled, reversible)".into())
            }
            ("config/mediamanagement", "copyUsingHardlinks" | "minimumFreeSpaceWhenImporting" | "enableMediaInfo") => (
                ItemStatus::Enforced,
                "applied by completed-download import (hardlink else staged copy; free-space check; ffprobe media validation)".into(),
            ),
            ("config/mediamanagement", "skipFreeSpaceCheckWhenImporting") => match v.as_bool() {
                Some(true) => (ItemStatus::EnforcedStricter, "free space is always checked".into()),
                _ => (ItemStatus::Enforced, "free space is checked before every import".into()),
            },
            ("config/mediamanagement", "importExtraFiles" | "setPermissionsLinux" | "useScriptImport") => match v.as_bool() {
                Some(false) => (ItemStatus::NotApplicable, "off natively; not used".into()),
                _ => (ItemStatus::UnsupportedBlocksImport, "not implemented; file import is blocked".into()),
            },
            ("config/mediamanagement", "recycleBin") => match v.as_str().map(str::trim) {
                Some("") | None => (ItemStatus::Enforced, "no recycle bin: a replaced managed file is removed only after its verified replacement is committed".into()),
                _ => (ItemStatus::UnsupportedBlocksImport, "a recycle bin is not implemented; file import is blocked".into()),
            },
            ("config/mediamanagement", "fileDate") => match v.as_str() {
                Some("none") => (ItemStatus::Enforced, "file dates are left unchanged".into()),
                _ => (ItemStatus::UnsupportedBlocksImport, "changing file dates is not implemented; file import is blocked".into()),
            },
            ("config/mediamanagement", "createEmptyMovieFolders" | "createEmptySeriesFolders" | "deleteEmptyFolders" | "autoRenameFolders") => (
                ItemStatus::NotApplicable,
                "this build creates a folder only when importing into it and never deletes folders".into(),
            ),
            ("config/mediamanagement", "rescanAfterRefresh") => (
                ItemStatus::ObservedOnly,
                "library rescans run on their own schedule ([automation] rescan_hours) and after imports".into(),
            ),
            ("config/mediamanagement", _) => (
                ItemStatus::NotApplicable,
                "has no effect with rename off and without permission/extra-file handling".into(),
            ),
            ("config/naming", "renameMovies" | "renameEpisodes") => match v.as_bool() {
                Some(false) => (ItemStatus::Enforced, "original file names are kept".into()),
                _ => (ItemStatus::UnsupportedBlocksImport, "renaming is not implemented; file import is blocked".into()),
            },
            ("config/naming", "movieFolderFormat" | "seriesFolderFormat" | "seasonFolderFormat") => {
                if STANDARD_FOLDER_FORMATS.contains(&v.as_str().unwrap_or("")) {
                    (ItemStatus::Enforced, "folders for new items use this template".into())
                } else {
                    (ItemStatus::UnsupportedBlocksImport, "only the standard folder templates are implemented; file import is blocked".into())
                }
            }
            ("config/naming", "colonReplacementFormat") => {
                if is_smart_colon(v) {
                    (ItemStatus::Enforced, "smart colon replacement in generated folder names".into())
                } else {
                    (ItemStatus::UnsupportedBlocksImport, "only smart colon replacement is implemented; file import is blocked".into())
                }
            }
            ("config/naming", "replaceIllegalCharacters") => (
                ItemStatus::Enforced,
                "illegal characters in generated folder names are replaced (else removed)".into(),
            ),
            ("config/naming", _) => (
                ItemStatus::NotApplicable,
                "file-name formats only apply when renaming, which is off".into(),
            ),
            ("config/downloadclient", "enableCompletedDownloadHandling") => match v.as_bool() {
                Some(true) => (ItemStatus::Enforced, "completed downloads are imported automatically".into()),
                _ => (ItemStatus::UnsupportedBlocksImport, "disabled natively; automatic file import is blocked".into()),
            },
            ("config/downloadclient", "autoRedownloadFailed") => {
                policy.redownload_failed = v.as_bool().unwrap_or(false);
                (ItemStatus::Enforced, "a failed automatic download is blocklisted and searched again".into())
            }
            ("config/downloadclient", "autoRedownloadFailedFromInteractiveSearch") => {
                policy.redownload_failed_interactive = v.as_bool().unwrap_or(false);
                (ItemStatus::Enforced, "a failed manually selected download is blocklisted and searched again".into())
            }
            ("config/downloadclient", "checkForFinishedDownloadInterval") => (
                ItemStatus::ObservedOnly,
                "the client is polled every [automation] poll_seconds".into(),
            ),
            ("config/downloadclient", _) => (
                ItemStatus::NotApplicable,
                "usenet working folders do not apply to torrents".into(),
            ),
            _ => (ItemStatus::ObservedOnly, "recorded only".into()),
        };
        let value = if is_secret_name(k) {
            json!("<withheld>")
        } else {
            v.clone()
        };
        ctx.item(app, section, k.clone(), value, status, effect);
    }
}

fn app_object<'a>(root: &'a Map<String, Value>, name: &str) -> Result<&'a Map<String, Value>> {
    match root.get(name) {
        Some(Value::Object(m)) => Ok(m),
        Some(_) => bail!("snapshot.{name} must be an object"),
        None => bail!("snapshot is missing the '{name}' object"),
    }
}

fn source_summary(
    app: &str,
    obj: &Map<String, Value>,
    expected: &[&str],
    catalog: &[&str],
) -> SourceSummary {
    let mut known: Vec<&str> = expected
        .iter()
        .chain(catalog)
        .chain(OBSERVATION_SECTIONS)
        .copied()
        .collect();
    if app == "sonarr" {
        known.push("languageprofile");
    }
    SourceSummary {
        app: app.into(),
        sections_present: obj
            .keys()
            .filter(|k| known.contains(&k.as_str()))
            .cloned()
            .collect(),
        sections_missing: expected
            .iter()
            .chain(catalog)
            .filter(|k| !obj.contains_key(**k))
            .map(|k| k.to_string())
            .collect(),
        sections_ignored: obj
            .keys()
            .filter(|k| !known.contains(&k.as_str()))
            .cloned()
            .collect(),
    }
}

fn app_settings(
    ctx: &mut Ctx,
    app: &str,
    media: MediaKind,
    obj: &Map<String, Value>,
    prowlarr: &[NProwlarrIndexer],
    series_language: &LanguageRule,
    out: &mut Settings,
) -> Result<Vec<String>> {
    let defs: Vec<NQualityDefinition> = section_list(obj, app, "qualitydefinition", true)?;
    if defs.is_empty() {
        bail!("{app}.qualitydefinition is empty");
    }
    let profile_list: Vec<NQualityProfile> = section_list(obj, app, "qualityprofile", true)?;
    if profile_list.is_empty() {
        bail!("{app}.qualityprofile is empty");
    }
    out.size_limits.extend(size_limits(ctx, app, media, &defs));
    out.profiles.extend(profiles(
        ctx,
        app,
        media,
        profile_list,
        &defs,
        series_language,
    ));

    let mut tags_in_use = false;
    let ix: Vec<NIndexer> = section_list(obj, app, "indexer", true)?;
    out.indexers
        .extend(indexers(ctx, app, media, ix, prowlarr, &mut tags_in_use));
    let dcs: Vec<NDownloadClient> = section_list(obj, app, "downloadclient", true)?;
    out.download_clients
        .extend(download_clients(ctx, app, media, dcs));

    let roots: Vec<NRootFolder> = section_list(obj, app, "rootfolder", false)?;
    let root_paths: Vec<String> = roots.iter().map(|r| r.path.clone()).collect();
    for r in roots {
        ctx.item(
            app,
            "rootfolder",
            r.path.clone(),
            json!({ "accessible": r.accessible, "freeSpace": r.free_space }),
            ItemStatus::ObservedOnly,
            "native root for new items; with [library] mapped, imports write and library scans read below it",
        );
        out.root_folders.push(RootFolder {
            media,
            path: r.path,
            accessible_at_snapshot: r.accessible,
            free_space_bytes_at_snapshot: r.free_space,
        });
    }

    let mut policy = ReleasePolicy {
        media,
        maximum_size_mb: 0,
        allow_hardcoded_subs: media == MediaKind::Series,
        prefer_propers_and_repacks: true,
        torrent_enabled: true,
        torrent_delay_minutes: 0,
        bypass_delay_if_highest_quality: true,
        revision_upgrades: false,
        redownload_failed: false,
        redownload_failed_interactive: false,
        rss_sync_interval_minutes: None,
        availability_delay_days: 0,
    };
    let delays: Vec<NDelayProfile> = section_list(obj, app, "delayprofile", false)?;
    if !obj.contains_key("delayprofile") {
        ctx.item(
            app,
            "delayprofile",
            "missing",
            Value::Null,
            ItemStatus::Enforced,
            "no delay profile in snapshot: no delay is applied",
        );
    }
    for d in delays {
        let area = format!("delayprofile[{}]", d.id);
        if !d.tags.is_empty() {
            tags_in_use = true;
            ctx.block(
                IssueScope::Media { media },
                "tag_routing",
                format!(
                    "{app} delay profile {} is tag-scoped; tag routing is not supported",
                    d.id
                ),
            );
            ctx.item(
                app,
                &area,
                "tags",
                json!(d.tags),
                ItemStatus::UnsupportedBlocksPlans,
                "tag-scoped delay profiles are not supported",
            );
            continue;
        }
        policy.torrent_enabled = d.enable_torrent;
        policy.torrent_delay_minutes = d.torrent_delay.max(0);
        policy.bypass_delay_if_highest_quality = d.bypass_if_highest_quality;
        ctx.item(app, &area, "torrent", json!({ "enableTorrent": d.enable_torrent, "torrentDelay": d.torrent_delay, "bypassIfHighestQuality": d.bypass_if_highest_quality }), ItemStatus::Enforced, "torrent releases younger than the delay are not approved unless in the top tier (when bypass is on)");
        ctx.item(app, &area, "usenet", json!({ "enableUsenet": d.enable_usenet, "usenetDelay": d.usenet_delay, "preferredProtocol": d.preferred_protocol }), ItemStatus::NotApplicable, "torrent-only build");
        if d.bypass_if_above_custom_format_score || d.minimum_custom_format_score != 0 {
            ctx.item(app, &area, "custom format bypass", json!({ "bypassIfAboveCustomFormatScore": d.bypass_if_above_custom_format_score, "minimumCustomFormatScore": d.minimum_custom_format_score }), ItemStatus::EnforcedStricter, "custom-format bypass is never applied (delay always honoured)");
        }
    }

    let cfs = obj
        .get("customformat")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    ctx.item(
        app,
        "customformat",
        "count",
        json!(cfs),
        if cfs == 0 {
            ItemStatus::NotApplicable
        } else {
            ItemStatus::ObservedOnly
        },
        "custom formats are not evaluated; profiles that score them are blocked",
    );
    let tags = obj
        .get("tag")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    ctx.item(
        app,
        "tag",
        "count",
        json!(tags),
        ItemStatus::ObservedOnly,
        if tags_in_use {
            "tags are used by indexers/delay profiles (see blocking issues)"
        } else {
            "no indexer or delay profile is tag-scoped"
        },
    );
    let rpm = obj
        .get("remotepathmapping")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    ctx.item(
        app,
        "remotepathmapping",
        "count",
        json!(rpm),
        if rpm == 0 {
            ItemStatus::NotApplicable
        } else {
            ItemStatus::NotImplemented
        },
        "native remote path mappings are not applied; [qbittorrent] save_path/local_path map client paths for import",
    );
    release_profiles(ctx, app, media, obj)?;

    for section in [
        "config/indexer",
        "config/mediamanagement",
        "config/naming",
        "config/downloadclient",
    ] {
        if let Some(o) = section_object(obj, app, section)? {
            config_items(ctx, app, section, o, &mut policy);
        } else {
            ctx.item(app, section, "missing", Value::Null, ItemStatus::ObservedOnly, "section absent from snapshot; defaults assumed (maximumSize 0, allowHardcodedSubs native default)");
        }
    }
    out.release_policies.push(policy);
    out.media_management
        .extend(media_management(ctx, app, media, obj));

    if let Some(Value::Array(h)) = obj.get("health") {
        let checks: Vec<Value> = h
            .iter()
            .map(|c| json!({ "source": c.get("source"), "type": c.get("type") }))
            .collect();
        ctx.item(
            app,
            "health",
            "checks at snapshot time",
            Value::Array(checks),
            ItemStatus::ObservedOnly,
            "native health is not re-checked",
        );
    }
    Ok(root_paths)
}

/// Release profiles (required/ignored terms) are not evaluated: enabled ones block plans, and
/// an absent section is reported because the terms are then unknown.
fn release_profiles(
    ctx: &mut Ctx,
    app: &str,
    media: MediaKind,
    obj: &Map<String, Value>,
) -> Result<()> {
    match obj.get("releaseprofile") {
        None | Some(Value::Null) => ctx.item(
            app,
            "releaseprofile",
            "missing",
            Value::Null,
            ItemStatus::ObservedOnly,
            "not in the snapshot: any native required/ignored terms are unknown and NOT enforced; export /api/v3/releaseprofile to close this gap",
        ),
        Some(Value::Array(list)) => {
            let enabled = list
                .iter()
                .filter(|p| p.get("enabled").and_then(Value::as_bool).unwrap_or(true))
                .count();
            if enabled == 0 {
                ctx.item(app, "releaseprofile", "enabled profiles", json!(0), ItemStatus::NotApplicable, "no enabled release profiles");
            } else {
                ctx.block(
                    IssueScope::Media { media },
                    "release_profile_terms",
                    format!("{app} has {enabled} enabled release profile(s); required/ignored terms are not supported"),
                );
                ctx.item(app, "releaseprofile", "enabled profiles", json!(enabled), ItemStatus::UnsupportedBlocksPlans, "required/ignored terms are not evaluated; plans for this media kind are blocked");
            }
        }
        Some(_) => bail!("{app}.releaseprofile must be an array"),
    }
    Ok(())
}

/// Effective Sonarr v4 language policy. v4 retired language profiles (rules moved to custom
/// formats, which the profile import handles fail-closed); its `languageprofile` endpoint
/// returns a compatibility stub named "Deprecated", recorded here as an observation only.
/// Anything else (pre-v4 profiles) is not modelled and blocks series plans.
fn series_language_rule(ctx: &mut Ctx, sonarr: &Map<String, Value>) -> Result<LanguageRule> {
    let profiles: Vec<NLanguageProfile> = section_list(sonarr, "sonarr", "languageprofile", false)?;
    let any = |source: &str| LanguageRule {
        requirement: LanguageRequirement::Any,
        unmarked: UnmarkedLanguage::Accept,
        source: source.into(),
    };
    Ok(match profiles.as_slice() {
        [] => any(
            "Sonarr v4: no language profile; language rules would come from scored custom formats",
        ),
        [stub] if stub.name.eq_ignore_ascii_case("deprecated") => {
            let langs: Vec<String> = stub
                .languages
                .iter()
                .filter(|l| l.allowed)
                .map(|l| normalize_language(&l.language.name))
                .collect();
            ctx.item(
                "sonarr",
                "languageprofile",
                stub.name.clone(),
                json!({ "allowed_languages": langs }),
                ItemStatus::ObservedOnly,
                "retired Sonarr v4 compatibility stub (language profiles were replaced by custom formats); recorded only, not enforced",
            );
            any(
                "Sonarr v4: language profiles retired and no custom formats are scored, so no language filter applies (native parity)",
            )
        }
        other => {
            let names: Vec<&str> = other.iter().map(|p| p.name.as_str()).collect();
            ctx.block(
                IssueScope::Media {
                    media: MediaKind::Series,
                },
                "language_profile_unsupported",
                format!("Sonarr language profiles {names:?} are not the v4 'Deprecated' stub; pre-v4 language profiles are not supported"),
            );
            ctx.item(
                "sonarr",
                "languageprofile",
                "profiles",
                json!(names),
                ItemStatus::UnsupportedBlocksPlans,
                "pre-v4 language profiles are not modelled; series plans are blocked",
            );
            any("unsupported Sonarr language profiles (series plans blocked)")
        }
    })
}

struct CatalogOut {
    movies: Vec<Movie>,
    series: Vec<Series>,
    notes: Vec<String>,
    conflicts: Vec<String>,
    skipped: Vec<String>,
    with_samples: usize,
}

fn catalog(
    ctx: &Ctx,
    radarr: &Map<String, Value>,
    sonarr: &Map<String, Value>,
    movie_roots: &[String],
    series_roots: &[String],
    profile_ids: &[String],
) -> Result<CatalogOut> {
    let mut out = CatalogOut {
        movies: vec![],
        series: vec![],
        notes: vec![],
        conflicts: vec![],
        skipped: vec![],
        with_samples: 0,
    };
    let provenance = |app: &str, id: i64| Provenance {
        source_app: app.into(),
        native_id: id,
        import_id: ctx.import_id.clone(),
        observed_at: ctx.observed_at.clone(),
    };
    let profile_ref =
        |app: &str, id: i64| -> Option<String> { (id > 0).then(|| format!("{app}:{id}")) };

    match radarr.get("movie") {
        None => out
            .notes
            .push("radarr.movie absent: no movie catalog imported".into()),
        Some(Value::Array(items)) => {
            for (i, v) in items.iter().enumerate() {
                let m = match NMovie::deserialize(v) {
                    Ok(m) => m,
                    Err(e) => {
                        out.skipped
                            .push(format!("radarr.movie[{i}]: unexpected shape ({e})"));
                        continue;
                    }
                };
                let Some(tmdb) = positive_u32(Some(m.tmdb_id)) else {
                    out.skipped.push(format!(
                        "radarr.movie[{i}] (native id {}): missing tmdbId",
                        m.id
                    ));
                    continue;
                };
                if m.title.trim().is_empty() {
                    out.skipped.push(format!(
                        "radarr.movie[{i}] (native id {}): empty title",
                        m.id
                    ));
                    continue;
                }
                let has_file = m
                    .has_file
                    .or(m
                        .statistics
                        .as_ref()
                        .and_then(|s| s.movie_file_count)
                        .map(|c| c > 0))
                    .unwrap_or(m.movie_file.is_some());
                let quality_profile = profile_ref("radarr", m.quality_profile_id);
                if let Some(p) = &quality_profile
                    && !profile_ids.contains(p)
                {
                    out.notes.push(format!(
                        "movie tmdb:{tmdb} references unknown profile {p}; plans for it are blocked"
                    ));
                }
                let root_folder = root_for(
                    m.path.as_deref(),
                    m.root_folder_path.as_deref(),
                    movie_roots,
                );
                let folder = relative_folder(m.path.as_deref(), root_folder.as_deref());
                let file = has_file
                    .then(|| file_evidence(&m.movie_file, folder.as_deref(), &ctx.observed_at));
                out.movies.push(Movie {
                    acquisition: None,
                    folder,
                    file,
                    ownership_unknown: false,
                    queued: false,
                    id: MediaId::Movie { tmdb },
                    tmdb_id: tmdb,
                    imdb_id: m.imdb_id.filter(|s| !s.trim().is_empty()),
                    original_title: m
                        .original_title
                        .filter(|t| !t.trim().is_empty() && *t != m.title),
                    alternate_titles: m
                        .alternate_titles
                        .into_iter()
                        .map(|a| a.title)
                        .filter(|t| !t.trim().is_empty())
                        .collect(),
                    year: year(m.year),
                    secondary_year: year(m.secondary_year),
                    runtime_minutes: positive_u32(m.runtime),
                    original_language: m
                        .original_language
                        .map(|l| normalize_language(&l.name))
                        .filter(|l| !l.is_empty()),
                    status: m.status,
                    is_available: m.is_available,
                    minimum_availability: m.minimum_availability.filter(|s| !s.trim().is_empty()),
                    in_cinemas: m.in_cinemas.filter(|s| !s.trim().is_empty()),
                    physical_release: m.physical_release.filter(|s| !s.trim().is_empty()),
                    digital_release: m.digital_release.filter(|s| !s.trim().is_empty()),
                    monitored: m.monitored,
                    quality_profile,
                    root_folder,
                    has_file,
                    file_quality: file_quality(&m.movie_file),
                    identity_conflict: false,
                    provenance: provenance("radarr", m.id),
                    title: m.title,
                });
            }
        }
        Some(_) => bail!("radarr.movie must be an array"),
    }

    let samples: HashMap<String, Vec<NEpisode>> = match sonarr.get("episode_samples") {
        None | Some(Value::Null) => HashMap::new(),
        Some(Value::Object(map)) => {
            let mut s = HashMap::new();
            for (k, v) in map {
                match Vec::<NEpisode>::deserialize(v) {
                    Ok(eps) => {
                        s.insert(k.clone(), eps);
                    }
                    Err(e) => out.skipped.push(format!(
                        "sonarr.episode_samples[{k}]: unexpected shape ({e})"
                    )),
                }
            }
            s
        }
        Some(_) => bail!("sonarr.episode_samples must be an object keyed by native series id"),
    };
    let mut used_samples = vec![];

    match sonarr.get("series") {
        None => out
            .notes
            .push("sonarr.series absent: no series catalog imported".into()),
        Some(Value::Array(items)) => {
            for (i, v) in items.iter().enumerate() {
                let s = match NSeries::deserialize(v) {
                    Ok(s) => s,
                    Err(e) => {
                        out.skipped
                            .push(format!("sonarr.series[{i}]: unexpected shape ({e})"));
                        continue;
                    }
                };
                let Some(tvdb) = positive_u32(Some(s.tvdb_id)) else {
                    out.skipped.push(format!(
                        "sonarr.series[{i}] (native id {}): missing tvdbId",
                        s.id
                    ));
                    continue;
                };
                if s.title.trim().is_empty() {
                    out.skipped.push(format!(
                        "sonarr.series[{i}] (native id {}): empty title",
                        s.id
                    ));
                    continue;
                }
                let mut scene_numbering = s.use_scene_numbering;
                let mut dropped_aliases = 0;
                let alternate_titles = s
                    .alternate_titles
                    .iter()
                    .filter(|a| !a.title.trim().is_empty())
                    .filter(|a| {
                        let differs = matches!((a.scene_season_number, a.season_number), (Some(sc), Some(se)) if sc >= 0 && se >= 0 && sc != se);
                        dropped_aliases += usize::from(differs);
                        !differs
                    })
                    .map(|a| AltTitle {
                        title: a.title.clone(),
                        season: a.season_number.or(a.scene_season_number).filter(|n| *n >= 0).map(|n| n as u16),
                    })
                    .collect();
                if dropped_aliases > 0 {
                    out.notes.push(format!("series tvdb:{tvdb}: {dropped_aliases} scene-season aliases dropped (scene numbering is not supported)"));
                }
                let series_root = root_for(
                    s.path.as_deref(),
                    s.root_folder_path.as_deref(),
                    series_roots,
                );
                let series_folder = relative_folder(s.path.as_deref(), series_root.as_deref());
                let episodes = samples.get(&s.id.to_string()).map(|eps| {
                    used_samples.push(s.id.to_string());
                    eps.iter()
                        .filter(|e| e.season_number >= 0 && e.episode_number > 0 && e.season_number <= 999 && e.episode_number <= 9999)
                        .map(|e| {
                            let scene_differs = matches!(e.scene_season_number, Some(x) if x != e.season_number)
                                || matches!(e.scene_episode_number, Some(x) if x != e.episode_number);
                            scene_numbering |= scene_differs && (e.scene_season_number.unwrap_or(0) > 0 || e.scene_episode_number.unwrap_or(0) > 0);
                            Episode {
                                season: e.season_number as u16,
                                number: e.episode_number as u16,
                                title: e.title.clone(),
                                air_date_utc: e.air_date_utc.clone(),
                                runtime_minutes: positive_u32(e.runtime),
                                has_file: e.has_file,
                                monitored: e.monitored,
                                file_quality: file_quality(&e.episode_file),
                                file: e.has_file.then(|| {
                                    file_evidence(&e.episode_file, series_folder.as_deref(), &ctx.observed_at)
                                }),
                                ownership_unknown: false,
                                queued: false,
                            }
                        })
                        .collect::<Vec<_>>()
                });
                out.with_samples += usize::from(episodes.is_some());
                let quality_profile = profile_ref("sonarr", s.quality_profile_id);
                if let Some(p) = &quality_profile
                    && !profile_ids.contains(p)
                {
                    out.notes.push(format!("series tvdb:{tvdb} references unknown profile {p}; plans for it are blocked"));
                }
                let mut seasons: Vec<SeasonInfo> = s
                    .seasons
                    .iter()
                    .filter(|x| (0..=999).contains(&x.season_number))
                    .map(|x| {
                        let st = x.statistics.as_ref();
                        let count = |f: fn(&NSeasonStats) -> Option<i64>| {
                            st.and_then(f).filter(|n| *n >= 0).map(|n| n as u32)
                        };
                        SeasonInfo {
                            number: x.season_number as u16,
                            monitored: x.monitored,
                            episode_file_count: count(|s| s.episode_file_count),
                            episode_count: count(|s| s.episode_count),
                            total_episode_count: count(|s| s.total_episode_count),
                        }
                    })
                    .collect();
                seasons.sort_by_key(|x| x.number);
                out.series.push(Series {
                    id: MediaId::Series(SeriesKey::Tvdb(tvdb)),
                    tvdb_id: tvdb,
                    tvmaze_id: positive_u32(s.tv_maze_id),
                    tmdb_id: positive_u32(s.tmdb_id),
                    imdb_id: s.imdb_id.filter(|x| !x.trim().is_empty()),
                    alternate_titles,
                    year: year(s.year),
                    series_type: s.series_type.unwrap_or_else(|| "standard".into()),
                    scene_numbering,
                    runtime_minutes: positive_u32(s.runtime),
                    original_language: s
                        .original_language
                        .map(|l| normalize_language(&l.name))
                        .filter(|l| !l.is_empty()),
                    status: s.status,
                    monitored: s.monitored,
                    quality_profile,
                    root_folder: series_root,
                    folder: series_folder,
                    seasons,
                    episodes,
                    identity_conflict: false,
                    provenance: provenance("sonarr", s.id),
                    monitor_new_items: s
                        .monitor_new_items
                        .map(|m| m.trim().to_ascii_lowercase())
                        .filter(|m| matches!(m.as_str(), "all" | "none")),
                    acquisition: None,
                    native_specials_monitored: None,
                    metadata_hold: None,
                    title: s.title,
                });
            }
        }
        Some(_) => bail!("sonarr.series must be an array"),
    }
    let mut unmatched: Vec<&String> = samples
        .keys()
        .filter(|k| !used_samples.contains(k))
        .collect();
    unmatched.sort();
    for k in unmatched {
        out.notes.push(format!(
            "sonarr.episode_samples[{k}] does not match any imported series"
        ));
    }

    // Identity collisions: keep every entry but mark all colliding ones; plans are blocked.
    let mut seen: HashMap<MediaId, usize> = HashMap::new();
    for m in &out.movies {
        *seen.entry(m.id).or_default() += 1;
    }
    for s in &out.series {
        *seen.entry(s.id).or_default() += 1;
    }
    for m in out.movies.iter_mut().filter(|m| seen[&m.id] > 1) {
        m.identity_conflict = true;
        out.conflicts.push(format!(
            "{} appears more than once (radarr native id {})",
            m.id, m.provenance.native_id
        ));
    }
    for s in out.series.iter_mut().filter(|s| seen[&s.id] > 1) {
        s.identity_conflict = true;
        out.conflicts.push(format!(
            "{} appears more than once (sonarr native id {})",
            s.id, s.provenance.native_id
        ));
    }
    let mut tvmaze: HashMap<u32, Vec<MediaId>> = HashMap::new();
    for s in &out.series {
        if let Some(t) = s.tvmaze_id {
            tvmaze.entry(t).or_default().push(s.id);
        }
    }
    for (t, ids) in tvmaze.iter().filter(|(_, ids)| ids.len() > 1) {
        out.conflicts.push(format!(
            "tvmaze id {t} is shared by {ids:?}; tvmaze-keyed lookups for it are ambiguous"
        ));
    }
    out.conflicts.sort();
    out.movies.sort_by_key(|m| m.id);
    out.series.sort_by_key(|s| s.id);
    Ok(out)
}

/// Pure normalisation of a parsed snapshot.
pub fn normalize(
    root: &Value,
    import_id: &str,
    observed_at: &str,
    snapshot_sha256: &str,
    snapshot_bytes: u64,
    imported_at: &str,
) -> Result<Normalized> {
    let root = root.as_object().context("snapshot must be a JSON object")?;
    let radarr = app_object(root, "radarr")?;
    let sonarr = app_object(root, "sonarr")?;
    let prowlarr = app_object(root, "prowlarr")?;
    let mut ctx = Ctx {
        import_id: import_id.into(),
        observed_at: observed_at.into(),
        items: vec![],
        credentials: vec![],
        blocking: vec![],
    };

    let prowlarr_indexers: Vec<NProwlarrIndexer> =
        section_list(prowlarr, "prowlarr", "indexer", true)?;
    if let Some(Value::Array(px)) = prowlarr.get("indexer") {
        for (raw, p) in px.iter().zip(&prowlarr_indexers) {
            let fields: Vec<NField> = raw
                .get("fields")
                .and_then(|f| Vec::<NField>::deserialize(f).ok())
                .unwrap_or_default();
            note_credentials(&mut ctx, "prowlarr", "indexer", p.id, &p.name, &fields);
            ctx.item("prowlarr", &format!("indexer[{}] {}", p.id, p.name), "indexer", json!({ "enable": p.enable, "protocol": p.protocol, "privacy": p.privacy, "priority": p.priority, "definitionName": p.definition_name, "tags": p.tags }), ItemStatus::ObservedOnly, "searched through Prowlarr when a Radarr/Sonarr indexer maps to it");
        }
    }
    if let Some(Value::Array(apps)) = prowlarr.get("applications") {
        ctx.item(
            "prowlarr",
            "applications",
            "count",
            json!(apps.len()),
            ItemStatus::ObservedOnly,
            "application sync is not used (fields not copied)",
        );
    }

    let series_language = series_language_rule(&mut ctx, sonarr)?;
    let mut settings = Settings {
        schema_version: SETTINGS_SCHEMA_VERSION,
        profiles: vec![],
        size_limits: vec![],
        indexers: vec![],
        release_policies: vec![],
        download_clients: vec![],
        root_folders: vec![],
        blocking_issues: vec![],
        media_management: vec![],
    };
    let movie_roots = app_settings(
        &mut ctx,
        "radarr",
        MediaKind::Movie,
        radarr,
        &prowlarr_indexers,
        &series_language,
        &mut settings,
    )?;
    let series_roots = app_settings(
        &mut ctx,
        "sonarr",
        MediaKind::Series,
        sonarr,
        &prowlarr_indexers,
        &series_language,
        &mut settings,
    )?;
    for media in [MediaKind::Movie, MediaKind::Series] {
        if !settings.indexers.iter().any(|i| i.media == media) {
            ctx.item(media.native_app(), "indexer", "usable indexers", json!(0), ItemStatus::ObservedOnly, "no indexer maps to Prowlarr: release searches for this media kind fail with no_usable_indexers");
        }
    }

    let profile_ids: Vec<String> = settings.profiles.iter().map(|p| p.id.clone()).collect();
    let cat = catalog(
        &ctx,
        radarr,
        sonarr,
        &movie_roots,
        &series_roots,
        &profile_ids,
    )?;
    settings.blocking_issues = ctx.blocking.clone();

    let sources = vec![
        source_summary("radarr", radarr, SETTINGS_SECTIONS, &["movie"]),
        source_summary(
            "sonarr",
            sonarr,
            SETTINGS_SECTIONS,
            &["series", "episode_samples"],
        ),
        source_summary("prowlarr", prowlarr, &["indexer"], &[]),
    ];
    let mut catalog_notes = cat.notes;
    let ignored_top: Vec<&String> = root
        .keys()
        .filter(|k| !matches!(k.as_str(), "radarr" | "sonarr" | "prowlarr"))
        .collect();
    if !ignored_top.is_empty() {
        catalog_notes.push(format!("ignored top-level keys: {ignored_top:?}"));
    }
    let summary = ReportSummary {
        profiles: settings.profiles.len(),
        size_limits: settings.size_limits.len(),
        indexers_usable: settings.indexers.len(),
        indexers_unusable: ctx
            .items
            .iter()
            .filter(|i| i.setting == "mapping" && i.status == ItemStatus::ObservedOnly)
            .count(),
        download_clients: settings.download_clients.len(),
        root_folders: settings.root_folders.len(),
        movies: cat.movies.len(),
        series: cat.series.len(),
        series_with_episode_samples: cat.with_samples,
        catalog_skipped: cat.skipped.len(),
        catalog_identity_conflicts: cat.conflicts.len(),
        blocking_issues: settings.blocking_issues.len(),
        unsupported_items: ctx
            .items
            .iter()
            .filter(|i| i.status == ItemStatus::UnsupportedBlocksPlans)
            .count(),
        not_implemented_items: ctx
            .items
            .iter()
            .filter(|i| i.status == ItemStatus::NotImplemented)
            .count(),
        credential_entries: ctx.credentials.len(),
    };
    let report = MigrationReport {
        schema_version: IMPORT_SCHEMA_VERSION,
        import_id: import_id.into(),
        snapshot_sha256: snapshot_sha256.into(),
        snapshot_bytes,
        imported_at: imported_at.into(),
        observed_at: observed_at.into(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        sources,
        summary,
        blocking_issues: settings.blocking_issues.clone(),
        items: ctx.items,
        credentials: ctx.credentials,
        catalog_notes,
        catalog_conflicts: cat.conflicts,
        catalog_skipped: cat.skipped,
        cutover_blockers: CUTOVER_BLOCKERS.iter().map(|s| s.to_string()).collect(),
    };
    let catalog = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        import_id: import_id.into(),
        observed_at: observed_at.into(),
        movies: cat.movies,
        series: cat.series,
    };
    Ok(Normalized {
        settings,
        catalog,
        report,
    })
}

pub fn render_markdown(r: &MigrationReport) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "# Migration report `{}`\n", r.import_id);
    let _ = writeln!(
        s,
        "- snapshot sha256: `{}` ({} bytes)",
        r.snapshot_sha256, r.snapshot_bytes
    );
    let _ = writeln!(
        s,
        "- observed at: {} / imported at: {}",
        r.observed_at, r.imported_at
    );
    let _ = writeln!(s, "- tool version: {}\n", r.tool_version);
    let _ = writeln!(
        s,
        "## Summary\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(&r.summary).unwrap_or_default()
    );
    let _ = writeln!(s, "## Blocking issues ({})\n", r.blocking_issues.len());
    if r.blocking_issues.is_empty() {
        let _ = writeln!(s, "None: no imported policy blocks plans.\n");
    }
    for b in &r.blocking_issues {
        let _ = writeln!(s, "- `{}` {:?}: {}", b.code, b.scope, b.message);
    }
    let _ = writeln!(
        s,
        "\n## Settings\n\n| app | area | setting | status | effect |\n|---|---|---|---|---|"
    );
    for i in &r.items {
        let status = serde_json::to_value(i.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} |",
            i.app,
            i.area.replace('|', "/"),
            i.setting.replace('|', "/"),
            status,
            i.effect.replace('|', "/")
        );
    }
    let _ = writeln!(s, "\n## Credentials detected (values not copied)\n");
    for c in &r.credentials {
        let _ = writeln!(
            s,
            "- {} {}[{}] {}: {:?}",
            c.app, c.section, c.native_id, c.name, c.fields
        );
    }
    let _ = writeln!(s, "\n## Catalog\n");
    for n in r
        .catalog_notes
        .iter()
        .chain(&r.catalog_conflicts)
        .chain(&r.catalog_skipped)
    {
        let _ = writeln!(s, "- {n}");
    }
    let _ = writeln!(s, "\n## Cutover blockers\n");
    for b in &r.cutover_blockers {
        let _ = writeln!(s, "- {b}");
    }
    s
}

// ---------------------------------------------------------------- persistence

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub import_id: String,
    pub schema_version: u32,
    pub snapshot_sha256: String,
    pub snapshot_bytes: u64,
    pub imported_at: String,
    pub observed_at: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActivePointer {
    pub import_id: String,
    pub manifest_sha256: String,
}

#[derive(Debug, Serialize)]
pub struct ImportSummary {
    pub status: &'static str,
    pub import_id: String,
    pub import_dir: Option<PathBuf>,
    pub snapshot_sha256: String,
    pub observed_at: String,
    pub summary: ReportSummary,
    pub blocking_issues: Vec<BlockingIssue>,
    pub warnings: Vec<String>,
}

pub struct ImportOptions {
    pub state_dir: PathBuf,
    pub snapshot: PathBuf,
    pub observed_at: Option<OffsetDateTime>,
    pub dry_run: bool,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// RFC 3339 in UTC, whole seconds.
pub fn rfc3339(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    t.replace_nanosecond(0)
        .unwrap_or(t)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

pub fn run_import(opts: &ImportOptions) -> Result<ImportSummary> {
    let mut warnings = vec![];
    let meta = std::fs::metadata(&opts.snapshot)
        .with_context(|| format!("cannot read snapshot {}", opts.snapshot.display()))?;
    if !meta.is_file() {
        bail!("snapshot {} is not a regular file", opts.snapshot.display());
    }
    #[cfg(unix)]
    if std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o077 != 0 {
        warnings.push("snapshot file is readable by group/other; it contains credentials (chmod 600 recommended)".into());
    }
    let bytes = fsutil::read_bounded(&opts.snapshot, MAX_SNAPSHOT_BYTES)?;
    let sha = sha256_hex(&bytes);
    let observed = match opts.observed_at {
        Some(t) => t,
        None => meta
            .modified()
            .map(OffsetDateTime::from)
            .unwrap_or_else(|_| OffsetDateTime::now_utc()),
    };
    let now = OffsetDateTime::now_utc();
    if observed > now + time::Duration::minutes(5) {
        warnings.push(format!(
            "observed_at {} is in the future (now {}); catalog age will read as negative",
            rfc3339(observed),
            rfc3339(now)
        ));
    }
    let observed_at = rfc3339(observed);
    let imported_at = rfc3339(now);
    let root: Value = serde_json::from_slice(&bytes).context("snapshot is not valid JSON")?;
    // Content-addressed by snapshot bytes, observation time and the normalised output (from a
    // deterministic probe run): repeating an import is a no-op, while a later observation or an
    // importer change that alters the output produces a new import.
    let probe = normalize(
        &root,
        "imp-pending",
        &observed_at,
        &sha,
        bytes.len() as u64,
        "",
    )?;
    let digest_input = [
        sha.clone(),
        observed_at.clone(),
        serde_json::to_string(&probe.settings)?,
        serde_json::to_string(&probe.catalog)?,
        serde_json::to_string(&probe.report)?,
    ]
    .join("\n");
    let import_id = format!(
        "imp-{}-s{}",
        &sha256_hex(digest_input.as_bytes())[..16],
        IMPORT_SCHEMA_VERSION
    );
    let n = normalize(
        &root,
        &import_id,
        &observed_at,
        &sha,
        bytes.len() as u64,
        &imported_at,
    )?;
    let mut summary = ImportSummary {
        status: "dry_run",
        import_id: import_id.clone(),
        import_dir: None,
        snapshot_sha256: sha.clone(),
        observed_at: observed_at.clone(),
        summary: n.report.summary.clone(),
        blocking_issues: n.settings.blocking_issues.clone(),
        warnings,
    };
    if opts.dry_run {
        return Ok(summary);
    }

    let (status, dir) = write_and_activate(&opts.state_dir, SOURCE_NATIVE, &bytes, &n)?;
    summary.status = status;
    summary.import_dir = Some(dir);
    Ok(summary)
}

/// The source file a native snapshot import keeps (byte for byte).
pub const SOURCE_NATIVE: &str = "source-native.json";

/// Writes `n` as the immutable import `n.report.import_id` (unless that import already
/// exists, which is verified and reused) and makes it the active import. Returns `created`
/// or `unchanged` and the import directory.
pub fn write_and_activate(
    state_dir: &Path,
    source_name: &str,
    source: &[u8],
    n: &Normalized,
) -> Result<(&'static str, PathBuf)> {
    let import_id = n.report.import_id.clone();
    fsutil::ensure_private_dir(state_dir)?;
    let imports = state_dir.join("imports");
    fsutil::ensure_private_dir(&imports)?;
    let final_dir = imports.join(&import_id);
    let (status, manifest_bytes) = if final_dir.exists() {
        // Same content was written before: verify and re-activate rather than rewrite.
        verify_import_dir(&final_dir)?;
        (
            "unchanged",
            fsutil::read_bounded(&final_dir.join("manifest.json"), MAX_IMPORT_FILE_BYTES)?,
        )
    } else {
        let tmp = imports.join(format!(".tmp-{import_id}-{}", fsutil::random_hex(6)?));
        let result = write_import_dir(&tmp, source_name, source, n).and_then(|manifest| {
            std::fs::rename(&tmp, &final_dir).context("cannot move import into place")?;
            fsutil::fsync_dir(&imports)?;
            Ok(manifest)
        });
        match result {
            Ok(m) => ("created", m),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e);
            }
        }
    };
    let pointer = ActivePointer {
        import_id,
        manifest_sha256: sha256_hex(&manifest_bytes),
    };
    fsutil::atomic_replace_private(
        &state_dir.join(ACTIVE_POINTER),
        &serde_json::to_vec_pretty(&pointer)?,
    )?;
    Ok((status, final_dir))
}

fn write_import_dir(
    dir: &Path,
    source_name: &str,
    source: &[u8],
    n: &Normalized,
) -> Result<Vec<u8>> {
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let files: Vec<(&str, Vec<u8>)> = vec![
        (source_name, source.to_vec()),
        ("settings.json", serde_json::to_vec_pretty(&n.settings)?),
        ("catalog.json", serde_json::to_vec_pretty(&n.catalog)?),
        (
            "migration-report.json",
            serde_json::to_vec_pretty(&n.report)?,
        ),
        (
            "migration-report.md",
            render_markdown(&n.report).into_bytes(),
        ),
    ];
    let mut hashes = BTreeMap::new();
    for (name, data) in &files {
        fsutil::write_new_private_file(&dir.join(name), data)?;
        hashes.insert(name.to_string(), sha256_hex(data));
    }
    let manifest = Manifest {
        import_id: n.report.import_id.clone(),
        schema_version: IMPORT_SCHEMA_VERSION,
        snapshot_sha256: n.report.snapshot_sha256.clone(),
        snapshot_bytes: n.report.snapshot_bytes,
        imported_at: n.report.imported_at.clone(),
        observed_at: n.report.observed_at.clone(),
        files: hashes,
    };
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    fsutil::write_new_private_file(&dir.join("manifest.json"), &bytes)?;
    fsutil::fsync_dir(dir)?;
    Ok(bytes)
}

/// Checks every file listed in the manifest against its recorded digest.
pub fn verify_import_dir(dir: &Path) -> Result<Manifest> {
    fsutil::check_private_dir(dir)?;
    let manifest: Manifest = serde_json::from_slice(&fsutil::read_bounded(
        &dir.join("manifest.json"),
        1024 * 1024,
    )?)
    .with_context(|| format!("{}/manifest.json is invalid", dir.display()))?;
    for (name, digest) in &manifest.files {
        if name.contains('/') || name.starts_with('.') {
            bail!("manifest lists an unsafe file name {name:?}");
        }
        let data = fsutil::read_bounded(&dir.join(name), MAX_IMPORT_FILE_BYTES)?;
        if sha256_hex(&data) != *digest {
            bail!(
                "{}/{name} does not match its manifest digest; the import is corrupt or was modified",
                dir.display()
            );
        }
    }
    Ok(manifest)
}

pub struct LoadedImport {
    pub manifest: Manifest,
    pub settings: Settings,
    pub catalog: Catalog,
    pub report: Value,
}

/// Loads and verifies the active import. `Ok(None)` when nothing has been imported yet.
pub fn load_active(state_dir: &Path) -> Result<Option<LoadedImport>> {
    let pointer_path = state_dir.join(ACTIVE_POINTER);
    if !pointer_path.exists() {
        return Ok(None);
    }
    fsutil::check_private_file(&pointer_path)?;
    let pointer: ActivePointer =
        serde_json::from_slice(&fsutil::read_bounded(&pointer_path, 64 * 1024)?)
            .context("active-import.json is invalid")?;
    if !pointer.import_id.starts_with("imp-") || pointer.import_id.contains(['/', '.']) {
        bail!("active-import.json names an invalid import id");
    }
    let dir = state_dir.join("imports").join(&pointer.import_id);
    let manifest_bytes = fsutil::read_bounded(&dir.join("manifest.json"), 1024 * 1024)?;
    if sha256_hex(&manifest_bytes) != pointer.manifest_sha256 {
        bail!("manifest digest does not match active-import.json");
    }
    let manifest = verify_import_dir(&dir)?;
    if manifest.schema_version != IMPORT_SCHEMA_VERSION {
        bail!(
            "import schema {} is not supported by this build (expected {}); re-run import",
            manifest.schema_version,
            IMPORT_SCHEMA_VERSION
        );
    }
    let read = |name: &str| fsutil::read_bounded(&dir.join(name), MAX_IMPORT_FILE_BYTES);
    let settings: Settings =
        serde_json::from_slice(&read("settings.json")?).context("settings.json is invalid")?;
    let catalog: Catalog =
        serde_json::from_slice(&read("catalog.json")?).context("catalog.json is invalid")?;
    let report: Value = serde_json::from_slice(&read("migration-report.json")?)
        .context("migration-report.json is invalid")?;
    Ok(Some(LoadedImport {
        manifest,
        settings,
        catalog,
        report,
    }))
}

#[cfg(test)]
mod source_tests {
    use super::*;

    #[test]
    fn copied_prowlarr_values_keep_public_semantics_and_withhold_the_rest() {
        let v = |x: Value| Some(x);
        assert_eq!(
            source_value("uploader", &v(json!(""))),
            json!(""),
            "empty stays the default"
        );
        assert_eq!(source_value("uploader", &None), Value::Null);
        assert_eq!(
            source_value("apiurl", &v(json!("apibay.org"))),
            json!("apibay.org")
        );
        assert_eq!(
            source_value("definitionFile", &v(json!("thepiratebay"))),
            json!("thepiratebay")
        );
        assert_eq!(source_value("top100", &v(json!(6))), json!(6));
        assert_eq!(
            source_value("baseUrl", &v(json!("https://u:p@thepiratebay.org/x?k=1"))),
            json!("https://thepiratebay.org/x")
        );
        assert_eq!(
            source_value("note", &v(json!("a/b?c=d&e"))),
            json!(WITHHELD)
        );
        assert_eq!(
            source_value("filters", &v(json!({ "a": 1 }))),
            json!(WITHHELD)
        );
        assert_eq!(
            source_value("categories", &v(json!([2000, 5000]))),
            json!([2000, 5000])
        );
        assert_eq!(
            source_value("weird", &v(json!("not a url ://"))),
            json!(WITHHELD)
        );
    }
}
