//! RSS sync: the routine acquisition path, through the configured search backend.
//!
//! Each indexer with `enableRss` in the settings is read through the backend that serves it:
//! a native indexer's own recent feed (The Pirate Bay's `data_top100_recent.json`, or a
//! Torznab endpoint's `t=search` without a query), or the configured Prowlarr's per-indexer
//! Torznab feed (`{base}{indexer}/api?t=search&cat=...&limit=100`). Bodies are bounded. The
//! XML parser (`torznab::parse_feed`) rejects DTDs (no entity expansion, nothing fetched),
//! bounds the node count and reads only the RSS 2.0 item children and Torznab/Newznab `attr`
//! elements it needs. Download links are kept only when they are magnets or on the
//! backend's own (or explicitly configured) origins, and stay private.
//!
//! Items are matched only to managed, monitored catalog items by canonical title/alias keys
//! with year and numbering; ambiguous or untracked titles are ignored. Matched items go
//! through the same decision engine as searches (profile, size, seeders, delay, ownership,
//! queued coverage, blocklist, air dates, native revision upgrades) and are grabbed through
//! the normal outbox. A per-item `rss_seen` record makes replays and restarts idempotent (a
//! grabbed item is never grabbed again); per-indexer state advances only after a successful
//! fetch and processing, and failing indexers back off. A feed is a window of the latest
//! releases: retrieval status is reported, provider completeness is not promised, and a
//! possible gap (the window was full of releases newer than the last sync) is flagged.

use crate::api::AppState;
use crate::catalog::CatalogIndex;
use crate::decision::{self, EvalContext, Scope};
use crate::downloads::{self, EnqueueError, EnqueueRequest};
use crate::ids::{MediaId, SeriesKey};
use crate::import::rfc3339;
use crate::indexer::{Backend, FeedWindow};
use crate::prowlarr::ProwlarrRelease;
use crate::release::{self, Numbering, ParsedRelease};
use crate::search::{self, Candidate};
use crate::settings::{MediaKind, Settings};
use crate::target::{Resolver, Target};
use crate::title;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub use crate::torznab::{MAX_ITEMS, NEWZNAB_NS, TORZNAB_NS};
/// Items requested per indexer feed.
pub const FEED_LIMIT: u32 = 100;
/// Grabs started by one sync at most (the rest wait for the next sync).
pub const MAX_GRABS_PER_SYNC: usize = 20;
pub const DEFAULT_INTERVAL_MINUTES: u32 = 15;
/// Rejections that can change while an item stays in the feed: re-evaluated next sync.
const TRANSIENT: &[&str] = &[
    "delay_profile_wait",
    "covers_queued",
    "episode_not_aired",
    "season_not_fully_aired",
    "release_age_unknown",
    "not_available",
    "ownership_unknown",
    "episode_list_unknown",
];

/// A parsed feed: items as Prowlarr releases (private links), plus counters.
#[derive(Debug, Default)]
pub struct Feed {
    pub items: Vec<ProwlarrRelease>,
    pub links_dropped: usize,
    pub truncated: bool,
}

/// Parses an untrusted Torznab/RSS 2.0 document from the configured Prowlarr: links are
/// kept only when magnets or on the Prowlarr origin (`torznab::parse_feed`).
pub fn parse(xml: &[u8], origin: &reqwest::Url, indexer_id: u32) -> Result<Feed, String> {
    let allowed = std::slice::from_ref(origin);
    crate::torznab::parse_feed(
        xml,
        &crate::torznab::LinkPolicy::origins(allowed),
        indexer_id,
        None,
    )
    .map(|p| Feed {
        items: p.items,
        links_dropped: p.links_dropped,
        truncated: p.truncated,
    })
    .map_err(|e| e.message())
}

/// What a feed item refers to in the managed catalog.
#[derive(Debug, Clone, PartialEq)]
pub enum Match {
    Movie(MediaId),
    Season(SeriesKey, u16),
    Ignored(String),
}

fn media_hint(r: &ProwlarrRelease) -> Option<MediaKind> {
    let ids: Vec<i64> = r.categories.iter().flatten().filter_map(|c| c.id).collect();
    let movie = ids.iter().any(|c| (2000..3000).contains(c));
    let tv = ids.iter().any(|c| (5000..6000).contains(c));
    match (movie, tv) {
        (true, false) => Some(MediaKind::Movie),
        (false, true) => Some(MediaKind::Series),
        _ => None,
    }
}

fn managed(app: &str) -> bool {
    !app.ends_with("(unmanaged)")
}

/// Unambiguous managed, monitored target for a feed item (title keys + year + numbering).
pub fn match_item(index: &CatalogIndex, r: &ProwlarrRelease, p: &ParsedRelease) -> Match {
    let key = title::key_from_tokens(&p.title_tokens);
    if key.is_empty() {
        return Match::Ignored("no title".into());
    }
    let hint = media_hint(r);
    match &p.numbering {
        Numbering::Episodes { season, .. } | Numbering::Season { season } => {
            if hint == Some(MediaKind::Movie) {
                return Match::Ignored("TV numbering in a movie category".into());
            }
            let mut hits: Vec<_> = index
                .series_with_key(&key, None)
                .into_iter()
                .filter(|s| managed(&s.provenance.source_app))
                .collect();
            if hits.len() > 1
                && let Some(y) = p.year
            {
                hits.retain(|s| {
                    s.year == Some(y) || title::split_trailing_year(&s.title).1 == Some(y)
                });
            }
            hits.dedup_by_key(|s| s.id);
            match hits.as_slice() {
                [] => Match::Ignored("not a managed series".into()),
                [s] if !s.monitored => Match::Ignored("the series is not monitored".into()),
                [s] if s.series_type != "standard" => {
                    Match::Ignored(format!("{} series are not supported", s.series_type))
                }
                [s] if s.episodes.is_none() => {
                    Match::Ignored("the series has no episode list yet (refresh_metadata)".into())
                }
                [s] => {
                    // A season left unmonitored as a whole can still hold individually
                    // monitored episodes (an episode-scoped add): releases naming only such
                    // episodes match. Season packs and other episodes of that season do not.
                    if s.seasons
                        .iter()
                        .any(|x| x.number == *season && !x.monitored)
                    {
                        let individually_monitored = match &p.numbering {
                            Numbering::Episodes { episodes, .. } => {
                                !episodes.is_empty()
                                    && episodes.iter().all(|n| {
                                        s.episodes.iter().flatten().any(|e| {
                                            e.season == *season && e.number == *n && e.monitored
                                        })
                                    })
                            }
                            _ => false,
                        };
                        if !individually_monitored {
                            return Match::Ignored(format!("season {season} is not monitored"));
                        }
                    }
                    match s.id.series_key() {
                        Some(k) => Match::Season(k, *season),
                        None => Match::Ignored("invalid series id".into()),
                    }
                }
                _ => Match::Ignored("ambiguous: several managed series share this title".into()),
            }
        }
        Numbering::None => {
            if hint == Some(MediaKind::Series) {
                return Match::Ignored("no episode numbering in a TV category".into());
            }
            let Some(year) = p.year else {
                return Match::Ignored("movie title without a year".into());
            };
            let hits: Vec<_> = index
                .movies_with_key(&key, None)
                .into_iter()
                .filter(|m| managed(&m.provenance.source_app))
                .filter(|m| m.year == Some(year) || m.secondary_year == Some(year))
                .collect();
            match hits.as_slice() {
                [] => Match::Ignored("not a managed movie".into()),
                [m] if !m.monitored => Match::Ignored("the movie is not monitored".into()),
                [m] => Match::Movie(m.id),
                _ => Match::Ignored(
                    "ambiguous: several managed movies share this title and year".into(),
                ),
            }
        }
        _ => Match::Ignored(
            "unsupported numbering (multi-season, partial season, daily or complete series)".into(),
        ),
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct IndexerSync {
    pub indexer_id: u32,
    /// `ok`, `failed`, `backoff` (skipped until `next_attempt_at`) or `unavailable` (the
    /// configured backend cannot serve this indexer's feed; nothing was requested).
    pub status: String,
    pub items: usize,
    pub error: Option<String>,
    pub gap_possible: bool,
    pub links_dropped: usize,
    /// Items in the indexer's whole window (before category filtering), when it differs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<usize>,
    /// Items dropped by the backend (unrelated category, filters, unusable fields).
    pub dropped: usize,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct SyncReport {
    pub indexers: Vec<IndexerSync>,
    pub items: usize,
    pub matched: usize,
    pub grabbed: usize,
    pub rejected: usize,
    pub deferred: usize,
    pub ignored: usize,
    pub already_grabbed: usize,
    /// `complete` when every due feed was read, `incomplete` otherwise.
    pub retrieval: &'static str,
    /// Prowlarr: always `unverified`. Native indexers: `complete_window` when every due feed
    /// answered (a window of the latest releases, never a promise of gap-free coverage), else
    /// `incomplete`.
    pub provider_completeness: &'static str,
    pub note: &'static str,
}

const NOTE: &str = "a feed lists only the latest releases; anything published between syncs beyond that window can be missed (gap_possible). Native indexers report their own status per feed; Prowlarr does not report per-indexer completeness";

/// Effective RSS interval: the shortest imported `rssSyncInterval` that is enabled, else the
/// native default. `None` when every media kind disables RSS.
pub fn interval(settings: &Settings, override_minutes: Option<u32>) -> Option<Duration> {
    if let Some(m) = override_minutes {
        return (m > 0).then(|| Duration::from_secs(u64::from(m) * 60));
    }
    let mins: Vec<u32> = [MediaKind::Movie, MediaKind::Series]
        .iter()
        .filter_map(|m| {
            settings.policy(*m).map(|p| {
                p.rss_sync_interval_minutes
                    .unwrap_or(DEFAULT_INTERVAL_MINUTES)
            })
        })
        .filter(|m| *m > 0)
        .collect();
    mins.into_iter()
        .min()
        .map(|m| Duration::from_secs(u64::from(m.max(10)) * 60))
}

fn rss_enabled(settings: &Settings, media: MediaKind) -> bool {
    settings
        .policy(media)
        .is_none_or(|p| p.rss_sync_interval_minutes != Some(0))
}

/// Feeds to read: imported RSS-enabled indexers (by Prowlarr id) with their categories.
pub fn feeds(settings: &Settings) -> BTreeMap<u32, Vec<u32>> {
    let mut out: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for ix in settings.indexers.iter().filter(|i| {
        i.enable_rss && i.protocol.eq_ignore_ascii_case("torrent") && rss_enabled(settings, i.media)
    }) {
        out.entry(ix.prowlarr_indexer_id)
            .or_default()
            .extend(ix.categories.iter().copied());
    }
    out.into_iter()
        .map(|(k, v)| (k, v.into_iter().collect()))
        .collect()
}

/// Whether releases may have been published after the last successful sync and dropped out of
/// the feed before this read. Only a window reaching back to (or past) the newest release the
/// last sync saw rules that out; the number of rows is no evidence either way, because a feed
/// can return fewer than its nominal limit (TPB's recent feed answered 50 of 100) and still
/// have moved past the last sync. Without a baseline (the first sync) there is nothing to
/// miss; a window without any dated item cannot show overlap, so it counts as a possible gap.
/// Overlap proves only that the window is contiguous in time with the last one, not that the
/// provider lists every release.
fn gap_possible(previous_newest: Option<&str>, window_oldest: Option<&str>) -> bool {
    previous_newest.is_some_and(|n| window_oldest.is_none_or(|o| o > n))
}

fn backoff(failures: i64, base: Duration) -> Duration {
    (base * (1u32 << failures.clamp(0, 5) as u32)).min(Duration::from_secs(6 * 3600))
}

/// One indexer's feed through the configured backend, or why it could not be read.
async fn read_feed(
    backend: &Backend,
    settings: &Settings,
    indexer: u32,
    cats: &[u32],
) -> Result<FeedWindow, (bool, String)> {
    match backend {
        Backend::Prowlarr(p) => match p.feed(indexer, cats, FEED_LIMIT).await {
            Ok(bytes) => parse(&bytes, p.base_url(), indexer)
                .map(|f| {
                    let dates = f.items.iter().filter_map(|i| i.publish_date.clone());
                    let (oldest, newest) = (dates.clone().min(), dates.max());
                    FeedWindow {
                        window: f.items.len(),
                        oldest,
                        newest,
                        links_dropped: f.links_dropped,
                        dropped: 0,
                        truncated: f.truncated,
                        items: f.items,
                    }
                })
                .map_err(|e| (true, e)),
            Err(e) => Err((true, format!("prowlarr {}", e.describe()))),
        },
        Backend::Native(r) => {
            let Some(ix) = r.get(indexer) else {
                return Err((
                    false,
                    format!("no [[indexer]] with id {indexer} is configured; its feed is not read"),
                ));
            };
            if let Some(why) = settings
                .indexers
                .iter()
                .filter(|s| s.prowlarr_indexer_id == indexer)
                .find_map(|s| r.unusable(s))
                .map(|(_, why)| why)
                .or_else(|| ix.rss_unsupported())
            {
                return Err((false, why));
            }
            ix.feed(cats, FEED_LIMIT)
                .await
                .map_err(|e| (true, format!("{} {}", ix.label(), e.describe())))
        }
    }
}

/// One RSS sync pass over every due feed.
pub async fn sync(st: &AppState, now: OffsetDateTime, force: bool) -> Result<SyncReport> {
    let mut report = SyncReport {
        retrieval: "complete",
        provider_completeness: "unverified",
        note: NOTE,
        ..Default::default()
    };
    let (Some(l), Some(backend)) = (st.current(), st.search.clone()) else {
        report.retrieval = "incomplete";
        return Ok(report);
    };
    let native = matches!(backend, Backend::Native(_));
    let settings = &l.settings;
    let base_interval = interval(settings, st.cfg.automation.rss_interval_minutes)
        .unwrap_or(Duration::from_secs(900));
    let mut fetched: Vec<(u32, Vec<ProwlarrRelease>, Option<String>)> = vec![];
    for (indexer, cats) in feeds(settings) {
        let state: Option<(Option<String>, Option<String>, i64)> = st
            .store
            .with(move |c| {
                Ok(c.query_row("SELECT next_attempt_at, newest_pub, failures FROM rss_state WHERE indexer_id = ?1", [indexer], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .optional()?)
            })
            .await?;
        let (next, newest, failures) = state.unwrap_or((None, None, 0));
        if !force
            && let Some(n) = next
                .as_deref()
                .and_then(|n| OffsetDateTime::parse(n, &Rfc3339).ok())
            && n > now
        {
            report.indexers.push(IndexerSync {
                indexer_id: indexer,
                status: "backoff".into(),
                ..Default::default()
            });
            report.retrieval = "incomplete";
            continue;
        }
        match read_feed(&backend, settings, indexer, &cats).await {
            Ok(feed) => {
                let gap = gap_possible(newest.as_deref(), feed.oldest.as_deref());
                report.indexers.push(IndexerSync {
                    indexer_id: indexer,
                    status: "ok".into(),
                    items: feed.items.len(),
                    error: None,
                    gap_possible: gap,
                    links_dropped: feed.links_dropped,
                    window: (feed.window != feed.items.len()).then_some(feed.window),
                    dropped: feed.dropped,
                });
                fetched.push((indexer, feed.items, feed.newest));
            }
            Err((requested, e)) => {
                tracing::warn!(provider = backend.name(), indexer, failure = %e, "rss feed failed");
                let (status, failures_inc) = if requested {
                    ("failed", 1)
                } else {
                    ("unavailable", 0)
                };
                let next = rfc3339(now + backoff(failures + failures_inc, base_interval));
                let (e2, now_s) = (e.clone(), rfc3339(now));
                st.store
                    .with(move |c| {
                        c.execute(
                            "INSERT INTO rss_state (indexer_id, last_attempt_at, last_status, last_error, failures, next_attempt_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                             ON CONFLICT (indexer_id) DO UPDATE SET last_attempt_at = ?2, last_status = ?3, last_error = ?4, failures = failures + ?5, next_attempt_at = ?6",
                            params![indexer, now_s, status, e2, failures_inc, next],
                        )?;
                        Ok(())
                    })
                    .await?;
                report.indexers.push(IndexerSync {
                    indexer_id: indexer,
                    status: status.into(),
                    error: Some(e),
                    ..Default::default()
                });
                report.retrieval = "incomplete";
            }
        }
    }
    if native {
        report.provider_completeness = if report.retrieval == "complete" {
            "complete_window"
        } else {
            "incomplete"
        };
    }
    // Evaluate everything fetched, best first, then record state per indexer.
    let mut seen_keys = BTreeSet::new();
    let mut items: Vec<ProwlarrRelease> = vec![];
    for (_, list, _) in &fetched {
        for r in list {
            if seen_keys.insert(r.dedupe_key()) {
                items.push(r.clone());
            }
        }
    }
    report.items = items.len();
    process(st, settings, items, now, &mut report).await?;
    for (indexer, list, newest) in fetched {
        let gap = report
            .indexers
            .iter()
            .any(|i| i.indexer_id == indexer && i.gap_possible);
        let (now_s, n) = (rfc3339(now), list.len() as i64);
        let next = rfc3339(now + base_interval);
        st.store
            .with(move |c| {
                c.execute(
                    "INSERT INTO rss_state (indexer_id, last_attempt_at, last_success_at, newest_pub, last_status, last_error, items, gap_possible, failures, next_attempt_at)
                     VALUES (?1, ?2, ?2, ?3, 'ok', NULL, ?4, ?5, 0, ?6)
                     ON CONFLICT (indexer_id) DO UPDATE SET last_attempt_at = ?2, last_success_at = ?2, newest_pub = MAX(COALESCE(newest_pub, ''), COALESCE(?3, '')),
                       last_status = 'ok', last_error = NULL, items = ?4, gap_possible = ?5, failures = 0, next_attempt_at = ?6",
                    params![indexer, now_s, newest, n, gap, next],
                )?;
                Ok(())
            })
            .await?;
    }
    let cutoff = rfc3339(now - Duration::from_secs(30 * 86_400));
    st.store
        .with(move |c| {
            Ok(
                c.execute("DELETE FROM rss_seen WHERE last_seen < ?1", [cutoff])
                    .map(|_| ())?,
            )
        })
        .await?;
    Ok(report)
}

async fn mark(
    st: &AppState,
    r: &ProwlarrRelease,
    status: &str,
    detail: String,
    grab: Option<String>,
    now: OffsetDateTime,
) -> Result<()> {
    let (key, indexer, title, published) = (
        r.dedupe_key(),
        r.indexer_id.unwrap_or(0),
        r.title.clone().unwrap_or_default(),
        r.publish_date.clone(),
    );
    let (status, now_s) = (status.to_string(), rfc3339(now));
    let key = crate::import::sha256_hex(key.as_bytes());
    st.store
        .with(move |c| {
            c.execute(
                "INSERT INTO rss_seen (key, indexer_id, title, published_at, first_seen, last_seen, status, detail, grab_id) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8)
                 ON CONFLICT (key) DO UPDATE SET last_seen = ?5, status = CASE WHEN status = 'grabbed' THEN status ELSE ?6 END, detail = CASE WHEN status = 'grabbed' THEN detail ELSE ?7 END, grab_id = COALESCE(grab_id, ?8)",
                params![key, indexer, title.chars().take(300).collect::<String>(), published, now_s, status, detail, grab],
            )?;
            Ok(())
        })
        .await
}

struct Evaluated {
    candidate: Candidate,
    target: Target,
}

async fn process(
    st: &AppState,
    settings: &Settings,
    items: Vec<ProwlarrRelease>,
    now: OffsetDateTime,
    report: &mut SyncReport,
) -> Result<()> {
    let native = st.search.as_ref().is_some_and(|b| b.native().is_some());
    let index = st.index();
    let resolver = Resolver {
        availability_delay_days: st.availability_delay_days(),
        catalog: Some(&index),
        tvmaze: None,
        movies: None,
        now,
    };
    // Already grabbed (by key) items are skipped outright.
    let keys: Vec<String> = items
        .iter()
        .map(|r| crate::import::sha256_hex(r.dedupe_key().as_bytes()))
        .collect();
    let grabbed: BTreeSet<String> = st
        .store
        .with(move |c| {
            let mut out = BTreeSet::new();
            let mut stmt =
                c.prepare("SELECT 1 FROM rss_seen WHERE key = ?1 AND status = 'grabbed'")?;
            for k in keys {
                if stmt.exists([&k])? {
                    out.insert(k);
                }
            }
            Ok(out)
        })
        .await?;
    let mut targets: HashMap<String, Target> = HashMap::new();
    let mut evaluated: Vec<Evaluated> = vec![];
    for r in items {
        if grabbed.contains(&crate::import::sha256_hex(r.dedupe_key().as_bytes())) {
            report.already_grabbed += 1;
            mark(
                st,
                &r,
                "grabbed",
                "already grabbed from an earlier sync".into(),
                None,
                now,
            )
            .await?;
            continue;
        }
        let parsed = release::parse(r.title.as_deref().unwrap_or(""));
        let id = match match_item(&index, &r, &parsed) {
            Match::Ignored(why) => {
                report.ignored += 1;
                mark(st, &r, "ignored", why, None, now).await?;
                continue;
            }
            Match::Movie(id) => id,
            Match::Season(k, s) => MediaId::Season(k, s),
        };
        let media = downloads::policy_kind(id);
        if !r
            .indexer_id
            .and_then(|i| u32::try_from(i).ok())
            .and_then(|i| settings.indexer(media, i))
            .is_some_and(|ix| ix.enable_rss)
        {
            report.ignored += 1;
            mark(
                st,
                &r,
                "ignored",
                format!("the indexer is not RSS-enabled for {}", media.label()),
                None,
                now,
            )
            .await?;
            continue;
        }
        report.matched += 1;
        let key = id.to_string();
        if !targets.contains_key(&key) {
            match resolver.target(id).await {
                Ok(t) => {
                    targets.insert(key.clone(), t);
                }
                Err(e) => {
                    report.deferred += 1;
                    mark(st, &r, "deferred", e.message, None, now).await?;
                    continue;
                }
            }
        }
        let target = targets[&key].clone();
        let media_id = match &target {
            Target::Movie(m) => m.id,
            Target::Season(s) | Target::Episode { season: s, .. } => s.series.id,
        };
        let blocked = st
            .store
            .with(move |c| downloads::blocked_for(c, media_id))
            .await?;
        let profile = match search::profile_for(settings, &target, None) {
            Ok(p) => p,
            Err(e) => {
                report.rejected += 1;
                mark(st, &r, "rejected", e.message, None, now).await?;
                continue;
            }
        };
        let scope = match &target {
            Target::Movie(_) => Scope::Movie,
            Target::Season(s) | Target::Episode { season: s, .. } => Scope::Season(s.season),
        };
        let ctx = EvalContext {
            target: &target,
            scope,
            settings,
            profile: profile.0,
            profile_id: profile.1.clone(),
            catalog: Some(&index),
            now,
            catalog_observed_at: None,
            blocked: Some(&blocked),
            partial_packs: st.can_select_files(),
        };
        let blockers = ctx.target_blockers();
        let decision = decision::evaluate(&ctx, &r, &parsed, &blockers);
        // RSS also requires every covered episode to be monitored (native).
        let mut decision = decision;
        search::apply_seed_requirement(&mut decision, &r, settings, media, native, &parsed);
        if let Target::Season(s) = &target
            && let Some(series) = s.series.id.series_key().and_then(|k| index.series(k))
        {
            let unmonitored: Vec<u16> = decision
                .covered_episodes
                .iter()
                .copied()
                .filter(|n| {
                    series
                        .episodes
                        .iter()
                        .flatten()
                        .any(|e| e.season == s.season && e.number == *n && !e.monitored)
                })
                .collect();
            if !unmonitored.is_empty() {
                decision.approved = false;
                decision.rejections.push(decision::Rejection {
                    code: "not_monitored".into(),
                    message: format!("episodes {unmonitored:?} are not monitored"),
                });
            }
        }
        if !decision.approved {
            let transient = decision
                .rejections
                .iter()
                .all(|x| TRANSIENT.contains(&x.code.as_str()));
            let codes: Vec<&str> = decision
                .rejections
                .iter()
                .map(|x| x.code.as_str())
                .collect();
            if transient {
                report.deferred += 1;
                mark(st, &r, "deferred", codes.join(", "), None, now).await?;
            } else {
                report.rejected += 1;
                mark(st, &r, "rejected", codes.join(", "), None, now).await?;
            }
            continue;
        }
        let indexer_priority = r
            .indexer_id
            .and_then(|i| u32::try_from(i).ok())
            .and_then(|i| settings.indexer(media, i))
            .map(|i| i.priority)
            .unwrap_or(i32::MAX);
        evaluated.push(Evaluated {
            candidate: Candidate {
                release_ref: search::release_ref(&r),
                release: r,
                parsed,
                decision,
                found_by: vec![],
                indexer_priority,
            },
            target,
        });
    }
    // Best first per target; nothing covering an already chosen unit.
    let prefer = |m: MediaKind| {
        settings
            .policy(m)
            .is_none_or(|p| p.prefer_propers_and_repacks)
    };
    let mut by_media: BTreeMap<bool, Vec<Evaluated>> = BTreeMap::new();
    for e in evaluated {
        by_media
            .entry(matches!(e.target, Target::Movie(_)))
            .or_default()
            .push(e);
    }
    let mut taken: BTreeSet<MediaId> = BTreeSet::new();
    let mut grabs = 0;
    for (is_movie, mut list) in by_media {
        let prefer_revision = prefer(if is_movie {
            MediaKind::Movie
        } else {
            MediaKind::Series
        });
        list.sort_by(|a, b| search::compare(&a.candidate, &b.candidate, prefer_revision));
        for e in &list {
            let (c, target) = (&e.candidate, &e.target);
            let Some(link) = downloads::link_for(c, target, false) else {
                continue;
            };
            if link.covers.iter().any(|t| taken.contains(t)) {
                report.deferred += 1;
                mark(
                    st,
                    &c.release,
                    "deferred",
                    "a better release for the same item was grabbed in this sync".into(),
                    None,
                    now,
                )
                .await?;
                continue;
            }
            if grabs >= MAX_GRABS_PER_SYNC {
                report.deferred += 1;
                mark(
                    st,
                    &c.release,
                    "deferred",
                    format!(
                        "more than {MAX_GRABS_PER_SYNC} grabs in one sync; continues next sync"
                    ),
                    None,
                    now,
                )
                .await?;
                continue;
            }
            let covers = link.covers.clone();
            let saved = vec![link.clone()];
            st.store
                .with(move |conn| downloads::save_links(conn, &saved, now))
                .await?;
            match downloads::enqueue(
                st,
                EnqueueRequest {
                    link,
                    plan_id: None,
                    origin: "automatic",
                },
            )
            .await
            {
                Ok(g) => {
                    grabs += 1;
                    report.grabbed += 1;
                    taken.extend(covers);
                    mark(
                        st,
                        &c.release,
                        "grabbed",
                        format!("grabbed as {}", g.id),
                        Some(g.id),
                        now,
                    )
                    .await?;
                }
                Err(EnqueueError::Refused(code, msg)) => {
                    let status = if matches!(code, "covers_queued" | "duplicate_torrent") {
                        "deferred"
                    } else {
                        "rejected"
                    };
                    if status == "deferred" {
                        report.deferred += 1
                    } else {
                        report.rejected += 1
                    }
                    mark(st, &c.release, status, format!("{code}: {msg}"), None, now).await?;
                }
                Err(EnqueueError::Upstream(e)) => {
                    report.deferred += 1;
                    mark(
                        st,
                        &c.release,
                        "deferred",
                        format!(
                            "payload fetch failed: {} {}",
                            st.provider_label(c.release.indexer_id),
                            e.describe()
                        ),
                        None,
                        now,
                    )
                    .await?;
                }
                Err(EnqueueError::Internal(e)) => return Err(e),
            }
        }
    }
    Ok(())
}

/// Per-indexer RSS state for task status and diagnostics.
pub fn state(c: &rusqlite::Connection) -> Result<Vec<serde_json::Value>> {
    let mut stmt = c.prepare("SELECT indexer_id, last_attempt_at, last_success_at, newest_pub, last_status, last_error, items, gap_possible, failures, next_attempt_at FROM rss_state ORDER BY indexer_id")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({
                "indexer_id": r.get::<_, i64>(0)?,
                "last_attempt_at": r.get::<_, Option<String>>(1)?,
                "last_success_at": r.get::<_, Option<String>>(2)?,
                "newest_published_at": r.get::<_, Option<String>>(3)?,
                "last_status": r.get::<_, Option<String>>(4)?,
                "last_error": r.get::<_, Option<String>>(5)?,
                "items": r.get::<_, Option<i64>>(6)?,
                "gap_possible": r.get::<_, bool>(7)?,
                "failures": r.get::<_, i64>(8)?,
                "next_attempt_at": r.get::<_, Option<String>>(9)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom" xmlns:torznab="http://torznab.com/schemas/2015/feed">
  <channel>
    <atom:link href="http://prowlarr.test/1/api" rel="self" type="application/rss+xml" />
    <title>Feed</title>
    <item>
      <title>Slow Horses S01E02 1080p WEB-DL x264-GRP</title>
      <guid isPermaLink="false">fixture-1</guid>
      <prowlarrindexer id="1">Fixture</prowlarrindexer>
      <description><![CDATA[<img src="http://evil.example/x.png">]]></description>
      <link>http://prowlarr.test/1/download?apikey=FAKE&amp;link=abc</link>
      <comments>https://tracker.example/details/1</comments>
      <pubDate>Thu, 01 Oct 2026 00:00:00 +0000</pubDate>
      <size>2147483648</size>
      <category>5000</category>
      <category>5040</category>
      <enclosure url="http://prowlarr.test/1/download?apikey=FAKE&amp;link=abc" length="2147483648" type="application/x-bittorrent" />
      <torznab:attr name="category" value="5040" />
      <torznab:attr name="seeders" value="12" />
      <torznab:attr name="peers" value="15" />
      <torznab:attr name="infohash" value="C12FE1C06BBA254A9DC9F519B335AA7C1367A88A" />
      <torznab:attr name="downloadvolumefactor" value="1" />
      <attr name="seeders" value="9999" />
    </item>
    <item>
      <title>Elsewhere 2026 1080p WEB-DL</title>
      <link>https://evil.example/download/2</link>
      <torznab:attr name="magneturl" value="magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&amp;dn=x" />
    </item>
    <item><description>no title</description></item>
  </channel>
</rss>"#;

    #[test]
    fn parses_namespaced_torznab() {
        let origin = reqwest::Url::parse("http://prowlarr.test/").unwrap();
        let f = parse(FEED.as_bytes(), &origin, 1).unwrap();
        assert_eq!(f.items.len(), 2);
        let a = &f.items[0];
        assert_eq!(
            a.title.as_deref(),
            Some("Slow Horses S01E02 1080p WEB-DL x264-GRP")
        );
        assert_eq!(a.seeders, Some(12), "un-namespaced attr ignored");
        assert_eq!(a.leechers, Some(3));
        assert_eq!(a.size, Some(2_147_483_648));
        assert_eq!(
            a.info_hash.as_deref(),
            Some("c12fe1c06bba254a9dc9f519b335aa7c1367a88a")
        );
        assert_eq!(a.publish_date.as_deref(), Some("2026-10-01T00:00:00Z"));
        assert_eq!(
            a.download_url.as_ref().unwrap().expose(),
            "http://prowlarr.test/1/download?apikey=FAKE&link=abc",
            "XML-escaped URL decoded"
        );
        assert_eq!(a.indexer.as_deref(), Some("Fixture"));
        let cats: Vec<i64> = a.categories.iter().flatten().filter_map(|c| c.id).collect();
        assert_eq!(cats, vec![5040, 5000]);
        let b = &f.items[1];
        assert!(b.download_url.is_none(), "off-origin link dropped");
        assert!(b.magnet_url.is_some());
        assert_eq!(f.links_dropped, 1);
        assert!(
            !format!("{a:?}").contains("apikey"),
            "links are redacted in Debug"
        );
    }

    #[test]
    fn a_gap_is_judged_by_overlap_with_the_last_sync_never_by_the_window_size() {
        let last = Some("2026-10-27T08:00:00Z");
        // First sync: no baseline, nothing could have been missed.
        assert!(!gap_possible(None, Some("2026-10-27T09:00:00Z")));
        assert!(!gap_possible(None, None));
        // The window reaches back to or past the last sync's newest release: contiguous.
        assert!(!gap_possible(last, Some("2026-10-27T07:00:00Z")));
        assert!(!gap_possible(last, Some("2026-10-27T08:00:00Z")));
        // Entirely newer than the last sync, however few rows it has: a gap is possible.
        assert!(gap_possible(last, Some("2026-10-27T08:00:01Z")));
        // Nothing dated to compare with a baseline: overlap is not shown.
        assert!(gap_possible(last, None));
    }

    #[test]
    fn hostile_documents_are_refused() {
        let origin = reqwest::Url::parse("http://prowlarr.test/").unwrap();
        let bomb = r#"<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;">]><rss><channel><item><title>&lol2;</title></item></channel></rss>"#;
        assert!(
            parse(bomb.as_bytes(), &origin, 1)
                .unwrap_err()
                .contains("DTD")
        );
        let xxe = r#"<?xml version="1.0"?><!DOCTYPE r [<!ENTITY x SYSTEM "file:///etc/passwd">]><rss><channel><item><title>&x;</title></item></channel></rss>"#;
        assert!(parse(xxe.as_bytes(), &origin, 1).is_err());
        assert!(parse(b"<rss><channel><item>", &origin, 1).is_err());
        assert!(parse(b"\xff\xfe", &origin, 1).is_err());
        let err = r#"<?xml version="1.0"?><error code="100" description="Incorrect user credentials apikey=SECRET"/>"#;
        let e = parse(err.as_bytes(), &origin, 1).unwrap_err();
        assert!(e.contains("100") && !e.contains("SECRET"));
        let many = format!(
            "<rss><channel>{}</channel></rss>",
            "<item><title>x 2026</title></item>".repeat(MAX_ITEMS + 5)
        );
        let f = parse(many.as_bytes(), &origin, 1).unwrap();
        assert_eq!(f.items.len(), MAX_ITEMS);
        assert!(f.truncated);
        let deep = format!("<rss>{}</rss>", "<a>".repeat(300_000));
        assert!(parse(deep.as_bytes(), &origin, 1).is_err());
    }
}
