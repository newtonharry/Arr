//! Release search for a resolved target: query construction, concurrent bounded Prowlarr
//! calls, de-duplication, evaluation and deterministic ranking.

use crate::catalog::CatalogIndex;
use crate::decision::{self, Decision, EvalContext, Rejection, Scope};
use crate::error::{ApiError, ErrorCode};
use crate::import::rfc3339;
use crate::indexer::Backend;
use crate::prowlarr::{ProwlarrRelease, SearchOutcome, SearchRequest};
use crate::release::{self, ParsedRelease};
use crate::settings::{Profile, Settings};
use crate::target::Target;
use crate::title;
use crate::upstream::UpstreamError;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use time::OffsetDateTime;
use unicode_normalization::UnicodeNormalization;

pub const MAX_RELEASES_IN_RESPONSE: usize = 200;
const MAX_TITLE_CHARS: usize = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryView {
    pub query: String,
    pub categories: Vec<u32>,
    pub indexer_ids: Vec<u32>,
    /// Retrieval status: `complete` (all pages fetched), `truncated` (stopped at the configured
    /// page bound with a full last page), `partial` (a later page failed), `failed`, or
    /// `unavailable` (a native indexer the settings name cannot be searched; nothing was
    /// sent). Plans recorded by earlier builds may show `ok`.
    pub status: String,
    pub error: Option<serde_json::Value>,
    pub pages_fetched: u32,
    pub raw_results: usize,
    pub unique_results: usize,
    pub truncated_by_page_limit: bool,
    pub from_cache: bool,
    pub fetched_at: Option<String>,
    /// `native` for a native indexer's answer; absent for Prowlarr (and earlier builds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The native indexer's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer: Option<String>,
    /// Results the indexer returned that were dropped (unrelated category, filters,
    /// unusable fields), counted rather than silently lost.
    #[serde(default)]
    pub dropped_results: usize,
    /// For a native answer without results: `confirmed` (the indexer said "no results") or
    /// `unconfirmed` (nothing came back, without the indexer saying so).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty: Option<String>,
}

pub struct Candidate {
    pub release: ProwlarrRelease,
    pub parsed: ParsedRelease,
    pub decision: Decision,
    pub release_ref: String,
    pub found_by: Vec<usize>,
    pub indexer_priority: i32,
}

pub struct SearchRun {
    pub queries: Vec<QueryView>,
    pub candidates: Vec<Candidate>,
    pub blockers: Vec<Rejection>,
}

/// Prowlarr's search API reports no per-indexer status and can answer HTTP 200 `[]` after an
/// internal indexer failure, so provider-side completeness is never established here.
pub const PROVIDER_COMPLETENESS_NOTE: &str = "Prowlarr GET /api/v1/search reports no per-indexer status and can return HTTP 200 with an empty list after an indexer failure; whether every indexer answered is unverified";
/// Native indexers answer directly, so each one's status is observed.
pub const NATIVE_COMPLETENESS_NOTE: &str = "native indexers were queried directly: each query lists every indexer the settings name with its own status; complete means every one answered in full (an explicit 'no results' counts), never that the indexers hold everything that exists";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retrieval {
    /// Every query returned all pages without error and none stopped at the page bound.
    Complete,
    /// At least one query failed, failed after earlier pages, or stopped at the page bound.
    Incomplete,
    /// Recorded by an earlier build that did not track retrieval completeness.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCompleteness {
    /// Not observable through Prowlarr's search API.
    Unverified,
    /// Native indexers: every indexer the settings name answered every query in full.
    Complete,
    /// Native indexers: at least one was unavailable, failed, stopped at its page bound or
    /// returned nothing without saying so.
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    PageLimit,
    PageFailed,
    QueryFailed,
    /// A native indexer the settings name is not configured or not implemented natively.
    IndexerUnavailable,
    /// A native indexer returned nothing without its explicit "no results" answer.
    EmptyUnconfirmed,
}

impl IssueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueKind::PageLimit => "page_limit",
            IssueKind::PageFailed => "page_failed",
            IssueKind::QueryFailed => "query_failed",
            IssueKind::IndexerUnavailable => "indexer_unavailable",
            IssueKind::EmptyUnconfirmed => "empty_unconfirmed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalIssue {
    pub query: String,
    pub kind: IssueKind,
    pub detail: String,
    /// The native indexer concerned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer: Option<String>,
}

/// Machine-readable search completeness, kept separate for what this server observed
/// (retrieval) and what it cannot observe (provider/indexer coverage).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completeness {
    pub retrieval: Retrieval,
    pub provider: ProviderCompleteness,
    pub issues: Vec<RetrievalIssue>,
    pub note: String,
}

impl Completeness {
    pub fn of(queries: &[QueryView]) -> Self {
        let native = !queries.is_empty()
            && queries
                .iter()
                .all(|q| q.provider.as_deref() == Some("native"));
        let issues: Vec<RetrievalIssue> = queries
            .iter()
            .filter_map(|q| {
                let error = || {
                    q.error
                        .as_ref()
                        .and_then(|e| e["message"].as_str())
                        .unwrap_or("unknown failure")
                        .to_string()
                };
                let (kind, detail) = match q.status.as_str() {
                    "complete" if q.empty.as_deref() == Some("unconfirmed") => (
                        IssueKind::EmptyUnconfirmed,
                        "the indexer returned nothing without its explicit 'no results' answer; an indexer-side failure cannot be ruled out".to_string(),
                    ),
                    "complete" => return None,
                    "unavailable" => (IssueKind::IndexerUnavailable, error()),
                    "truncated" => (
                        IssueKind::PageLimit,
                        format!(
                            "stopped at the configured page bound after {} full page(s); more results may exist",
                            q.pages_fetched
                        ),
                    ),
                    "partial" => (IssueKind::PageFailed, error()),
                    _ => (IssueKind::QueryFailed, error()),
                };
                Some(RetrievalIssue {
                    query: q.query.clone(),
                    kind,
                    detail,
                    indexer: q.indexer.clone(),
                })
            })
            .collect();
        let retrieval_issue = issues.iter().any(|i| i.kind != IssueKind::EmptyUnconfirmed);
        Completeness {
            retrieval: if retrieval_issue {
                Retrieval::Incomplete
            } else {
                Retrieval::Complete
            },
            provider: match (native, issues.is_empty()) {
                (false, _) => ProviderCompleteness::Unverified,
                (true, true) => ProviderCompleteness::Complete,
                (true, false) => ProviderCompleteness::Incomplete,
            },
            issues,
            note: if native {
                NATIVE_COMPLETENESS_NOTE.into()
            } else {
                PROVIDER_COMPLETENESS_NOTE.into()
            },
        }
    }

    /// For plans stored by builds that predate completeness tracking.
    pub fn legacy() -> Self {
        Completeness {
            retrieval: Retrieval::Unknown,
            provider: ProviderCompleteness::Unverified,
            issues: vec![],
            note: "recorded by an earlier build that did not track search completeness".into(),
        }
    }
}

impl SearchRun {
    pub fn completeness(&self) -> Completeness {
        Completeness::of(&self.queries)
    }

    pub fn rejection_counts(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for c in &self.candidates {
            for r in &c.decision.rejections {
                *m.entry(r.code.clone()).or_default() += 1;
            }
        }
        m
    }
}

/// Opaque, stable reference; never reversible to a GUID, hash or URL.
pub fn release_ref(r: &ProwlarrRelease) -> String {
    let mut h = Sha256::new();
    h.update(b"unified-media/release-ref/v1\0");
    h.update(r.dedupe_key().as_bytes());
    format!("rel_{}", &hex::encode(h.finalize())[..24])
}

/// Search text for a title: no "(YYYY)" suffix, apostrophes and brackets removed, other
/// separators (`:` `,` `;` `!` `?` `/`) turned into spaces, bounded length.
fn query_title(t: &str) -> String {
    let (base, _) = title::split_trailing_year(t);
    base.chars()
        .filter(|c| !matches!(c, '\'' | '\u{2019}' | '(' | ')' | '[' | ']'))
        .map(|c| {
            if c.is_control() || matches!(c, ':' | ',' | ';' | '!' | '?' | '/') {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(150)
        .collect()
}

/// Case-preserving ASCII transliteration ("Amélie" -> "Amelie"); non-Latin text is dropped.
fn ascii_fold(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
            continue;
        }
        let decomposed: String = c.nfkd().filter(char::is_ascii).collect();
        if decomposed.is_empty() {
            out.extend(
                title::fold_chars(&c.to_string())
                    .chars()
                    .filter(char::is_ascii_alphanumeric),
            );
        } else {
            out.push_str(&decomposed);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One query per target (plus an ASCII-folded variant for non-ASCII titles).
///
/// A native "(YYYY)" disambiguator ("WAR (2026)", "Doctor Who (2005)") stays in TV queries as
/// a bare year; ordinary series titles get no year. Movie queries carry the movie year once.
pub fn queries_for(target: &Target, scope: Scope) -> Vec<String> {
    let (title, suffix) = match target {
        Target::Movie(m) => {
            let year = m.year.or(title::split_trailing_year(&m.title).1);
            (
                query_title(&m.title),
                year.map(|y| y.to_string()).unwrap_or_default(),
            )
        }
        Target::Season(s) | Target::Episode { season: s, .. } => {
            let numbering = match scope {
                Scope::Episode(season, ep) => format!("S{season:02}E{ep:02}"),
                _ => format!("S{:02}", s.season),
            };
            let suffix = match title::split_trailing_year(&s.series.title).1 {
                Some(year) => format!("{year} {numbering}"),
                None => numbering,
            };
            (query_title(&s.series.title), suffix)
        }
    };
    let mut out = vec![format!("{title} {suffix}").trim().to_string()];
    if !title.is_ascii() {
        let folded = ascii_fold(&title);
        if !folded.is_empty() && folded != title {
            out.push(format!("{folded} {suffix}").trim().to_string());
        }
    }
    out
}

pub struct SearchInputs<'a> {
    /// The release search backend (native indexers or Prowlarr); `None` when neither is
    /// configured.
    pub backend: Option<&'a Backend>,
    pub settings: &'a Settings,
    pub catalog: Option<&'a CatalogIndex>,
    pub catalog_observed_at: Option<String>,
    pub now: OffsetDateTime,
    /// Automatic (plan) or interactive (search endpoint) indexer selection.
    pub automatic: bool,
    pub refresh: bool,
    /// Blocklisted release refs and `hash:<info hash>` entries for the target's media.
    pub blocked: Option<&'a std::collections::HashSet<String>>,
}

/// Chooses the quality profile: the catalog item's own, an explicit one for items outside
/// the catalog, or the only profile of that media kind.
pub fn profile_for<'a>(
    settings: &'a Settings,
    target: &Target,
    requested: Option<&str>,
) -> Result<(Option<&'a Profile>, Option<String>), ApiError> {
    let id = match (requested, target.quality_profile()) {
        (Some(r), Some(own)) if r != own => {
            return Err(ApiError::bad_request(
                ErrorCode::InvalidRequest,
                format!(
                    "catalog item uses profile {own}; a different profile ({r}) cannot be applied to it"
                ),
            ));
        }
        (_, Some(own)) => own.to_string(),
        (Some(r), None) => match settings.profile(r) {
            Some(p) if p.media == target.media() => r.to_string(),
            _ => {
                return Err(ApiError::bad_request(
                    ErrorCode::InvalidRequest,
                    format!("unknown {:?} quality_profile {r}", target.media()),
                ));
            }
        },
        (None, None) => match settings.sole_profile(target.media()) {
            Some(p) => p.id.clone(),
            None => {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    ErrorCode::ProfileRequired,
                    "target is not in the catalog and several (or no) profiles exist for its media kind; pass quality_profile",
                ));
            }
        },
    };
    Ok((settings.profile(&id), Some(id)))
}

/// Runs `scopes` (each producing its own queries) and evaluates every unique release
/// against `eval_scope`. Fails only if every query failed.
pub async fn run(
    inputs: &SearchInputs<'_>,
    target: &Target,
    profile: (Option<&Profile>, Option<String>),
    query_scopes: &[Scope],
    eval_scope: Scope,
) -> Result<SearchRun, ApiError> {
    let media = target.media();
    let backend = inputs.backend.ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::ProviderNotConfigured,
            "no release search backend is configured: add [[indexer]] entries (native indexers) or a [prowlarr] section to config.toml",
        )
    })?;
    let indexers = inputs.settings.search_indexers(media, inputs.automatic);
    let mode = if inputs.automatic {
        "automatic"
    } else {
        "interactive"
    };
    if indexers.is_empty() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::NoUsableIndexers,
            format!(
                "no {} indexer in the active settings has {mode} search enabled",
                media.label(),
            ),
        ));
    }
    let mut texts: Vec<String> = vec![];
    for scope in query_scopes {
        for q in queries_for(target, *scope) {
            if !texts.contains(&q) {
                texts.push(q);
            }
        }
    }

    // One request per (query, indexer) for native indexers, each with that indexer's own
    // categories; one request per query for Prowlarr, which fans out itself.
    struct Job {
        req: SearchRequest,
        provider: Option<String>,
        indexer: Option<String>,
        label: String,
    }
    let mut jobs: Vec<Job> = vec![];
    let mut unavailable: Vec<(u32, String, String)> = vec![];
    match backend {
        Backend::Prowlarr(_) => {
            let indexer_ids: Vec<u32> = indexers.iter().map(|i| i.prowlarr_indexer_id).collect();
            let mut categories: Vec<u32> = indexers
                .iter()
                .flat_map(|i| i.categories.iter().copied())
                .collect();
            categories.sort_unstable();
            categories.dedup();
            for q in &texts {
                jobs.push(Job {
                    req: SearchRequest {
                        query: q.clone(),
                        categories: categories.clone(),
                        indexer_ids: indexer_ids.clone(),
                    },
                    provider: None,
                    indexer: None,
                    label: "prowlarr".into(),
                });
            }
        }
        Backend::Native(registry) => {
            let mut usable: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
            for ix in &indexers {
                match registry.unusable(ix) {
                    None => usable
                        .entry(ix.prowlarr_indexer_id)
                        .or_default()
                        .extend(ix.categories.iter().copied()),
                    Some((_, why)) => {
                        if !unavailable.iter().any(|u| u.0 == ix.prowlarr_indexer_id) {
                            unavailable.push((ix.prowlarr_indexer_id, ix.name.clone(), why));
                        }
                    }
                }
            }
            if usable.is_empty() {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::NoUsableIndexers,
                    format!(
                        "no {} indexer with {mode} search enabled can be searched natively: {}",
                        media.label(),
                        unavailable
                            .iter()
                            .map(|u| u.2.clone())
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                ));
            }
            for q in &texts {
                for (id, cats) in &usable {
                    let mut cats = cats.clone();
                    cats.sort_unstable();
                    cats.dedup();
                    let ix = registry.get(*id).expect("usable indexers are configured");
                    jobs.push(Job {
                        req: SearchRequest {
                            query: q.clone(),
                            categories: cats,
                            indexer_ids: vec![*id],
                        },
                        provider: Some("native".into()),
                        indexer: Some(ix.cfg.name.clone()),
                        label: ix.label(),
                    });
                }
            }
        }
    }

    let mut set = tokio::task::JoinSet::new();
    for (i, job) in jobs.iter().enumerate() {
        let (req, refresh, backend) = (job.req.clone(), inputs.refresh, backend.clone());
        set.spawn(async move {
            let r: Result<SearchOutcome, UpstreamError> = match &backend {
                Backend::Prowlarr(p) => p.search(&req, refresh).await,
                Backend::Native(reg) => match reg.get(req.indexer_ids[0]) {
                    Some(ix) => ix.search(&req, refresh).await,
                    None => Err(UpstreamError::Unavailable {
                        reason: "indexer not configured".into(),
                    }),
                },
            };
            (i, r)
        });
    }
    let mut outcomes = vec![None; jobs.len()];
    while let Some(joined) = set.join_next().await {
        let (i, r) = joined.map_err(|e| ApiError::internal("search task", e))?;
        outcomes[i] = Some(r);
    }

    let mut queries = vec![];
    let mut first_error: Option<(UpstreamError, String)> = None;
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut found: Vec<(ProwlarrRelease, Vec<usize>)> = vec![];
    for (i, (job, outcome)) in jobs.iter().zip(outcomes).enumerate() {
        let (req, label) = (&job.req, &job.label);
        let outcome = outcome.expect("every task reports");
        let mut view = QueryView {
            query: req.query.clone(),
            categories: req.categories.clone(),
            indexer_ids: req.indexer_ids.clone(),
            status: "complete".into(),
            error: None,
            pages_fetched: 0,
            raw_results: 0,
            unique_results: 0,
            truncated_by_page_limit: false,
            from_cache: false,
            fetched_at: None,
            provider: job.provider.clone(),
            indexer: job.indexer.clone(),
            dropped_results: 0,
            empty: None,
        };
        match outcome {
            Err(e) => {
                tracing::warn!(provider = %label, query = %req.query, failure = %e.describe(), "search query failed");
                view.status = "failed".into();
                view.error = Some(
                    serde_json::json!({ "code": e.code(), "message": format!("{label} {}", e.describe()) }),
                );
                first_error.get_or_insert((e, label.clone()));
            }
            Ok(o) => {
                if let Some(e) = &o.partial_error {
                    tracing::warn!(provider = %label, query = %req.query, failure = %e.describe(), pages = o.pages_fetched, "search query partially failed");
                    view.status = "partial".into();
                    view.error = Some(
                        serde_json::json!({ "code": e.code(), "message": format!("{label} {} (after {} page(s))", e.describe(), o.pages_fetched) }),
                    );
                }
                view.pages_fetched = o.pages_fetched;
                view.raw_results = o.raw_count;
                view.unique_results = o.releases.len();
                view.truncated_by_page_limit = o.truncated_by_page_limit;
                if o.partial_error.is_none() && o.truncated_by_page_limit {
                    view.status = "truncated".into();
                }
                view.from_cache = o.from_cache;
                view.fetched_at = Some(rfc3339(o.fetched_at));
                view.dropped_results = o.dropped;
                if job.provider.is_some() && o.raw_count == 0 {
                    view.empty = Some(
                        if o.confirmed_empty {
                            "confirmed"
                        } else {
                            "unconfirmed"
                        }
                        .into(),
                    );
                }
                for r in o.releases.iter() {
                    let k = r.dedupe_key();
                    match by_key.get(&k) {
                        Some(&j) => found[j].1.push(i),
                        None => {
                            by_key.insert(k, found.len());
                            found.push((r.clone(), vec![i]));
                        }
                    }
                }
            }
        }
        queries.push(view);
    }
    for q in &texts {
        for (id, name, why) in &unavailable {
            queries.push(QueryView {
                query: q.clone(),
                categories: vec![],
                indexer_ids: vec![*id],
                status: "unavailable".into(),
                error: Some(
                    serde_json::json!({ "code": ErrorCode::NoUsableIndexers, "message": why }),
                ),
                pages_fetched: 0,
                raw_results: 0,
                unique_results: 0,
                truncated_by_page_limit: false,
                from_cache: false,
                fetched_at: None,
                provider: Some("native".into()),
                indexer: Some(name.clone()),
                dropped_results: 0,
                empty: None,
            });
        }
    }
    if let Some((e, label)) = first_error
        && queries
            .iter()
            .all(|q| matches!(q.status.as_str(), "failed" | "unavailable"))
    {
        return Err(e.into_api(&label));
    }

    let native = matches!(backend, Backend::Native(_));
    let ctx = EvalContext {
        target,
        scope: eval_scope,
        settings: inputs.settings,
        profile: profile.0,
        profile_id: profile.1,
        catalog: inputs.catalog,
        now: inputs.now,
        catalog_observed_at: inputs.catalog_observed_at.clone(),
        blocked: inputs.blocked,
    };
    let blockers = ctx.target_blockers();
    let mut candidates: Vec<Candidate> = found
        .into_iter()
        .map(|(release, found_by)| {
            let parsed = release::parse(release.title.as_deref().unwrap_or(""));
            let mut decision = decision::evaluate(&ctx, &release, &parsed, &blockers);
            apply_seed_requirement(
                &mut decision,
                &release,
                inputs.settings,
                media,
                native,
                &parsed,
            );
            let indexer_priority = release
                .indexer_id
                .and_then(|i| u32::try_from(i).ok())
                .and_then(|i| inputs.settings.indexer(media, i))
                .map(|i| i.priority)
                .unwrap_or(i32::MAX);
            Candidate {
                release_ref: release_ref(&release),
                release,
                parsed,
                decision,
                found_by,
                indexer_priority,
            }
        })
        .collect();
    let prefer_revision = inputs
        .settings
        .policy(media)
        .is_none_or(|p| p.prefer_propers_and_repacks);
    rank(&mut candidates, prefer_revision);
    Ok(SearchRun {
        queries,
        candidates,
        blockers,
    })
}

/// Rejects a release whose tracker seed requirement the indexer's seed goal cannot meet.
pub fn apply_seed_requirement(
    decision: &mut Decision,
    release: &ProwlarrRelease,
    settings: &Settings,
    media: crate::settings::MediaKind,
    native: bool,
    parsed: &ParsedRelease,
) {
    let Some(ix) = release
        .indexer_id
        .and_then(|i| u32::try_from(i).ok())
        .and_then(|i| settings.indexer(media, i))
    else {
        return;
    };
    let (ratio, time, pack) = crate::indexer::seed_goals(ix, native);
    let pack_release = matches!(parsed.numbering, release::Numbering::Season { .. });
    let minutes = if pack_release { pack.or(time) } else { time };
    if let Some(r) = decision::seed_requirement(release, ratio, minutes) {
        decision.approved = false;
        decision.rejections.push(r);
    }
}

/// Deterministic preference order (approved first; tier; revision when preferred; coverage;
/// certainty; indexer priority; seeders; preferred size; release ref).
pub fn compare(a: &Candidate, b: &Candidate, prefer_revision: bool) -> std::cmp::Ordering {
    let (da, db) = (&a.decision, &b.decision);
    db.approved
        .cmp(&da.approved)
        .then(db.tier.cmp(&da.tier))
        .then(if prefer_revision {
            b.parsed.revision.cmp(&a.parsed.revision)
        } else {
            std::cmp::Ordering::Equal
        })
        .then(db.covered_episodes.len().cmp(&da.covered_episodes.len()))
        .then(da.uncertainty.cmp(&db.uncertainty))
        .then(a.indexer_priority.cmp(&b.indexer_priority))
        .then(
            b.release
                .seeders
                .unwrap_or(-1)
                .cmp(&a.release.seeders.unwrap_or(-1)),
        )
        .then(da.preferred_distance.cmp(&db.preferred_distance))
        .then(a.release_ref.cmp(&b.release_ref))
}

pub fn rank(candidates: &mut [Candidate], prefer_revision: bool) {
    candidates.sort_by(|a, b| compare(a, b, prefer_revision));
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateSummary {
    pub release_ref: String,
    pub title: String,
    pub indexer_id: Option<i64>,
    pub indexer: Option<String>,
    pub size_bytes: Option<i64>,
    pub seeders: Option<i64>,
    pub quality: String,
    pub revision: crate::release::Revision,
    pub published_at: Option<String>,
    pub covers_episodes: Vec<u16>,
    pub full_season: bool,
}

fn clip(s: &str) -> String {
    s.chars().take(MAX_TITLE_CHARS).collect()
}

pub fn summary(c: &Candidate) -> CandidateSummary {
    CandidateSummary {
        release_ref: c.release_ref.clone(),
        title: clip(c.release.title.as_deref().unwrap_or("")),
        indexer_id: c.release.indexer_id,
        indexer: c.release.indexer.as_deref().map(clip),
        size_bytes: c.release.size,
        seeders: c.release.seeders,
        quality: c.parsed.quality.to_string(),
        revision: c.parsed.revision,
        published_at: c.release.publish_date.clone(),
        covers_episodes: c.decision.covered_episodes.clone(),
        full_season: c.decision.full_season,
    }
}

/// The query texts that found a candidate (each once; native views repeat a text per indexer).
fn found_by(c: &Candidate, queries: &[QueryView]) -> Vec<String> {
    let mut by: Vec<String> = vec![];
    for q in c.found_by.iter().filter_map(|i| queries.get(*i)) {
        if !by.contains(&q.query) {
            by.push(q.query.clone());
        }
    }
    by
}

/// Full per-release view for the search endpoint. Contains no URLs, GUIDs or hashes.
pub fn view(c: &Candidate, rank: usize, queries: &[QueryView]) -> serde_json::Value {
    let p = &c.parsed;
    serde_json::json!({
        "rank": rank,
        "release_ref": c.release_ref,
        "title": clip(c.release.title.as_deref().unwrap_or("")),
        "indexer": { "id": c.release.indexer_id, "name": c.release.indexer.as_deref().map(clip) },
        "protocol": c.release.protocol,
        "size_bytes": c.release.size,
        "seeders": c.release.seeders,
        "leechers": c.release.leechers,
        "published_at": c.release.publish_date,
        "categories": c.release.categories.iter().flatten().filter_map(|x| x.id).collect::<Vec<_>>(),
        "tracker": {
            "minimum_ratio": c.release.minimum_ratio,
            "minimum_seed_time_secs": c.release.minimum_seed_time,
            "download_volume_factor": c.release.download_volume_factor,
            "upload_volume_factor": c.release.upload_volume_factor,
        },
        "found_by": found_by(c, queries),
        "parsed": {
            "title": p.title_tokens.join(" "),
            "year": p.year,
            "numbering": p.numbering,
            "quality": p.quality,
            "quality_basis": p.quality_basis,
            "revision": p.revision,
            "languages": p.languages,
            "multi_language": p.multi_language,
        },
        "decision": c.decision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{MediaId, SeriesKey};
    use crate::target::{MovieTarget, Ownership, SeasonTarget, SeriesTarget, Source};

    #[test]
    fn release_refs_are_stable_across_versions() {
        // Persisted in plans, grabs and the blocklist: changing the derivation orphans them.
        let r: ProwlarrRelease =
            serde_json::from_value(serde_json::json!({ "infoHash": "a".repeat(40) })).unwrap();
        assert_eq!(release_ref(&r), "rel_bbca78a99506a586158db3db");
    }

    fn season_target(title: &str) -> Target {
        Target::Season(SeasonTarget {
            series: SeriesTarget {
                id: MediaId::Series(SeriesKey::Tvdb(1)),
                title: title.into(),
                alternate_titles: vec![],
                year: None,
                runtime_minutes: None,
                original_language: None,
                series_type: "standard".into(),
                scene_numbering: false,
                quality_profile: None,
                root_folder: None,
                identity_conflict: false,
                metadata_source: Source::Catalog,
                in_catalog: true,
                tvdb_id: Some(1),
                tvmaze_id: None,
            },
            season: 1,
            episodes: None,
            episode_list_source: None,
            ownership_source: String::new(),
            notes: vec![],
        })
    }

    #[test]
    fn query_text_is_cleaned_and_bounded() {
        let q = |t: &str, scope| queries_for(&season_target(t), scope);
        assert_eq!(
            q("The Office (US)", Scope::Season(1)),
            ["The Office US S01"]
        );
        assert_eq!(
            q("Doctor Who (2005)", Scope::Season(1)),
            ["Doctor Who 2005 S01"]
        );
        assert_eq!(q("WAR (2026)", Scope::Season(1)), ["WAR 2026 S01"]);
        assert_eq!(q("WAR (2026)", Scope::Episode(1, 2)), ["WAR 2026 S01E02"]);
        assert_eq!(q("Slow Horses", Scope::Season(1)), ["Slow Horses S01"]);
        assert_eq!(
            q("Slow Horses", Scope::Episode(1, 4)),
            ["Slow Horses S01E04"]
        );
        assert_eq!(
            q("Star Wars: Andor", Scope::Episode(1, 4)),
            ["Star Wars Andor S01E04"]
        );
        assert_eq!(
            q("Marvel's Agents of S.H.I.E.L.D.", Scope::Season(1)),
            ["Marvels Agents of S.H.I.E.L.D. S01"]
        );
        assert_eq!(
            q("Pokémon", Scope::Season(1)),
            ["Pokémon S01", "Pokemon S01"]
        );
        assert_eq!(
            q("Слово пацана", Scope::Season(1)),
            ["Слово пацана S01"],
            "no empty transliteration variant"
        );
        assert!(q(&"x".repeat(500), Scope::Season(1))[0].len() <= 160);
        let movie = Target::Movie(MovieTarget {
            id: MediaId::Movie { tmdb: 603 },
            title: "The Matrix".into(),
            alternate_titles: vec![],
            year: Some(1999),
            secondary_year: None,
            runtime_minutes: None,
            original_language: None,
            is_available: None,
            ownership: Ownership::Missing,
            quality_profile: None,
            root_folder: None,
            identity_conflict: false,
            metadata_source: Source::Catalog,
            in_catalog: true,
            existing: None,
            queued: false,
            folder: None,
            availability_note: None,
        });
        assert_eq!(queries_for(&movie, Scope::Movie), ["The Matrix 1999"]);
        let mut dune = movie.clone();
        if let Target::Movie(m) = &mut dune {
            m.title = "Dune (2021)".into();
            m.year = Some(2021);
        }
        assert_eq!(
            queries_for(&dune, Scope::Movie),
            ["Dune 2021"],
            "year said once"
        );
    }
}
