//! Plans: what to grab for a movie, season or episode, built from approved candidates only.
//! A plan is a shadow plan unless it is executed (`POST /plans {execute: true}` or
//! `POST /plans/{id}/execute`, only with `[execution] enabled`): then every planned candidate
//! goes through the download outbox and the plan records its grabs (`download_started`, then
//! `imported` once they are all imported).

use crate::decision::{Health, Rejection, Scope};
use crate::downloads::ReleaseLink;
use crate::error::{ApiError, ErrorCode};
use crate::fsutil;
use crate::ids::MediaId;
use crate::import::rfc3339;
use crate::search::{
    self, Candidate, CandidateSummary, Completeness, QueryView, Retrieval, SearchInputs, SearchRun,
};
use crate::settings::Profile;
use crate::target::{Ownership, SeasonTarget, Target};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Upper bound on per-episode queries in one season plan.
pub const MAX_EPISODE_QUERIES: usize = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    /// Every wanted unit has an approved candidate. Nothing has been submitted.
    ShadowPlanned,
    /// Some wanted units have approved candidates, others do not.
    ShadowPartial,
    /// Nothing can be planned (policy unsupported, no approved release, metadata missing...).
    Blocked,
    /// Everything wanted is already owned per the catalog observation.
    AlreadySatisfied,
    Cancelled,
    Superseded,
    /// Executed: grabs are in the download outbox, not yet confirmed by qBittorrent.
    DownloadQueued,
    /// Executed: qBittorrent confirmed the grabs.
    DownloadStarted,
    /// Executed, but every grab failed or was cancelled before qBittorrent confirmed it.
    DownloadFailed,
    /// Executed and every grab imported.
    Imported,
    /// Nothing left to plan: every wanted unit is owned or already being downloaded.
    InProgress,
}

pub const ALL_PLAN_STATES: &[PlanState] = &[
    PlanState::ShadowPlanned,
    PlanState::ShadowPartial,
    PlanState::Blocked,
    PlanState::AlreadySatisfied,
    PlanState::Cancelled,
    PlanState::Superseded,
    PlanState::DownloadStarted,
    PlanState::Imported,
    PlanState::InProgress,
    PlanState::DownloadQueued,
    PlanState::DownloadFailed,
];

impl PlanState {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanState::ShadowPlanned => "shadow_planned",
            PlanState::ShadowPartial => "shadow_partial",
            PlanState::Blocked => "blocked",
            PlanState::AlreadySatisfied => "already_satisfied",
            PlanState::Cancelled => "cancelled",
            PlanState::Superseded => "superseded",
            PlanState::DownloadStarted => "download_started",
            PlanState::Imported => "imported",
            PlanState::InProgress => "in_progress",
            PlanState::DownloadQueued => "download_queued",
            PlanState::DownloadFailed => "download_failed",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            PlanState::ShadowPlanned
                | PlanState::ShadowPartial
                | PlanState::Blocked
                | PlanState::AlreadySatisfied
                | PlanState::InProgress
        )
    }

    pub fn parse(s: &str) -> Option<Self> {
        ALL_PLAN_STATES.iter().copied().find(|p| p.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    Planned,
    Owned,
    NotAired,
    /// Releases covering the unit were observed, but none was approved.
    NoApprovedRelease,
    /// No release covering the unit was observed. Not proof of absence: Prowlarr's
    /// provider/indexer completeness is unverified.
    NoReleaseObserved,
    SearchFailed,
    NotSearched,
    OwnershipUnknown,
    /// Covered by an active download.
    Queued,
}

pub const ALL_UNIT_STATUSES: &[UnitStatus] = &[
    UnitStatus::Planned,
    UnitStatus::Owned,
    UnitStatus::NotAired,
    UnitStatus::NoApprovedRelease,
    UnitStatus::NoReleaseObserved,
    UnitStatus::SearchFailed,
    UnitStatus::NotSearched,
    UnitStatus::OwnershipUnknown,
    UnitStatus::Queued,
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanUnit {
    pub target: MediaId,
    pub label: String,
    pub status: UnitStatus,
    pub candidate: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    /// `shadow` or `executed`.
    pub mode: String,
    /// `disabled` (`[execution] enabled = false`), `enabled`, or `not_implemented` in plans
    /// recorded by earlier builds.
    pub download_submission: String,
    pub submitted: bool,
    pub note: String,
    /// Grab ids created by executing the plan. `submitted` turns true only once qBittorrent
    /// has confirmed them (not when they enter the outbox).
    #[serde(default)]
    pub grabs: Vec<String>,
    /// Candidates the outbox refused when the plan was executed.
    #[serde(default)]
    pub refused: Vec<Rejection>,
    #[serde(default)]
    pub executed_at: Option<String>,
    /// When qBittorrent first confirmed a grab of this plan.
    #[serde(default)]
    pub confirmed_at: Option<String>,
    /// What executing the plan did: `queued` (every candidate entered the outbox),
    /// `partially_queued` (some were refused) or `nothing_queued` (all were refused; the plan
    /// keeps its shadow state and can be executed again). Absent for shadow plans and for
    /// plans executed by earlier builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ExecutionOutcome>,
    /// Grab ids that a season pack of this plan took over (cancelled, not blocklisted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub superseded_grabs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Queued,
    PartiallyQueued,
    NothingQueued,
}

impl Execution {
    pub fn shadow(enabled: bool) -> Self {
        Execution {
            mode: "shadow".into(),
            download_submission: if enabled { "enabled" } else { "disabled" }.into(),
            submitted: false,
            note: if enabled {
                "shadow plan: nothing submitted; execute it with POST /api/v1/plans/{id}/execute"
                    .into()
            } else {
                "shadow plan only: [execution] enabled = false".into()
            },
            grabs: vec![],
            refused: vec![],
            executed_at: None,
            confirmed_at: None,
            outcome: None,
            superseded_grabs: vec![],
        }
    }

    /// Executed, and not a single candidate entered the outbox.
    pub fn nothing_queued(&self) -> bool {
        self.outcome == Some(ExecutionOutcome::NothingQueued)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSummary {
    pub queries: Vec<QueryView>,
    /// Absent in plans stored by earlier builds, which read back as `retrieval: unknown`.
    #[serde(default = "Completeness::legacy")]
    pub completeness: Completeness,
    pub releases_considered: usize,
    pub approved: usize,
    pub rejection_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    /// Canonical target (catalog identity when known).
    pub target: MediaId,
    pub requested_target: MediaId,
    pub title: String,
    pub state: PlanState,
    pub execution: Execution,
    pub created_at: String,
    pub updated_at: String,
    pub import_id: Option<String>,
    pub catalog_observed_at: Option<String>,
    pub profile: Option<String>,
    pub destination_root: Option<String>,
    pub selection: Option<String>,
    pub units: Vec<PlanUnit>,
    pub candidates: Vec<CandidateSummary>,
    pub blockers: Vec<Rejection>,
    pub search: Option<SearchSummary>,
    pub notes: Vec<String>,
    pub superseded_by: Option<String>,
}

fn blocker(code: &str, message: impl Into<String>) -> Rejection {
    Rejection {
        code: code.into(),
        message: message.into(),
    }
}

/// Plan-level blocker codes in addition to `decision::REJECTION_CODES`.
pub const PLAN_BLOCKER_CODES: &[&str] = &[
    "no_approved_release",
    "no_release_observed",
    "search_failed",
    "not_searched",
];

const NOTHING_OBSERVED: &str = "no release was observed; not proof of absence (see search.completeness: through Prowlarr an indexer failure can look like an empty result; native indexers report their own status)";

/// Plan note when retrieval was incomplete (failed, partial or page-bounded queries).
fn retrieval_note(c: &Completeness) -> Option<String> {
    (c.retrieval == Retrieval::Incomplete).then(|| {
        let issues: Vec<String> = c
            .issues
            .iter()
            .map(|i| format!("'{}' {}: {}", i.query, i.kind.as_str(), i.detail))
            .collect();
        format!(
            "search retrieval incomplete; results may be missing: {}",
            issues.join("; ")
        )
    })
}

pub fn canonical_id(t: &Target) -> MediaId {
    match t {
        Target::Movie(m) => m.id,
        Target::Season(s) => {
            MediaId::Season(s.series.id.series_key().expect("series id"), s.season)
        }
        Target::Episode { season, episode } => MediaId::Episode(
            season.series.id.series_key().expect("series id"),
            season.season,
            *episode,
        ),
    }
}

fn display_title(t: &Target) -> String {
    match t {
        Target::Movie(m) => match m.year {
            Some(y) => format!("{} ({y})", m.title),
            None => m.title.clone(),
        },
        Target::Season(s) => format!("{} S{:02}", s.series.title, s.season),
        Target::Episode { season, episode } => format!(
            "{} S{:02}E{:02}",
            season.series.title, season.season, episode
        ),
    }
}

fn summarize(runs: &[&SearchRun]) -> SearchSummary {
    let mut seen = BTreeSet::new();
    let mut counts = BTreeMap::new();
    let mut approved = 0;
    for run in runs {
        for c in &run.candidates {
            if seen.insert(c.release_ref.clone()) {
                approved += usize::from(c.decision.approved);
                for r in &c.decision.rejections {
                    *counts.entry(r.code.clone()).or_default() += 1;
                }
            }
        }
    }
    SearchSummary {
        queries: runs.iter().flat_map(|r| r.queries.clone()).collect(),
        completeness: Completeness::of(
            &runs
                .iter()
                .flat_map(|r| r.queries.iter().cloned())
                .collect::<Vec<_>>(),
        ),
        releases_considered: seen.len(),
        approved,
        rejection_counts: counts,
    }
}

fn top_rejections(run: &SearchRun) -> String {
    let counts = run.rejection_counts();
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let parts: Vec<String> = v.iter().take(4).map(|(k, n)| format!("{k} x{n}")).collect();
    if parts.is_empty() {
        "no releases found".into()
    } else {
        format!(
            "{} release(s) considered, none approved ({})",
            run.candidates.len(),
            parts.join(", ")
        )
    }
}

pub struct PlanInputs<'a> {
    pub search: SearchInputs<'a>,
    pub import_id: Option<String>,
    /// Also search owned units for native revision upgrades (exact quality, higher revision).
    pub revisions: bool,
    /// `[execution] enabled` (recorded in the plan).
    pub execution_enabled: bool,
    /// Per-episode queries stop at this instant; unfinished units become `search_failed`
    /// so a partial plan is still recorded instead of the whole request timing out.
    pub episode_deadline: tokio::time::Instant,
}

/// A built plan plus the private link material of its chosen candidates (persisted to
/// `release_links`, never serialised with the plan).
pub struct Built {
    pub plan: Plan,
    pub links: Vec<ReleaseLink>,
}

pub async fn build(
    inputs: &PlanInputs<'_>,
    target: &Target,
    requested: MediaId,
    profile: (Option<&Profile>, Option<String>),
) -> Result<Built, ApiError> {
    let now = rfc3339(inputs.search.now);
    let mut plan = Plan {
        id: format!(
            "pln_{}",
            fsutil::random_hex(12).map_err(|e| ApiError::internal("plan id", e))?
        ),
        target: canonical_id(target),
        requested_target: requested,
        title: display_title(target),
        state: PlanState::Blocked,
        execution: Execution::shadow(inputs.execution_enabled),
        created_at: now.clone(),
        updated_at: now,
        import_id: inputs.import_id.clone(),
        catalog_observed_at: inputs.search.catalog_observed_at.clone(),
        profile: profile.1.clone(),
        destination_root: target.root_folder().map(str::to_owned),
        selection: None,
        units: vec![],
        candidates: vec![],
        blockers: vec![],
        search: None,
        notes: vec![],
        superseded_by: None,
    };
    let mut links = vec![];
    let ctx = crate::decision::EvalContext {
        target,
        scope: Scope::Movie,
        settings: inputs.search.settings,
        profile: profile.0,
        profile_id: profile.1.clone(),
        catalog: inputs.search.catalog,
        now: inputs.search.now,
        catalog_observed_at: None,
        blocked: None,
        partial_packs: inputs.search.partial_packs,
    };
    let target_blockers = ctx.target_blockers();
    if !target_blockers.is_empty() {
        plan.blockers = target_blockers;
        plan.notes
            .push("not searched: target-level policy problems block every release".into());
        return Ok(Built { plan, links });
    }
    if inputs.revisions {
        plan.notes.push("revision search: owned units are searched for exact-quality higher revisions (PROPER/REPACK)".into());
    }
    match target {
        Target::Movie(m) => {
            let label = plan.title.clone();
            match m.ownership {
                Ownership::Owned if !inputs.revisions => {
                    plan.state = PlanState::AlreadySatisfied;
                    plan.units
                        .push(unit(m.id, label, UnitStatus::Owned, None, OWNED_REASON));
                    return Ok(Built { plan, links });
                }
                Ownership::Unknown => {
                    plan.blockers
                        .push(blocker("ownership_unknown", "movie ownership is unknown"));
                    return Ok(Built { plan, links });
                }
                // Like native, a movie that is not available yet is not searched.
                Ownership::Missing if m.is_available == Some(false) => {
                    let why = m
                        .availability_note
                        .clone()
                        .unwrap_or_else(|| "the movie is not available yet".into());
                    plan.blockers.push(blocker("not_available", why.clone()));
                    plan.units
                        .push(unit(m.id, label, UnitStatus::NotAired, None, &why));
                    return Ok(Built { plan, links });
                }
                _ if m.queued => {
                    plan.state = PlanState::InProgress;
                    plan.units
                        .push(unit(m.id, label, UnitStatus::Queued, None, QUEUED_REASON));
                    return Ok(Built { plan, links });
                }
                _ => {}
            }
            let run = search::run(
                &inputs.search,
                target,
                profile,
                &[Scope::Movie],
                Scope::Movie,
            )
            .await?;
            finish_single(
                &mut plan,
                &mut links,
                &run,
                target,
                m.id,
                label,
                "movie",
                m.ownership == Ownership::Owned,
            );
            plan.search = Some(summarize(&[&run]));
        }
        Target::Episode { season, episode } => {
            let id = canonical_id(target);
            let label = plan.title.clone();
            let Some(info) = season.episode(*episode) else {
                let code = if season.episodes.is_some() {
                    "unknown_episode"
                } else {
                    "episode_list_unknown"
                };
                plan.blockers.push(blocker(
                    code,
                    format!(
                        "episode {episode} is not in the known season {} episode list",
                        season.season
                    ),
                ));
                plan.notes.extend(season.notes.iter().cloned());
                return Ok(Built { plan, links });
            };
            match (info.ownership, info.aired) {
                (Ownership::Owned, _) if !inputs.revisions => {
                    plan.state = PlanState::AlreadySatisfied;
                    plan.units
                        .push(unit(id, label, UnitStatus::Owned, None, OWNED_REASON));
                    return Ok(Built { plan, links });
                }
                (Ownership::Unknown, _) => {
                    plan.blockers.push(blocker(
                        "ownership_unknown",
                        season.ownership_source.clone(),
                    ));
                    return Ok(Built { plan, links });
                }
                _ if info.queued => {
                    plan.state = PlanState::InProgress;
                    plan.units
                        .push(unit(id, label, UnitStatus::Queued, None, QUEUED_REASON));
                    return Ok(Built { plan, links });
                }
                (Ownership::Missing, false) => {
                    plan.blockers.push(blocker(
                        "episode_not_aired",
                        "the episode has not aired (or has no air date)",
                    ));
                    return Ok(Built { plan, links });
                }
                _ => {}
            }
            let scope = Scope::Episode(season.season, *episode);
            let run = search::run(&inputs.search, target, profile, &[scope], scope).await?;
            finish_single(
                &mut plan,
                &mut links,
                &run,
                target,
                id,
                label,
                "episode",
                info.ownership == Ownership::Owned,
            );
            plan.search = Some(summarize(&[&run]));
        }
        Target::Season(s) => season_plan(inputs, &mut plan, &mut links, target, s, profile).await?,
    }
    Ok(Built { plan, links })
}

const OWNED_REASON: &str =
    "has a file; owned items are re-searched only by an explicit revision search (revisions: true)";
const QUEUED_REASON: &str = "already being downloaded (see /api/v1/queue)";

fn unit(
    target: MediaId,
    label: String,
    status: UnitStatus,
    candidate: Option<String>,
    reason: &str,
) -> PlanUnit {
    PlanUnit {
        target,
        label,
        status,
        candidate,
        reason: reason.into(),
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_single(
    plan: &mut Plan,
    links: &mut Vec<ReleaseLink>,
    run: &SearchRun,
    target: &Target,
    id: MediaId,
    label: String,
    selection: &str,
    owned: bool,
) {
    match run.candidates.iter().find(|c| c.decision.approved) {
        Some(c) => {
            plan.state = PlanState::ShadowPlanned;
            plan.selection = Some(selection.into());
            plan.units.push(unit(
                id,
                label,
                UnitStatus::Planned,
                Some(c.release_ref.clone()),
                if c.decision.replacement {
                    "revision upgrade: replaces the owned file (exact quality, higher revision)"
                } else {
                    "best approved release"
                },
            ));
            plan.candidates.push(search::summary(c));
            links.extend(crate::downloads::link_for(c, target, false));
        }
        None if owned => {
            plan.state = PlanState::AlreadySatisfied;
            plan.units.push(unit(
                id,
                label,
                UnitStatus::Owned,
                None,
                &format!(
                    "owned; no approved exact-quality higher revision ({})",
                    top_rejections(run)
                ),
            ));
        }
        None if run.candidates.is_empty() => {
            let queries: Vec<&str> = run.queries.iter().map(|q| q.query.as_str()).collect();
            let reason = format!("{NOTHING_OBSERVED}; queries: {}", queries.join(", "));
            plan.blockers
                .push(blocker("no_release_observed", reason.clone()));
            plan.units.push(unit(
                id,
                label,
                UnitStatus::NoReleaseObserved,
                None,
                &reason,
            ));
        }
        None => {
            let reason = top_rejections(run);
            plan.blockers
                .push(blocker("no_approved_release", reason.clone()));
            plan.units.push(unit(
                id,
                label,
                UnitStatus::NoApprovedRelease,
                None,
                &reason,
            ));
        }
    }
    plan.notes.extend(retrieval_note(&run.completeness()));
}

async fn season_plan(
    inputs: &PlanInputs<'_>,
    plan: &mut Plan,
    links: &mut Vec<ReleaseLink>,
    target: &Target,
    s: &SeasonTarget,
    profile: (Option<&Profile>, Option<String>),
) -> Result<(), ApiError> {
    plan.notes.extend(s.notes.iter().cloned());
    plan.notes
        .push(format!("ownership source: {}", s.ownership_source));
    let key = s.series.id.series_key().expect("series id");
    let Some(eps) = s.episodes.as_ref() else {
        plan.blockers.push(blocker(
            "episode_list_unknown",
            format!("no episode list for season {}", s.season),
        ));
        return Ok(());
    };
    let ep_id = |n: u16| MediaId::Episode(key, s.season, n);
    let label = |n: u16| format!("S{:02}E{:02}", s.season, n);
    let owned: Vec<u16> = eps
        .iter()
        .filter(|e| e.ownership == Ownership::Owned && !e.queued)
        .map(|e| e.number)
        .collect();
    let queued: Vec<u16> = eps.iter().filter(|e| e.queued).map(|e| e.number).collect();
    let unknown: Vec<u16> = eps
        .iter()
        .filter(|e| e.ownership == Ownership::Unknown)
        .map(|e| e.number)
        .collect();
    let unaired: Vec<u16> = eps.iter().filter(|e| !e.aired).map(|e| e.number).collect();
    let wanted: Vec<u16> = eps
        .iter()
        .filter(|e| e.ownership == Ownership::Missing && e.aired && !e.queued)
        .map(|e| e.number)
        .collect();
    let replaceable: Vec<u16> = if inputs.revisions {
        owned.clone()
    } else {
        vec![]
    };
    // With a client that selects files, a season pack may leave out owned episodes and ones
    // a download is making progress on, and may take over idle or unsent single-episode
    // downloads: those are worth a pack search even when nothing else is missing.
    let partial = inputs.search.partial_packs;
    let takeover: Vec<u16> = if partial {
        eps.iter()
            .filter(|e| e.queued && e.replaceable)
            .map(|e| e.number)
            .collect()
    } else {
        vec![]
    };

    if !unknown.is_empty() {
        plan.blockers
            .push(blocker("ownership_unknown", s.ownership_source.clone()));
        plan.units = eps
            .iter()
            .map(|e| {
                unit(
                    ep_id(e.number),
                    label(e.number),
                    UnitStatus::OwnershipUnknown,
                    None,
                    &s.ownership_source,
                )
            })
            .collect();
        return Ok(());
    }
    let mut units: BTreeMap<u16, PlanUnit> = BTreeMap::new();
    for e in eps {
        let (status, reason) = if e.queued {
            (UnitStatus::Queued, QUEUED_REASON)
        } else if e.ownership == Ownership::Owned {
            (UnitStatus::Owned, OWNED_REASON)
        } else if !e.aired {
            (
                UnitStatus::NotAired,
                "not aired (or no air date) per metadata",
            )
        } else {
            (UnitStatus::NoApprovedRelease, "no approved release found")
        };
        units.insert(
            e.number,
            unit(ep_id(e.number), label(e.number), status, None, reason),
        );
    }
    let finish_idle = |plan: &mut Plan, units: BTreeMap<u16, PlanUnit>| {
        if owned.is_empty() && queued.is_empty() {
            plan.blockers.push(blocker(
                "episode_not_aired",
                format!("no episode of season {} has aired", s.season),
            ));
        } else {
            plan.state = if queued.is_empty() {
                PlanState::AlreadySatisfied
            } else {
                PlanState::InProgress
            };
            if !unaired.is_empty() {
                plan.notes
                    .push(format!("{} episode(s) not aired yet", unaired.len()));
            }
        }
        plan.units = units.into_values().collect();
    };
    if wanted.is_empty() && replaceable.is_empty() && takeover.is_empty() {
        finish_idle(plan, units);
        return Ok(());
    }

    // Phase 1: season query (season packs plus any episode releases it returns).
    let season_scope = Scope::Season(s.season);
    let season_run = search::run(
        &inputs.search,
        target,
        profile.clone(),
        &[season_scope],
        season_scope,
    )
    .await?;
    let pack_eligible = unaired.is_empty()
        && (partial || (queued.is_empty() && (owned.is_empty() || inputs.revisions)));
    if !pack_eligible {
        plan.notes.push(if !queued.is_empty() && !partial {
            format!("season packs not eligible: episodes {queued:?} are already being downloaded")
        } else if !owned.is_empty() && !inputs.revisions && !partial {
            format!("season packs not eligible: they would re-download owned episodes {owned:?} (owned items are re-searched only by an explicit revision search)")
        } else {
            "season packs not eligible: the season has not fully aired".to_string()
        });
    }
    // The best approved pack. A weak-swarm pack is held back as a fallback: healthy single
    // episodes are better than a pack that will crawl, but a weak pack still beats nothing or
    // weak singles.
    let pack = pack_eligible
        .then(|| {
            season_run
                .candidates
                .iter()
                .find(|c| c.decision.approved && c.decision.full_season)
        })
        .flatten();
    let mut weak_pack = None;
    if let Some(c) = pack {
        if c.decision.health == Health::Weak {
            weak_pack = Some(c);
        } else {
            take_pack(plan, links, target, c, &wanted, &replaceable, &mut units);
            plan.units = units.into_values().collect();
            let summary = summarize(&[&season_run]);
            plan.notes.extend(retrieval_note(&summary.completeness));
            plan.search = Some(summary);
            return Ok(());
        }
    }

    // Phase 2: episode releases for the missing, aired episodes (and, for a revision search,
    // owned episodes).
    let mut remaining: BTreeSet<u16> = wanted.iter().chain(&replaceable).copied().collect();
    let wanted_set: BTreeSet<u16> = wanted.iter().copied().collect();
    let mut chosen: Vec<&Candidate> = vec![];
    // Weak-swarm singles from the season query don't close their episodes yet: the episode
    // searches below may find healthier copies. They are the fallback after those searches.
    pick(&season_run, &mut remaining, &wanted_set, &mut chosen, false);
    let to_query: Vec<u16> = remaining
        .iter()
        .copied()
        .filter(|n| wanted_set.contains(n))
        .chain(
            remaining
                .iter()
                .copied()
                .filter(|n| !wanted_set.contains(n)),
        )
        .take(MAX_EPISODE_QUERIES)
        .collect();
    for n in remaining
        .iter()
        .filter(|n| wanted_set.contains(n) && !to_query.contains(n))
    {
        let u = units.get_mut(n).expect("unit");
        u.status = UnitStatus::NotSearched;
        u.reason =
            format!("more than {MAX_EPISODE_QUERIES} missing episodes; not searched in this plan");
    }
    let scopes: Vec<Scope> = to_query
        .iter()
        .map(|n| Scope::Episode(s.season, *n))
        .collect();
    let episode_run = if scopes.is_empty() {
        None
    } else {
        let run = tokio::time::timeout_at(
            inputs.episode_deadline,
            search::run(&inputs.search, target, profile, &scopes, season_scope),
        )
        .await
        .unwrap_or_else(|_| {
            Err(ApiError::new(
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                ErrorCode::UpstreamTimeout,
                "plan time budget exhausted before the episode searches finished",
            ))
        });
        match run {
            Ok(run) => Some(run),
            Err(e) if is_upstream(e.code) => {
                for n in to_query.iter().filter(|n| wanted_set.contains(n)) {
                    let u = units.get_mut(n).expect("unit");
                    u.status = UnitStatus::SearchFailed;
                    u.reason = format!("episode search failed: {}", e.message);
                }
                plan.blockers
                    .push(blocker("search_failed", e.message.clone()));
                None
            }
            Err(e) => return Err(e),
        }
    };
    if let Some(run) = &episode_run {
        pick(run, &mut remaining, &wanted_set, &mut chosen, true);
    }
    pick(&season_run, &mut remaining, &wanted_set, &mut chosen, true);
    if let Some(run) = &episode_run {
        for n in to_query
            .iter()
            .filter(|n| remaining.contains(n) && wanted_set.contains(n))
        {
            let own = search::queries_for(target, Scope::Episode(s.season, *n));
            let all_failed = run
                .queries
                .iter()
                .filter(|q| own.contains(&q.query))
                .all(|q| q.status == "failed");
            if all_failed {
                let u = units.get_mut(n).expect("unit");
                u.status = UnitStatus::SearchFailed;
                u.reason = "episode query failed".into();
            }
        }
    }
    if let Some(c) = weak_pack
        && (remaining.iter().any(|n| wanted_set.contains(n))
            || chosen.iter().any(|x| x.decision.health == Health::Weak))
    {
        // The singles leave something unplanned or are just as weak: use the pack.
        take_pack(plan, links, target, c, &wanted, &replaceable, &mut units);
        plan.units = units.into_values().collect();
        let all_runs: Vec<&SearchRun> = std::iter::once(&season_run)
            .chain(episode_run.as_ref())
            .collect();
        let summary = summarize(&all_runs);
        plan.notes.extend(retrieval_note(&summary.completeness));
        plan.search = Some(summary);
        return Ok(());
    }
    for c in &chosen {
        for n in &c.decision.covered_episodes {
            if let Some(u) = units.get_mut(n) {
                u.status = UnitStatus::Planned;
                u.candidate = Some(c.release_ref.clone());
                u.reason = if replaceable.contains(n) {
                    "revision upgrade: replaces the owned file (exact quality, higher revision)"
                        .into()
                } else if c.decision.covered_episodes.len() > 1 {
                    "covered by a multi-episode release".into()
                } else {
                    "best approved episode release".into()
                };
            }
        }
        plan.candidates.push(search::summary(c));
        links.extend(crate::downloads::link_for(c, target, false));
    }
    if inputs.revisions {
        for n in &replaceable {
            if let Some(u) = units.get_mut(n)
                && u.status == UnitStatus::Owned
            {
                u.reason = "owned; no approved exact-quality higher revision found".into();
            }
        }
    }
    // Explain unplanned episodes with the rejections of releases that covered them.
    let all_runs: Vec<&SearchRun> = std::iter::once(&season_run)
        .chain(episode_run.as_ref())
        .collect();
    for u in units
        .values_mut()
        .filter(|u| u.status == UnitStatus::NoApprovedRelease)
    {
        let MediaId::Episode(_, _, n) = u.target else {
            continue;
        };
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for c in all_runs.iter().flat_map(|r| &r.candidates) {
            if c.decision.covered_episodes.contains(&n) && seen.insert(&c.release_ref) {
                for r in &c.decision.rejections {
                    *counts.entry(r.code.as_str()).or_default() += 1;
                }
            }
        }
        u.reason = if seen.is_empty() {
            u.status = UnitStatus::NoReleaseObserved;
            NOTHING_OBSERVED.into()
        } else {
            let mut v: Vec<_> = counts.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let parts: Vec<String> = v.iter().take(4).map(|(k, n)| format!("{k} x{n}")).collect();
            format!(
                "{} release(s) covered this episode, none approved: {}",
                seen.len(),
                parts.join(", ")
            )
        };
    }
    plan.selection = Some("episodes".into());
    // Blockers mirror the final unit statuses.
    let with_status = |st: UnitStatus| -> Vec<String> {
        units
            .values()
            .filter(|u| u.status == st)
            .map(|u| u.label.clone())
            .collect()
    };
    let failed = with_status(UnitStatus::SearchFailed);
    if !failed.is_empty() && !plan.blockers.iter().any(|b| b.code == "search_failed") {
        plan.blockers.push(blocker(
            "search_failed",
            format!("episode queries failed for {failed:?}"),
        ));
    }
    let skipped = with_status(UnitStatus::NotSearched);
    if !skipped.is_empty() {
        plan.blockers.push(blocker(
            "not_searched",
            format!(
                "{} missing episode(s) beyond the per-plan query cap of {MAX_EPISODE_QUERIES}",
                skipped.len()
            ),
        ));
    }
    let unapproved = with_status(UnitStatus::NoApprovedRelease);
    if !unapproved.is_empty() {
        plan.blockers.push(blocker(
            "no_approved_release",
            format!("no approved release for {unapproved:?}"),
        ));
    }
    let unobserved = with_status(UnitStatus::NoReleaseObserved);
    if !unobserved.is_empty() {
        plan.blockers.push(blocker(
            "no_release_observed",
            format!("no release observed for {unobserved:?}; not proof of absence (see search.completeness)"),
        ));
    }
    let missing_left = remaining.iter().any(|n| wanted_set.contains(n));
    plan.state = if chosen.is_empty() && !missing_left {
        if queued.is_empty() {
            PlanState::AlreadySatisfied
        } else {
            PlanState::InProgress
        }
    } else if !missing_left {
        PlanState::ShadowPlanned
    } else if chosen.is_empty() {
        PlanState::Blocked
    } else {
        PlanState::ShadowPartial
    };
    let summary = summarize(&all_runs);
    plan.notes.extend(retrieval_note(&summary.completeness));
    plan.search = Some(summary);
    plan.units = units.into_values().collect();
    Ok(())
}

/// Plans the whole of `wanted` (and the replaceable owned episodes) as one season pack.
fn take_pack(
    plan: &mut Plan,
    links: &mut Vec<ReleaseLink>,
    target: &Target,
    c: &Candidate,
    wanted: &[u16],
    replaceable: &[u16],
    units: &mut BTreeMap<u16, PlanUnit>,
) {
    plan.state = PlanState::ShadowPlanned;
    plan.selection = Some("season_pack".into());
    let d = &c.decision;
    for n in wanted.iter().chain(replaceable).chain(&d.supersedes) {
        let u = units.get_mut(n).expect("episode has a unit");
        u.status = UnitStatus::Planned;
        u.candidate = Some(c.release_ref.clone());
        u.reason = if replaceable.contains(n) {
            "revision upgrade: replaced by the selected season pack".into()
        } else if d.supersedes.contains(n) {
            "covered by the selected season pack, which replaces this episode's idle or unstarted download (cancelled, not blocklisted)".into()
        } else {
            "covered by the selected season pack".into()
        };
    }
    if !d.skips_owned.is_empty() {
        plan.notes.push(format!(
            "the season pack leaves out owned episodes {:?}; they are not downloaded",
            d.skips_owned
        ));
    }
    if !d.skips_active.is_empty() {
        plan.notes.push(format!(
            "the season pack leaves out episodes {:?}: their downloads are making progress and stay as they are",
            d.skips_active
        ));
    }
    if !d.supersedes.is_empty() {
        plan.notes.push(format!(
            "the season pack takes over episodes {:?} from downloads that are idle or not started; those are cancelled, not blocklisted, when the plan is executed",
            d.supersedes
        ));
    }
    plan.candidates.push(search::summary(c));
    links.extend(crate::downloads::link_for(c, target, false));
}

/// Greedy selection in rank order: approved releases whose episodes are all still open and
/// that cover a wanted episode (or are revision replacements); weak-swarm ones only if `weak`.
fn pick<'a>(
    run: &'a SearchRun,
    remaining: &mut BTreeSet<u16>,
    wanted: &BTreeSet<u16>,
    chosen: &mut Vec<&'a Candidate>,
    weak: bool,
) {
    for c in run.candidates.iter().filter(|c| {
        c.decision.approved
            && !c.decision.full_season
            && (weak || c.decision.health != Health::Weak)
    }) {
        let eps = &c.decision.covered_episodes;
        let useful = eps.iter().any(|e| wanted.contains(e)) || c.decision.replacement;
        if useful && !eps.is_empty() && eps.iter().all(|e| remaining.contains(e)) {
            for e in eps {
                remaining.remove(e);
            }
            chosen.push(c);
        }
    }
}

fn is_upstream(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::UpstreamTimeout
            | ErrorCode::UpstreamAuth
            | ErrorCode::UpstreamRateLimited
            | ErrorCode::UpstreamUnavailable
            | ErrorCode::UpstreamBadResponse
    )
}
