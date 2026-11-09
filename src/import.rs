//! The versioned import store: immutable, digest-verified state directories (`imports/<id>/`)
//! plus the `active-import.json` pointer. Managed setup and the Plex bootstrap write them
//! (`write_and_activate`), and startup loads the active one (`load_active`). Also home to the
//! small shared helpers (`rfc3339`, `sha256_hex`, `WITHHELD`).
//!
//! The name is historical: this module used to hold the Sonarr/Radarr snapshot importer, which
//! has been removed. The on-disk layout is unchanged so existing state keeps loading (a state
//! made by the old importer is recognised and refused at startup, see `main.rs`).

use crate::catalog::Catalog;
use crate::fsutil;
use crate::settings::*;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const IMPORT_SCHEMA_VERSION: u32 = 1;
pub const MAX_IMPORT_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const ACTIVE_POINTER: &str = "active-import.json";

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

pub struct Normalized {
    pub settings: Settings,
    pub catalog: Catalog,
    pub report: MigrationReport,
}

/// Marker for a copied value that is not kept verbatim. It is never a default, so a native
/// provider that does not know the field refuses it (fail closed).
pub const WITHHELD: &str = "<withheld>";

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
    #[cfg_attr(not(unix), allow(unused_mut))]
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
