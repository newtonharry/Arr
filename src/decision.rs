//! Release decisions: every applicable rejection is collected; a release is approved only
//! when the list is empty. Prowlarr results carry no Sonarr/Radarr approval.

use crate::catalog::{CatalogIndex, movie_title_keys, series_keys, series_title_keys, title_keys};
use crate::prowlarr::ProwlarrRelease;
use crate::quality::Quality;
use crate::release::{Numbering, ParsedRelease, QualityBasis};
use crate::settings::{
    BlockingIssue, LanguageRequirement, MediaKind, Profile, ReleasePolicy, Settings,
    UnmarkedLanguage,
};
use crate::target::{MovieTarget, Ownership, SeasonTarget, Target};
use crate::title;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const MIB: f64 = 1024.0 * 1024.0;
const TITLE_ONLY_NOTE: &str = "release names no year; matched by title only";
const INFERRED_PREFIX: &str = "source not named;";

/// Stable rejection codes (also published in the OpenAPI schema).
pub const REJECTION_CODES: &[&str] = &[
    "seed_requirement_unmet",
    "policy_unsupported",
    "profile_missing",
    "identity_conflict",
    "unsupported_series_type",
    "unsupported_numbering",
    "title_mismatch",
    "year_mismatch",
    "year_missing",
    "target_year_unknown",
    "ambiguous_title",
    "not_a_movie",
    "multi_movie_bundle",
    "not_tv_numbered",
    "ambiguous_numbering",
    "wrong_season",
    "multi_season_pack",
    "partial_season_pack",
    "complete_series_pack",
    "season_pack_for_episode",
    "wrong_episode",
    "unknown_episode",
    "episode_list_unknown",
    "episode_not_aired",
    "season_not_fully_aired",
    "covers_owned",
    "ownership_unknown",
    "not_available",
    "sample_or_extra",
    "three_d",
    "hardcoded_subs",
    "dangerous_file_type",
    "quality_unknown",
    "quality_not_allowed",
    "language_mismatch",
    "language_ambiguous_multi",
    "language_unknown",
    "size_unknown",
    "size_limits_missing",
    "size_unverifiable_runtime",
    "size_below_minimum",
    "size_above_maximum",
    "size_above_global_maximum",
    "protocol_unsupported",
    "protocol_disabled",
    "no_download_link",
    "indexer_not_configured",
    "indexer_policy_unsupported",
    "seeders_unknown",
    "below_minimum_seeders",
    "release_age_unknown",
    "delay_profile_wait",
    "covers_queued",
    "blocklisted",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejection {
    pub code: String,
    pub message: String,
}

/// A tracker's seed requirement (Torznab `minimumratio`, `minimumseedtime` in seconds)
/// against the indexer's seed goal (`ratio`, `minutes`; negative = unset). Each goal half
/// that ends seeding must satisfy the matching requirement, and at least one must be set.
/// An unset half falls to the download client's own share limits, which are reported here
/// but cannot be checked. `None` when the release states no requirement or the goal meets it.
pub fn seed_requirement(
    r: &ProwlarrRelease,
    ratio: Option<f64>,
    minutes: Option<i64>,
) -> Option<Rejection> {
    let need_ratio = r.minimum_ratio.filter(|v| *v > 0.0);
    let need_secs = r.minimum_seed_time.filter(|v| *v > 0);
    if need_ratio.is_none() && need_secs.is_none() {
        return None;
    }
    let ratio = ratio.filter(|v| *v >= 0.0);
    let minutes = minutes.filter(|v| *v >= 0);
    let need = format!(
        "the tracker requires{}{}",
        need_ratio
            .map(|v| format!(" ratio {v:.2}"))
            .unwrap_or_default(),
        need_secs
            .map(|v| format!(" seeding {} min", (v + 59) / 60))
            .unwrap_or_default()
    );
    let mut problems = vec![];
    if ratio.is_none() && minutes.is_none() {
        problems.push(
            "the indexer has no seed goal (the client's share limits would decide)".to_string(),
        );
    }
    if let Some(g) = ratio
        && need_ratio.is_none_or(|n| g < n)
    {
        problems.push(format!("the seed goal can end seeding at ratio {g:.2}"));
    }
    if let Some(m) = minutes
        && need_secs.is_none_or(|n| m.saturating_mul(60) < n)
    {
        problems.push(format!("the seed goal can end seeding after {m} min"));
    }
    (!problems.is_empty()).then(|| {
        rej(
            "seed_requirement_unmet",
            format!("{need}; {}", problems.join("; ")),
        )
    })
}

fn rej(code: &'static str, message: impl Into<String>) -> Rejection {
    debug_assert!(
        REJECTION_CODES.contains(&code),
        "undocumented rejection code {code}"
    );
    Rejection {
        code: code.into(),
        message: message.into(),
    }
}

/// How well a release's swarm can be expected to deliver; ranks above the quality tier among
/// approved releases (see [`health`]). `Weak < Ok < Healthy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Passes the indexer's minimum seeders but is likely to be slow or to stall.
    Weak,
    Ok,
    Healthy,
}

/// Swarm health from the seeder and leecher counts (`None` = unknown, which counts as the worst).
///
/// - `healthy`: 50 or more seeders, or 10 or more with leechers at most 10 times the seeders,
///   or 5 or more with leechers at most equal to the seeders.
/// - `ok`: 3 or more seeders.
/// - `weak`: anything else (fewer than 3 seeders, or unknown).
///
/// Seeders are the supply. Leechers are demand, but they also upload what they already have,
/// so they count against a release only when they swamp the seeders. Ten seeders are enough
/// for one episode or one movie to fill a home connection even with a crowd (hence the
/// generous 10x allowance); with only 5 to 9 seeders the crowd must be no bigger than they
/// are. 50 seeders are healthy whatever the crowd, and also when an indexer reports no leecher
/// count. Inside a band the order is by seeders first, so fewer leechers never beat more
/// seeders.
pub fn health(seeders: Option<i64>, leechers: Option<i64>) -> Health {
    let Some(s) = seeders.filter(|s| *s >= 0) else {
        return Health::Weak;
    };
    if s >= 50 {
        return Health::Healthy;
    }
    // Unknown leechers count as worst: they never satisfy a leecher bound.
    let l = leechers.filter(|l| *l >= 0);
    let within = |factor: i64| l.is_some_and(|l| l <= s.saturating_mul(factor));
    if (s >= 10 && within(10)) || (s >= 5 && within(1)) {
        Health::Healthy
    } else if s >= 3 {
        Health::Ok
    } else {
        Health::Weak
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub approved: bool,
    pub rejections: Vec<Rejection>,
    pub notes: Vec<String>,
    pub covered_episodes: Vec<u16>,
    pub owned_overlap: Vec<u16>,
    pub full_season: bool,
    /// The release replaces owned file(s) as a native revision upgrade (exact quality, higher
    /// known revision); `owned_overlap` lists which episodes (empty for a movie).
    #[serde(default)]
    pub replacement: bool,
    /// Swarm health band (`weak`, `ok`, `healthy`); ranks above the quality tier.
    pub health: Health,
    /// A full-season pack leaves these owned episodes out: they are not a revision upgrade,
    /// so the grab does not include them (`covered_episodes` lists only what it gets).
    #[serde(default)]
    pub skips_owned: Vec<u16>,
    /// A full-season pack leaves these episodes out because a download that is making
    /// progress already has them.
    #[serde(default)]
    pub skips_active: Vec<u16>,
    /// A full-season pack takes these episodes over from downloads that are idle, waiting for
    /// metadata or not yet submitted; those downloads are cancelled (not blocklisted) when
    /// the pack is grabbed. They are part of `covered_episodes`.
    #[serde(default)]
    pub supersedes: Vec<u16>,
    #[serde(skip)]
    pub tier: Option<usize>,
    #[serde(skip)]
    pub preferred_distance: u64,
    /// Count of unverified aspects (title-only match, inferred quality); ranks lower first.
    #[serde(skip)]
    pub uncertainty: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Movie,
    Season(u16),
    Episode(u16, u16),
}

pub struct EvalContext<'a> {
    pub target: &'a Target,
    pub scope: Scope,
    pub settings: &'a Settings,
    pub profile: Option<&'a Profile>,
    pub profile_id: Option<String>,
    pub catalog: Option<&'a CatalogIndex>,
    pub now: OffsetDateTime,
    pub catalog_observed_at: Option<String>,
    /// Blocklisted release refs and `hash:<info hash>` entries for this media.
    pub blocked: Option<&'a std::collections::HashSet<String>>,
    /// The download client can fetch only some files of a torrent (the embedded engine). A
    /// full-season pack may then leave out owned or already-downloading episodes; without
    /// it a pack must cover every episode, as before.
    pub partial_packs: bool,
}

impl EvalContext<'_> {
    fn media(&self) -> MediaKind {
        self.target.media()
    }

    fn policy(&self) -> Option<&ReleasePolicy> {
        self.settings.policy(self.media())
    }

    fn season_target(&self) -> Option<&SeasonTarget> {
        match self.target {
            Target::Season(s) | Target::Episode { season: s, .. } => Some(s),
            Target::Movie(_) => None,
        }
    }

    /// Target-level problems; they apply to every release and block plans outright.
    pub fn target_blockers(&self) -> Vec<Rejection> {
        let mut out = vec![];
        let blocking: Vec<&BlockingIssue> = self
            .settings
            .blocking_for(self.media(), self.profile_id.as_deref());
        for b in blocking {
            out.push(rej(
                "policy_unsupported",
                format!("{}: {}", b.code, b.message),
            ));
        }
        if self.profile.is_none() {
            out.push(rej(
                "profile_missing",
                match &self.profile_id {
                    Some(p) => format!("quality profile {p} is not in the imported settings"),
                    None => "no quality profile applies to this target".to_string(),
                },
            ));
        }
        if self.policy().is_none() {
            out.push(rej(
                "policy_unsupported",
                format!("no release policy imported for {:?}", self.media()),
            ));
        }
        match self.target {
            Target::Movie(m) if m.identity_conflict => {
                out.push(rej(
                    "identity_conflict",
                    format!("{} appears more than once in the imported catalog", m.id),
                ));
            }
            Target::Season(s) | Target::Episode { season: s, .. } => {
                if s.series.identity_conflict {
                    out.push(rej(
                        "identity_conflict",
                        format!(
                            "{} appears more than once in the imported catalog",
                            s.series.id
                        ),
                    ));
                }
                if s.series.series_type != "standard" {
                    out.push(rej(
                        "unsupported_series_type",
                        format!(
                            "series type '{}' (absolute/date numbering) is not supported",
                            s.series.series_type
                        ),
                    ));
                }
                if s.series.scene_numbering {
                    out.push(rej(
                        "unsupported_numbering",
                        "series uses scene numbering, which is not supported",
                    ));
                }
            }
            _ => {}
        }
        out
    }
}

/// Words of the target's own titles, so "Extras" (a series) is not treated as an extras marker.
fn title_words(ctx: &EvalContext) -> Vec<String> {
    let mut words: Vec<String> = vec![];
    let mut add = |t: &str| words.extend(title::tokens(t));
    match ctx.target {
        Target::Movie(m) => {
            add(&m.title);
            m.alternate_titles.iter().for_each(|t| add(t));
        }
        Target::Season(s) | Target::Episode { season: s, .. } => {
            add(&s.series.title);
            s.series.alternate_titles.iter().for_each(|a| add(&a.title));
        }
    }
    words
}

fn target_keys(ctx: &EvalContext) -> (Vec<String>, Vec<u16>) {
    match ctx.target {
        Target::Movie(m) => {
            let aliases: Vec<&str> = m.alternate_titles.iter().map(String::as_str).collect();
            (
                title_keys(&m.title, &aliases),
                m.year.into_iter().chain(m.secondary_year).collect(),
            )
        }
        Target::Season(s) | Target::Episode { season: s, .. } => {
            let year = s
                .series
                .year
                .or_else(|| title::split_trailing_year(&s.series.title).1);
            (
                series_keys(&s.series.title, &s.series.alternate_titles, Some(s.season)),
                year.into_iter().collect(),
            )
        }
    }
}

fn check_title(
    ctx: &EvalContext,
    p: &ParsedRelease,
    out: &mut Vec<Rejection>,
    notes: &mut Vec<String>,
) {
    let (keys, years) = target_keys(ctx);
    let movie = matches!(ctx.target, Target::Movie(_));
    let key_a = title::key_from_tokens(&p.title_tokens);
    let key_b = p.year.map(|y| {
        let mut t = p.title_tokens.clone();
        t.push(y.to_string());
        title::key_from_tokens(&t)
    });
    let (matched_key, release_year) = if keys.contains(&key_a) {
        (key_a, p.year)
    } else if let Some(kb) = key_b.filter(|k| keys.contains(k)) {
        (kb, None)
    } else {
        out.push(rej(
            "title_mismatch",
            format!(
                "release title '{}' does not match the target title or its aliases",
                p.title_tokens.join(" ")
            ),
        ));
        return;
    };
    match release_year {
        Some(y) if years.is_empty() => {
            if movie {
                out.push(rej(
                    "target_year_unknown",
                    format!("release says {y} but the target's year is unknown"),
                ));
            } else {
                notes.push(format!(
                    "release year {y} not verified: series year unknown"
                ));
            }
        }
        Some(y) if !years.contains(&y) => {
            out.push(rej(
                "year_mismatch",
                format!("release year {y} does not match {years:?}"),
            ));
        }
        Some(_) => {}
        None if movie => out.push(rej(
            "year_missing",
            "movie release names no year; refusing to match by title alone",
        )),
        None => notes.push(TITLE_ONLY_NOTE.into()),
    }

    // Other catalog items with the same title key make the release ambiguous unless the year
    // tells them apart.
    if let Some(cat) = ctx.catalog {
        let target_id = match ctx.target {
            Target::Movie(m) => m.id,
            Target::Season(s) | Target::Episode { season: s, .. } => s.series.id,
        };
        let others: Vec<String> = if movie {
            cat.movies_with_key(&matched_key, Some(target_id))
                .into_iter()
                .filter(|m| movie_title_keys(m).contains(&matched_key))
                .filter(|m| {
                    release_year.is_none_or(|y| m.year == Some(y) || m.secondary_year == Some(y))
                })
                .map(|m| format!("{} ({})", m.title, m.id))
                .collect()
        } else {
            let season = ctx.season_target().map(|s| s.season);
            cat.series_with_key(&matched_key, Some(target_id))
                .into_iter()
                .filter(|s| series_title_keys(s, season).contains(&matched_key))
                .filter(|s| {
                    let year = s.year.or_else(|| title::split_trailing_year(&s.title).1);
                    release_year.is_none_or(|y| year == Some(y))
                })
                .map(|s| format!("{} ({})", s.title, s.id))
                .collect()
        };
        if !others.is_empty() {
            out.push(rej(
                "ambiguous_title",
                format!(
                    "release title also matches catalog item(s) {}",
                    others.join(", ")
                ),
            ));
        }
    }
}

struct Coverage {
    /// Every episode the release holds (what size limits are judged over).
    episodes: Vec<u16>,
    full_season: bool,
    /// For a full-season pack with file selection: what the grab would actually get.
    pack: Option<PackSplit>,
}

/// How a full-season pack divides the season when the client can select files.
#[derive(Default)]
struct PackSplit {
    /// Episodes the grab gets: missing ones, ones taken over from replaceable downloads,
    /// and owned ones that are a revision upgrade.
    get: Vec<u16>,
    skips_owned: Vec<u16>,
    skips_active: Vec<u16>,
    supersedes: Vec<u16>,
}

fn ep_list(eps: &[u16]) -> String {
    eps.iter()
        .map(|e| format!("E{e:02}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The episode checks of a full-season pack whose file selection leaves episodes out.
/// Owned episodes that are not a revision upgrade, and episodes a download that is making
/// progress already has, are left out of the grab. Episodes whose download is replaceable
/// (see `catalog::Episode::replaceable`) are taken over, unless the pack's own swarm is weak:
/// a weak pack never displaces a download. The pack is rejected when nothing is left for it.
fn pack_checks(
    ctx: &EvalContext,
    p: &ParsedRelease,
    s: &SeasonTarget,
    pack_health: Health,
    out: &mut Vec<Rejection>,
    owned: &mut Vec<u16>,
) -> PackSplit {
    let mut split = PackSplit::default();
    let mut unknown_own = vec![];
    for x in s.episodes.iter().flatten() {
        let n = x.number;
        let upgrade = x.ownership == Ownership::Owned
            && revision_upgrade(ctx, x.existing.as_ref(), p).is_ok();
        match x.ownership {
            Ownership::Unknown => {
                unknown_own.push(n);
                continue;
            }
            Ownership::Owned if !upgrade => {
                split.skips_owned.push(n);
                continue;
            }
            _ => {}
        }
        if x.queued {
            if x.replaceable && pack_health != Health::Weak {
                split.supersedes.push(n);
            } else {
                split.skips_active.push(n);
                continue;
            }
        }
        if upgrade {
            owned.push(n);
        }
        split.get.push(n);
    }
    if !unknown_own.is_empty() {
        out.push(rej(
            "ownership_unknown",
            format!(
                "ownership of episodes {unknown_own:?} is unknown ({})",
                s.ownership_source
            ),
        ));
    }
    if split.get.is_empty() && unknown_own.is_empty() {
        out.push(if split.skips_active.is_empty() {
            rej(
                "covers_owned",
                format!(
                    "every episode of the season is already owned ({}) and none is a revision upgrade",
                    ep_list(&split.skips_owned)
                ),
            )
        } else {
            rej(
                "covers_queued",
                format!(
                    "nothing is left for this pack: {} already downloading, {}",
                    ep_list(&split.skips_active),
                    if split.skips_owned.is_empty() {
                        "no other episode is missing".to_string()
                    } else {
                        format!("{} owned", ep_list(&split.skips_owned))
                    }
                ),
            )
        });
    }
    split
}

/// Why an owned item is never re-planned.
fn owned_reason(ctx: &EvalContext) -> &'static str {
    match ctx.profile {
        Some(p) if p.upgrade_allowed => {
            "quality upgrades of owned files are not implemented in this build"
        }
        _ => "the profile does not allow upgrades",
    }
}

/// Native revision-upgrade rule (Sonarr 4.0.15 / Radarr 5.26.2 `IsUpgradeAllowed` and
/// `IsUpgradable` with `preferAndUpgrade`): only the exact same quality with a higher known
/// revision may replace an owned file, even when profile upgrades are off. Everything that
/// cannot be verified fails closed.
pub fn revision_upgrade(
    ctx: &EvalContext,
    existing: Option<&crate::catalog::FileEvidence>,
    p: &ParsedRelease,
) -> Result<(), String> {
    if !ctx.policy().is_some_and(|x| x.revision_upgrades) {
        return Err(format!(
            "{}; revision upgrades are off (revision_upgrades = true in the setup enables them)",
            owned_reason(ctx)
        ));
    }
    if p.quality_basis != QualityBasis::Parsed {
        return Err("the release's quality is not stated explicitly; it cannot be compared with the owned file".into());
    }
    revision_rule(existing, p.quality, p.revision).map_err(|e| {
        if e.starts_with("the owned file is ") {
            format!("{e} ({})", owned_reason(ctx))
        } else {
            e
        }
    })
}

/// The evidence half of the revision rule, shared with import-time re-verification: known
/// exact quality, known lower revision and a known managed path.
pub fn revision_rule(
    existing: Option<&crate::catalog::FileEvidence>,
    quality: crate::quality::Quality,
    revision: crate::release::Revision,
) -> Result<(), String> {
    let Some(ev) = existing else {
        return Err("nothing is known about the owned file".into());
    };
    let Some(q) = ev.quality else {
        return Err("the owned file's quality is unknown; revision upgrades fail closed".into());
    };
    if quality != q {
        return Err(format!(
            "the owned file is {q} and the release {quality}; only an exact-quality revision may replace it"
        ));
    }
    let Some(rev) = ev.revision else {
        return Err("the owned file's revision is unknown; revision upgrades fail closed".into());
    };
    if revision <= rev {
        return Err(format!(
            "revision {}.{} is not higher than the owned {}.{} (real.version)",
            revision.real, revision.version, rev.real, rev.version
        ));
    }
    if ev.path.is_none() {
        return Err(
            "the owned file's path is unknown (rescan the library before replacing it)".into(),
        );
    }
    Ok(())
}

fn episode_checks(
    ctx: &EvalContext,
    p: &ParsedRelease,
    s: &SeasonTarget,
    eps: &[u16],
    out: &mut Vec<Rejection>,
    owned: &mut Vec<u16>,
) {
    let Some(list) = s.episodes.as_ref() else {
        out.push(rej(
            "episode_list_unknown",
            format!(
                "no episode list for season {}; coverage cannot be verified",
                s.season
            ),
        ));
        return;
    };
    let unknown: Vec<u16> = eps
        .iter()
        .copied()
        .filter(|e| !list.iter().any(|x| x.number == *e))
        .collect();
    if !unknown.is_empty() {
        out.push(rej(
            "unknown_episode",
            format!(
                "episodes {unknown:?} are not in the season {} episode list",
                s.season
            ),
        ));
    }
    let covered: Vec<_> = list.iter().filter(|x| eps.contains(&x.number)).collect();
    let unaired: Vec<u16> = covered
        .iter()
        .filter(|x| !x.aired)
        .map(|x| x.number)
        .collect();
    if !unaired.is_empty() {
        out.push(rej("episode_not_aired", format!("episodes {unaired:?} have not aired according to the metadata (or have no air date)")));
    }
    let mut refused: Vec<String> = vec![];
    for x in covered.iter().filter(|x| x.ownership == Ownership::Owned) {
        owned.push(x.number);
        if let Err(why) = revision_upgrade(ctx, x.existing.as_ref(), p) {
            refused.push(format!("E{:02}: {why}", x.number));
        }
    }
    let queued: Vec<u16> = covered
        .iter()
        .filter(|x| x.queued)
        .map(|x| x.number)
        .collect();
    if !queued.is_empty() {
        out.push(rej(
            "covers_queued",
            format!("episodes {queued:?} are already being downloaded"),
        ));
    }
    let unknown_own: Vec<u16> = covered
        .iter()
        .filter(|x| x.ownership == Ownership::Unknown)
        .map(|x| x.number)
        .collect();
    if !refused.is_empty() {
        out.push(rej(
            "covers_owned",
            format!(
                "would replace owned episodes {owned:?}: {}",
                refused.join("; ")
            ),
        ));
    }
    if !unknown_own.is_empty() {
        out.push(rej(
            "ownership_unknown",
            format!(
                "ownership of episodes {unknown_own:?} is unknown ({})",
                s.ownership_source
            ),
        ));
    }
}

fn check_scope(
    ctx: &EvalContext,
    p: &ParsedRelease,
    pack_health: Health,
    out: &mut Vec<Rejection>,
    owned: &mut Vec<u16>,
) -> Coverage {
    let none = Coverage {
        episodes: vec![],
        full_season: false,
        pack: None,
    };
    let n = &p.numbering;
    let (s, target_season) = match (ctx.scope, ctx.season_target()) {
        (Scope::Movie, _) => {
            if *n != Numbering::None {
                out.push(rej(
                    "not_a_movie",
                    "release carries TV season/episode numbering",
                ));
            }
            return none;
        }
        (Scope::Season(t) | Scope::Episode(t, _), Some(s)) => (s, t),
        _ => return none,
    };
    match n {
        Numbering::None => out.push(rej(
            "not_tv_numbered",
            "no season/episode numbering found in the release title",
        )),
        Numbering::Daily => out.push(rej("unsupported_numbering", "date-numbered release")),
        Numbering::Ambiguous => out.push(rej(
            "ambiguous_numbering",
            "season/episode numbering is contradictory or unreadable",
        )),
        Numbering::CompleteSeries => out.push(rej(
            "complete_series_pack",
            "whole-series pack without season numbers",
        )),
        Numbering::Seasons { seasons } => out.push(rej(
            "multi_season_pack",
            format!("pack spans seasons {seasons:?}; exactly season {target_season} is wanted"),
        )),
        Numbering::PartialSeason { season } if *season != target_season => out.push(rej(
            "wrong_season",
            format!("release is season {season}, wanted {target_season}"),
        )),
        Numbering::PartialSeason { .. } => out.push(rej(
            "partial_season_pack",
            "partial season (part/volume) pack; coverage cannot be verified",
        )),
        Numbering::Season { season } | Numbering::Episodes { season, .. }
            if *season != target_season =>
        {
            out.push(rej(
                "wrong_season",
                format!("release is season {season}, wanted {target_season}"),
            ))
        }
        Numbering::Season { .. } => match ctx.scope {
            Scope::Episode(_, e) => out.push(rej(
                "season_pack_for_episode",
                format!("full-season pack; only episode {e} is wanted"),
            )),
            _ => {
                let Some(list) = s.episodes.as_ref() else {
                    out.push(rej("episode_list_unknown", format!("no episode list for season {target_season}; a season pack's coverage cannot be verified")));
                    return none;
                };
                let all: Vec<u16> = list.iter().map(|e| e.number).collect();
                let unaired = list.iter().filter(|e| !e.aired).count();
                if unaired > 0 {
                    out.push(rej("season_not_fully_aired", format!("{unaired} episode(s) of season {target_season} have not aired; a full-season pack cannot be complete")));
                }
                let mut sub = vec![];
                let pack = if ctx.partial_packs {
                    Some(pack_checks(ctx, p, s, pack_health, &mut sub, owned))
                } else {
                    episode_checks(ctx, p, s, &all, &mut sub, owned);
                    None
                };
                out.extend(sub.into_iter().filter(|r| r.code != "episode_not_aired"));
                return Coverage {
                    episodes: all,
                    full_season: true,
                    pack,
                };
            }
        },
        Numbering::Episodes { episodes, .. } => {
            if let Scope::Episode(_, e) = ctx.scope
                && *episodes != [e]
            {
                out.push(rej(
                    "wrong_episode",
                    format!("release covers episodes {episodes:?}; exactly episode {e} is wanted"),
                ));
            }
            episode_checks(ctx, p, s, episodes, out, owned);
            return Coverage {
                episodes: episodes.clone(),
                full_season: false,
                pack: None,
            };
        }
    }
    none
}

/// What a name's explicit language markers say about a set of allowed languages.
pub enum Explicit {
    Allowed,
    /// (rejection code, message)
    Rejected(&'static str, String),
    /// No language, MULTI or dubbed marker.
    Unmarked,
}

/// The explicit-marker part of the language rule, shared by release decisions and the
/// import-time check of the payload's own names.
pub fn explicit_language(allowed: &[String], p: &ParsedRelease) -> Explicit {
    if p.languages.iter().any(|l| allowed.iter().any(|a| a == l)) {
        return Explicit::Allowed;
    }
    if !p.languages.is_empty() {
        return Explicit::Rejected(
            "language_mismatch",
            format!("marked {:?}; profile requires {allowed:?}", p.languages),
        );
    }
    if p.multi_language {
        return Explicit::Rejected(
            "language_ambiguous_multi",
            format!(
                "MULTI/dual without an explicit {allowed:?} marker (no multi-language mapping is configured)"
            ),
        );
    }
    if p.dubbed {
        return Explicit::Rejected("language_unknown", "dubbed into an unnamed language".into());
    }
    Explicit::Unmarked
}

/// Languages a profile requires (`None` = any language).
pub fn required_languages(
    profile: &Profile,
    original: Option<&str>,
) -> Option<Result<Vec<String>, String>> {
    match &profile.language.requirement {
        LanguageRequirement::Any => None,
        LanguageRequirement::OneOf { languages } => Some(Ok(languages.clone())),
        LanguageRequirement::Original => {
            Some(original.map(|o| vec![o.to_string()]).ok_or_else(|| {
                "the profile requires the original language, which is unknown".to_string()
            }))
        }
    }
}

fn check_language(
    ctx: &EvalContext,
    p: &ParsedRelease,
    profile: &Profile,
    out: &mut Vec<Rejection>,
    notes: &mut Vec<String>,
) {
    let original = match ctx.target {
        Target::Movie(m) => m.original_language.clone(),
        Target::Season(s) | Target::Episode { season: s, .. } => s.series.original_language.clone(),
    };
    let allowed: Vec<String> = match &profile.language.requirement {
        LanguageRequirement::Any => return,
        LanguageRequirement::OneOf { languages } => languages.clone(),
        LanguageRequirement::Original => match &original {
            Some(o) => vec![o.clone()],
            None => {
                out.push(rej(
                    "language_unknown",
                    "profile requires the original language, which is unknown for this target",
                ));
                return;
            }
        },
    };
    match explicit_language(&allowed, p) {
        Explicit::Allowed => return,
        Explicit::Rejected(code, message) => {
            out.push(rej(code, message));
            return;
        }
        Explicit::Unmarked => {}
    }
    match profile.language.unmarked {
        UnmarkedLanguage::Accept => notes.push(format!("no language marker; accepted (native Sonarr v4 applies no language filter; {allowed:?} enforced only against explicit markers)")),
        UnmarkedLanguage::AssumeOriginal => match &original {
            Some(o) if allowed.contains(o) => notes.push(format!("no language marker; assumed original language {o}")),
            Some(o) => out.push(rej("language_mismatch", format!("no language marker, so the original language {o} is assumed; profile requires {allowed:?}"))),
            None => out.push(rej("language_unknown", "no language marker and the original language is unknown")),
        },
    }
}

fn runtime_total(ctx: &EvalContext, cov: &Coverage) -> Option<f64> {
    match ctx.target {
        Target::Movie(m) => m.runtime_minutes.map(f64::from),
        Target::Season(s) | Target::Episode { season: s, .. } => {
            if cov.episodes.is_empty() {
                return None;
            }
            let mut total = 0.0;
            for e in &cov.episodes {
                let ep = s.episode(*e);
                let rt = ep
                    .and_then(|x| x.runtime_minutes)
                    .or(s.series.runtime_minutes)?;
                total += f64::from(rt);
            }
            Some(total)
        }
    }
}

fn check_size(
    ctx: &EvalContext,
    r: &ProwlarrRelease,
    quality: Quality,
    cov: &Coverage,
    out: &mut Vec<Rejection>,
) -> u64 {
    let size = r.size.filter(|s| *s > 0);
    let Some(size) = size else {
        out.push(rej("size_unknown", "release size is unknown"));
        return u64::MAX;
    };
    if let Some(policy) = ctx.policy()
        && policy.maximum_size_mb > 0
        && size as f64 > policy.maximum_size_mb as f64 * MIB
    {
        out.push(rej(
            "size_above_global_maximum",
            format!(
                "{} MiB exceeds the configured maximum of {} MB",
                size / (1 << 20),
                policy.maximum_size_mb
            ),
        ));
    }
    if quality == Quality::Unknown {
        return u64::MAX;
    }
    let Some(limit) = ctx.settings.size_limit(ctx.media(), quality) else {
        out.push(rej(
            "size_limits_missing",
            format!("no quality definition for {quality}"),
        ));
        return u64::MAX;
    };
    let constrained = limit.min_mib_per_minute > 0.0 || limit.max_mib_per_minute.is_some();
    let Some(runtime) = runtime_total(ctx, cov) else {
        if constrained {
            out.push(rej(
                "size_unverifiable_runtime",
                "runtime is unknown, so the size limits cannot be checked",
            ));
        }
        return u64::MAX;
    };
    let mib = size as f64 / MIB;
    let min = limit.min_mib_per_minute * runtime;
    if mib < min {
        out.push(rej(
            "size_below_minimum",
            format!(
                "{mib:.0} MiB is below the {min:.0} MiB minimum for {quality} over {runtime:.0} min"
            ),
        ));
    }
    if let Some(max) = limit.max_mib_per_minute.map(|m| m * runtime)
        && mib > max
    {
        out.push(rej(
            "size_above_maximum",
            format!(
                "{mib:.0} MiB is above the {max:.0} MiB maximum for {quality} over {runtime:.0} min"
            ),
        ));
    }
    limit
        .preferred_mib_per_minute
        .map(|p| ((p * runtime - mib).abs() * MIB) as u64)
        .unwrap_or(u64::MAX)
}

pub fn evaluate(
    ctx: &EvalContext,
    r: &ProwlarrRelease,
    p: &ParsedRelease,
    blockers: &[Rejection],
) -> Decision {
    let mut out: Vec<Rejection> = blockers.to_vec();
    let mut notes = vec![];
    let mut owned = vec![];
    let media = ctx.media();

    let swarm = health(r.seeders, r.leechers);
    check_title(ctx, p, &mut out, &mut notes);
    let cov = check_scope(ctx, p, swarm, &mut out, &mut owned);
    let (skips_owned, skips_active, supersedes, grab_episodes) = match &cov.pack {
        Some(sp) => (
            sp.skips_owned.clone(),
            sp.skips_active.clone(),
            sp.supersedes.clone(),
            Some(sp.get.clone()),
        ),
        None => (vec![], vec![], vec![], None),
    };
    if !skips_owned.is_empty() {
        notes.push(format!(
            "{} won't be downloaded: already owned",
            ep_list(&skips_owned)
        ));
    }
    if !skips_active.is_empty() {
        notes.push(format!(
            "{} stay as they are: already downloading",
            ep_list(&skips_active)
        ));
    }
    if !supersedes.is_empty() {
        notes.push(format!(
            "{} would be taken over from downloads that are idle or not started; those are cancelled",
            ep_list(&supersedes)
        ));
    }

    if let Target::Movie(MovieTarget {
        ownership,
        is_available,
        ..
    }) = ctx.target
    {
        match ownership {
            Ownership::Owned => {
                if let Target::Movie(m) = ctx.target
                    && let Err(why) = revision_upgrade(ctx, m.existing.as_ref(), p)
                {
                    out.push(rej(
                        "covers_owned",
                        format!("the movie already has a file: {why}"),
                    ));
                }
            }
            Ownership::Unknown => out.push(rej("ownership_unknown", "movie ownership is unknown")),
            Ownership::Missing => {}
        }
        if *is_available == Some(false) {
            let note = match ctx.target {
                Target::Movie(m) => m.availability_note.clone(),
                _ => None,
            };
            out.push(rej(
                "not_available",
                note.unwrap_or_else(|| {
                    "the movie is not available yet (minimum availability)".into()
                }),
            ));
        }
        if let Target::Movie(m) = ctx.target
            && m.queued
        {
            out.push(rej(
                "covers_queued",
                "the movie is already being downloaded",
            ));
        }
    }
    if let Some(blocked) = ctx.blocked {
        let hash = r
            .info_hash
            .as_deref()
            .map(|h| format!("hash:{}", h.trim().to_ascii_lowercase()));
        if blocked.contains(&crate::search::release_ref(r))
            || hash.is_some_and(|h| blocked.contains(&h))
        {
            out.push(rej(
                "blocklisted",
                "the release is on the blocklist for this item",
            ));
        }
    }

    let words = title_words(ctx);
    let extras: Vec<&String> = p
        .extra_markers
        .iter()
        .filter(|m| !m.split(' ').all(|w| words.iter().any(|t| t == w)))
        .collect();
    if !extras.is_empty() {
        out.push(rej(
            "sample_or_extra",
            format!("sample/extras markers {extras:?}"),
        ));
    }
    let bundle: Vec<&String> = p
        .bundle_markers
        .iter()
        .filter(|m| !words.contains(m))
        .collect();
    if !bundle.is_empty() {
        out.push(rej(
            "multi_movie_bundle",
            format!("bundle/collection markers {bundle:?}"),
        ));
    }
    if p.three_d {
        out.push(rej(
            "three_d",
            "3D release; no imported policy covers 3D variants",
        ));
    }
    if p.dangerous_file_type {
        out.push(rej(
            "dangerous_file_type",
            "release title ends in an executable/script extension",
        ));
    }
    if let Some(policy) = ctx.policy() {
        if p.hardcoded_subs && !policy.allow_hardcoded_subs {
            out.push(rej(
                "hardcoded_subs",
                "hardcoded-subtitle marker and allowHardcodedSubs is false",
            ));
        }
        if !policy.torrent_enabled {
            out.push(rej(
                "protocol_disabled",
                "the delay profile disables torrents",
            ));
        }
    }

    let tier = ctx.profile.and_then(|prof| prof.tier_of(p.quality));
    match (p.quality, p.quality_basis) {
        (Quality::Unknown, QualityBasis::Conflicting) => {
            out.push(rej("quality_unknown", "conflicting source markers"))
        }
        (Quality::Unknown, _) => out.push(rej(
            "quality_unknown",
            "quality could not be determined from the title",
        )),
        (q, basis) => {
            if ctx.profile.is_some() && tier.is_none() {
                out.push(rej(
                    "quality_not_allowed",
                    format!(
                        "{q} is not allowed by profile {}",
                        ctx.profile_id.as_deref().unwrap_or("?")
                    ),
                ));
            }
            if basis == QualityBasis::InferredFromResolution {
                notes.push(format!(
                    "{INFERRED_PREFIX} {q} inferred from resolution (native convention)"
                ));
            }
        }
    }
    if let Some(profile) = ctx.profile {
        check_language(ctx, p, profile, &mut out, &mut notes);
    }
    let tv_scope_rejected = ctx.season_target().is_some() && cov.episodes.is_empty();
    let preferred_distance = if tv_scope_rejected {
        u64::MAX
    } else {
        check_size(ctx, r, p.quality, &cov, &mut out)
    };

    if !r
        .protocol
        .as_deref()
        .is_some_and(|x| x.eq_ignore_ascii_case("torrent"))
    {
        out.push(rej(
            "protocol_unsupported",
            format!(
                "protocol {:?}; only torrent is supported",
                r.protocol.as_deref().unwrap_or("unknown")
            ),
        ));
    }
    if r.download_url.is_none() && r.magnet_url.is_none() {
        out.push(rej(
            "no_download_link",
            "Prowlarr returned no download or magnet link",
        ));
    }
    let indexer = r
        .indexer_id
        .and_then(|i| u32::try_from(i).ok())
        .and_then(|i| ctx.settings.indexer(media, i));
    match indexer {
        None => out.push(rej(
            "indexer_not_configured",
            format!(
                "indexer {:?} is not an imported {media:?} indexer",
                r.indexer_id
            ),
        )),
        Some(ix) => {
            if !ix.unsupported.is_empty() {
                out.push(rej(
                    "indexer_policy_unsupported",
                    format!("indexer rules {:?} are not evaluated", ix.unsupported),
                ));
            }
            match r.seeders {
                None => out.push(rej("seeders_unknown", "seeder count unknown")),
                Some(s) if s < i64::from(ix.minimum_seeders) => out.push(rej(
                    "below_minimum_seeders",
                    format!("{s} seeders; indexer minimum is {}", ix.minimum_seeders),
                )),
                Some(_) => {}
            }
        }
    }
    if let Some(policy) = ctx.policy()
        && policy.torrent_delay_minutes > 0
    {
        let top = ctx.profile.map(|p| p.tiers.len().saturating_sub(1));
        let bypass = policy.bypass_delay_if_highest_quality && tier.is_some() && tier == top;
        match r
            .publish_date
            .as_deref()
            .and_then(|d| OffsetDateTime::parse(d, &Rfc3339).ok())
        {
            None if !bypass => out.push(rej(
                "release_age_unknown",
                "publish date unknown; the delay profile cannot be applied",
            )),
            Some(t) if !bypass && (ctx.now - t).whole_minutes() < policy.torrent_delay_minutes => {
                out.push(rej(
                    "delay_profile_wait",
                    format!(
                        "release is younger than the {} minute torrent delay",
                        policy.torrent_delay_minutes
                    ),
                ))
            }
            _ => {}
        }
    }
    if let Some(obs) = &ctx.catalog_observed_at {
        let in_catalog = match ctx.target {
            Target::Movie(m) => m.in_catalog,
            Target::Season(s) | Target::Episode { season: s, .. } => s.series.in_catalog,
        };
        if in_catalog {
            notes.push(format!("ownership per catalog snapshot observed {obs}"));
        }
    }
    out.dedup();
    let uncertainty = notes
        .iter()
        .filter(|n| *n == TITLE_ONLY_NOTE || n.starts_with(INFERRED_PREFIX))
        .count() as u8;
    let replacement = out.is_empty()
        && match ctx.target {
            Target::Movie(m) => m.ownership == Ownership::Owned,
            _ => !owned.is_empty(),
        };
    Decision {
        uncertainty,
        replacement,
        approved: out.is_empty(),
        rejections: out,
        notes,
        covered_episodes: grab_episodes.unwrap_or(cov.episodes),
        owned_overlap: owned,
        full_season: cov.full_season,
        tier,
        health: swarm,
        skips_owned,
        skips_active,
        supersedes,
        preferred_distance,
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;
    use Health::{Healthy, Ok as Fine, Weak};

    #[test]
    fn bands_and_their_boundaries() {
        let h = |s, l| health(Some(s), Some(l));
        // 10+ seeders: healthy while leechers are at most 10x the seeders.
        assert_eq!(h(10, 0), Healthy);
        assert_eq!(h(10, 100), Healthy);
        assert_eq!(h(10, 101), Fine);
        assert_eq!(h(9, 9), Healthy, "5-9 seeders need leechers <= seeders");
        assert_eq!(h(9, 10), Fine);
        assert_eq!(h(5, 5), Healthy);
        assert_eq!(h(5, 6), Fine);
        assert_eq!(h(4, 0), Fine);
        assert_eq!(h(3, 500), Fine);
        assert_eq!(h(2, 0), Weak);
        assert_eq!(h(0, 0), Weak);
        assert_eq!(h(49, 100_000), Fine);
        assert_eq!(h(50, 100_000), Healthy);
        // Unknown counts are the worst case.
        assert_eq!(health(None, Some(0)), Weak);
        assert_eq!(health(None, None), Weak);
        assert_eq!(health(Some(-1), Some(0)), Weak);
        assert_eq!(health(Some(30), None), Fine);
        assert_eq!(health(Some(4), None), Fine);
        assert_eq!(health(Some(60), None), Healthy);
        assert_eq!(health(Some(2), None), Weak);
        // The Futurama S05 evidence.
        assert_eq!(h(0, 6), Weak);
        assert_eq!(h(2, 0), Weak);
        assert_eq!(h(8, 3), Healthy);
        assert_eq!(h(27, 4), Healthy);
        assert_eq!(h(48, 3), Healthy);
    }
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    fn release(ratio: Option<f64>, secs: Option<i64>) -> ProwlarrRelease {
        serde_json::from_value(
            serde_json::json!({ "minimumRatio": ratio, "minimumSeedTime": secs }),
        )
        .unwrap()
    }

    #[test]
    fn tracker_seed_requirements_against_the_seed_goal() {
        // No requirement: nothing to check.
        assert!(seed_requirement(&release(None, None), None, None).is_none());
        // Ratio 1.0 required; a ratio goal of 2 satisfies it, an extra time goal could end
        // seeding early (the time half has no matching requirement).
        assert!(seed_requirement(&release(Some(1.0), None), Some(2.0), None).is_none());
        let r = seed_requirement(&release(Some(1.0), None), Some(2.0), Some(60)).unwrap();
        assert_eq!(r.code, "seed_requirement_unmet");
        assert!(r.message.contains("after 60 min"), "{}", r.message);
        // 72 h required; a 72 h goal satisfies it, 24 h does not; no goal at all never does.
        assert!(seed_requirement(&release(None, Some(259_200)), None, Some(4320)).is_none());
        assert!(seed_requirement(&release(None, Some(259_200)), None, Some(1440)).is_some());
        assert!(
            seed_requirement(&release(Some(1.0), Some(259_200)), None, None)
                .unwrap()
                .message
                .contains("no seed goal")
        );
        // Negative goals are unset (the client's limits).
        assert!(seed_requirement(&release(Some(1.0), None), Some(-1.0), None).is_some());
        // Prowlarr's JSON names are read too.
        assert_eq!(release(Some(0.5), Some(10)).minimum_seed_time, Some(10));
    }
}
