//! Knaben's JSON search API (https://knaben.org/api/v1), natively. Knaben aggregates public
//! trackers (The Pirate Bay and others) and answers one query from all of them:
//!
//! - search: `POST {base}v1` with a JSON body: `query` (matched against `title`, every word
//!   must match: `search_type` "100%"), `categories`, `order_by` seeders (descending), `size`,
//!   `hide_unsafe` and `hide_xxx`;
//! - latest releases (no query): the same request without `query`, ordered by `date`;
//! - results: `{"hits": [...], "total": {"relation", "value"}}`. A hit has `title`, `hash`
//!   (hex, upper case, may be null), `bytes`, `date`, `seeders`, `peers`, `liveSeeders`,
//!   `livePeers`, `lastSeen`, `category`, `categoryId` and more that is not used here
//!   (`magnetUrl` carries a long tracker list and is ignored: the magnet is rebuilt from the
//!   info-hash exactly as for The Pirate Bay, with only the operator's `magnet_trackers`).
//!
//! What the counts mean (measured against the live API; the API page does not define them):
//! `seeders` and `peers` are the stored counts from the source tracker, and `peers` is the
//! number of **leechers** (it is below `seeders` for well-seeded torrents: 48 seeders, 3 peers),
//! not seeders plus leechers. `liveSeeders` and `livePeers` are the same two numbers read
//! from the trackers when Knaben last saw the torrent (`lastSeen`). They are used when
//! `liveSeeders` is present and `lastSeen` is at most three days old; otherwise the stored
//! counts are.
//!
//! Categories are Knaben's ids, `N * 1_000_000` for a top level and `+ k * 1000` for a
//! sub-category (TV 2000000, Movies 3000000), measured against the live API (see
//! `CATEGORIES`). A torrent can carry a top-level id of a kind it is not in (a mislabelled
//! "Movies / HD" season pack carries TV 2000000), so a hit is kept when **any** of its ids is
//! among the requested ones. The release then reports the Torznab categories of those ids
//! only. A request that maps to no Knaben category is refused rather than searching all.
//! One request returns one page of at most 100 hits (a full page is reported as truncated).

use crate::import::rfc3339;
use crate::indexer::{FeedWindow, Indexer, Release};
use crate::prowlarr::{Category, PrivateLink, SearchOutcome, SearchRequest};
use crate::tpb::{Parsed, magnet, normalize_title};
use crate::upstream::{self, UpstreamError};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Hits one search asks for (no paging).
pub const PAGE_LIMIT: usize = 100;
/// Hits one latest-releases read asks for (the API's default is 150, its maximum 300).
const FEED_LIMIT: usize = 150;
/// Rows read from one answer at most.
const MAX_ROWS: usize = 1000;
/// Live tracker counts older than this are not trusted over the stored ones.
const LIVE_MAX_AGE_SECS: i64 = 3 * 86_400;

/// Knaben video categories: (Knaben id, Torznab id, whether a request for the Torznab parent
/// (2000/5000) includes it). The top-level ids 2000000/3000000 are the broadest and also
/// catch torrents that only carry the parent. Anime (6000000) and the rest are never searched.
pub const CATEGORIES: &[(u32, u32, bool)] = &[
    (3000000, 2000, true), // Movies
    (3001000, 2040, true), // Movies / HD
    (3002000, 2030, true), // Movies / SD
    (3003000, 2045, true), // Movies / UHD
    (3004000, 2070, true), // Movies / DVD
    (3005000, 2010, true), // Movies / Foreign
    (3006000, 2000, true), // Movies / Bollywood
    (3007000, 2060, true), // Movies / 3D
    (2000000, 5000, true), // TV
    (2001000, 5040, true), // TV / HD
    (2002000, 5030, true), // TV / SD
    (2003000, 5045, true), // TV / UHD
    (2004000, 5080, true), // TV / Documentary
    (2005000, 5020, true), // TV / Foreign
    (2006000, 5060, true), // TV / Sport
    (2007000, 5000, true), // TV / Cartoon
    (2008000, 5000, true), // TV / Other
];

/// Knaben categories for requested Torznab categories (sorted, deduplicated).
pub fn knaben_categories(torznab: &[u32]) -> Vec<u32> {
    let mut out: BTreeSet<u32> = BTreeSet::new();
    for &c in torznab {
        for &(kn, tz, in_parent) in CATEGORIES {
            if tz == c || (in_parent && c % 1000 == 0 && tz / 1000 == c / 1000) {
                out.insert(kn);
            }
        }
    }
    out.into_iter().collect()
}

/// Torznab categories (specific, then parent) of the Knaben `ids` that are among `allowed`.
fn torznab_of(ids: &[u32], allowed: &[u32]) -> Vec<Category> {
    let mut out: Vec<i64> = vec![];
    for id in ids.iter().filter(|i| allowed.contains(i)) {
        if let Some(&(_, tz, _)) = CATEGORIES.iter().find(|(k, _, _)| k == id) {
            for c in [i64::from(tz), i64::from(tz / 1000 * 1000)] {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
    }
    out.into_iter().map(|c| Category { id: Some(c) }).collect()
}

fn num(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn text(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A Knaben timestamp: RFC 3339, or the same without an offset (read as UTC).
fn timestamp(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339)
        .or_else(|_| OffsetDateTime::parse(&format!("{s}Z"), &Rfc3339))
        .ok()
}

/// The torrent's seeders and leechers: the live tracker counts when present and recent, else
/// the stored ones. `peers` is the leecher count (see the module docs).
fn counts(row: &Value, now: OffsetDateTime) -> (Option<i64>, Option<i64>) {
    let nonneg = |v: Option<i64>| v.filter(|n| *n >= 0);
    let fresh = text(row.get("lastSeen"))
        .and_then(timestamp)
        .is_some_and(|t| (now - t).whole_seconds() <= LIVE_MAX_AGE_SECS);
    let live_seeders = nonneg(num(row.get("liveSeeders")));
    if fresh && live_seeders.is_some() {
        let leechers = nonneg(num(row.get("livePeers"))).or(nonneg(num(row.get("peers"))));
        return (live_seeders, leechers);
    }
    (
        nonneg(num(row.get("seeders"))),
        nonneg(num(row.get("peers"))),
    )
}

/// Parses an untrusted answer; keeps hits that carry one of the `allowed` Knaben category
/// ids. A torrent listed by several trackers comes once, with the larger seeder count.
pub fn parse(
    body: &[u8],
    indexer_id: u32,
    indexer_name: &str,
    allowed: &[u32],
    trackers: &[String],
    now: OffsetDateTime,
) -> Result<Parsed, UpstreamError> {
    let bad = |reason: &str| UpstreamError::BadResponse {
        reason: reason.into(),
    };
    let doc: Value = serde_json::from_slice(body).map_err(|e| UpstreamError::BadResponse {
        reason: format!(
            "invalid JSON ({:?} error at line {} column {})",
            e.classify(),
            e.line(),
            e.column()
        ),
    })?;
    let hits = doc
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("the answer has no hits list"))?;
    let mut out = Parsed {
        // An explicit zero total is Knaben's "no results" (an absent total is not).
        confirmed_empty: hits.is_empty()
            && doc.pointer("/total/relation").and_then(Value::as_str) == Some("eq")
            && num(doc.pointer("/total/value")) == Some(0),
        truncated: hits.len() > MAX_ROWS,
        ..Parsed::default()
    };
    let mut by_hash: HashMap<String, usize> = HashMap::new();
    for row in hits.iter().take(MAX_ROWS) {
        out.rows += 1;
        let added = text(row.get("date"))
            .and_then(timestamp)
            .filter(|t| t.year() > 1990)
            .map(rfc3339);
        if let Some(a) = &added {
            if out.oldest.as_ref().is_none_or(|o| a < o) {
                out.oldest = Some(a.clone());
            }
            if out.newest.as_ref().is_none_or(|n| a > n) {
                out.newest = Some(a.clone());
            }
        }
        let name = text(row.get("title"))
            .filter(|n| n.chars().count() <= 1000 && !n.chars().any(char::is_control));
        let hash = text(row.get("hash"))
            .map(str::to_ascii_lowercase)
            .or_else(|| {
                text(row.get("magnetUrl")).and_then(|m| crate::torrent::magnet_hash(m).ok())
            })
            .filter(|h| crate::torrent::is_info_hash(h) && h.bytes().any(|b| b != b'0'));
        let ids: Vec<u32> = row
            .get("categoryId")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| num(Some(v)).and_then(|n| u32::try_from(n).ok()))
                    .collect()
            })
            .unwrap_or_default();
        let (Some(name), Some(hash)) = (name, hash) else {
            out.dropped += 1;
            continue;
        };
        let categories = torznab_of(&ids, allowed);
        if categories.is_empty() {
            out.dropped += 1;
            continue;
        }
        let (seeders, leechers) = counts(row, now);
        let release = Release {
            guid: Some(format!("knaben:{hash}")),
            title: Some(normalize_title(name)),
            size: num(row.get("bytes")).filter(|s| *s > 0),
            indexer_id: Some(i64::from(indexer_id)),
            indexer: Some(indexer_name.chars().take(200).collect()),
            seeders,
            leechers,
            protocol: Some("torrent".into()),
            publish_date: added,
            magnet_url: Some(PrivateLink::new(magnet(&hash, name, trackers))),
            info_hash: Some(hash.clone()),
            download_url: None,
            categories: Some(categories),
            minimum_ratio: None,
            minimum_seed_time: None,
            download_volume_factor: None,
            upload_volume_factor: None,
        };
        match by_hash.get(&hash) {
            Some(&i) => {
                if release.seeders > out.releases[i].seeders {
                    out.releases[i] = release;
                }
            }
            None => {
                by_hash.insert(hash, out.releases.len());
                out.releases.push(release);
            }
        }
    }
    Ok(out)
}

/// The request body of a search (`query` empty: the latest releases, newest first).
pub fn request_body(query: &str, categories: &[u32], size: usize) -> Value {
    let mut body = json!({
        "search_type": "100%",
        "categories": categories,
        "order_by": "seeders",
        "order_direction": "desc",
        "size": size,
        "hide_unsafe": true,
        "hide_xxx": true,
    });
    if query.is_empty() {
        body["order_by"] = json!("date");
    } else {
        body["search_field"] = json!("title");
        body["query"] = json!(query);
    }
    body
}

async fn post(ix: &Indexer, body: &Value) -> Result<Vec<u8>, UpstreamError> {
    let url = ix
        .cfg
        .base_url
        .join("v1")
        .map_err(|_| UpstreamError::BadResponse {
            reason: "invalid base URL".into(),
        })?;
    let resp = ix
        .send(
            ix.http()
                .post(url)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .body(body.to_string()),
            true,
        )
        .await?;
    upstream::read_bytes(resp, ix.cfg.max_response_bytes).await
}

fn no_categories(requested: &[u32]) -> UpstreamError {
    UpstreamError::BadResponse {
        reason: format!(
            "no Knaben category maps to the requested categories {requested:?}; refusing to search every category"
        ),
    }
}

pub async fn search(ix: &Indexer, req: &SearchRequest) -> Result<SearchOutcome, UpstreamError> {
    let cats = knaben_categories(&req.categories);
    if cats.is_empty() {
        return Err(no_categories(&req.categories));
    }
    let q = req.query.trim();
    let body = post(ix, &request_body(q, &cats, PAGE_LIMIT)).await?;
    let p = parse(
        &body,
        ix.cfg.id,
        &ix.cfg.name,
        &cats,
        &ix.cfg.magnet_trackers,
        OffsetDateTime::now_utc(),
    )?;
    Ok(SearchOutcome {
        pages_fetched: 1,
        raw_count: p.rows,
        partial_error: None,
        truncated_by_page_limit: p.rows >= PAGE_LIMIT || p.truncated,
        fetched_at: OffsetDateTime::now_utc(),
        from_cache: false,
        dropped: p.dropped,
        confirmed_empty: p.confirmed_empty,
        releases: Arc::new(p.releases),
    })
}

pub async fn feed(ix: &Indexer, categories: &[u32]) -> Result<FeedWindow, UpstreamError> {
    if let Some(why) = ix.rss_unsupported() {
        return Err(UpstreamError::BadResponse { reason: why });
    }
    let cats = knaben_categories(categories);
    if cats.is_empty() {
        return Err(no_categories(categories));
    }
    let body = post(ix, &request_body("", &cats, FEED_LIMIT)).await?;
    let p = parse(
        &body,
        ix.cfg.id,
        &ix.cfg.name,
        &cats,
        &ix.cfg.magnet_trackers,
        OffsetDateTime::now_utc(),
    )?;
    Ok(FeedWindow {
        items: p.releases,
        window: p.rows,
        oldest: p.oldest,
        newest: p.newest,
        links_dropped: 0,
        dropped: p.dropped,
        truncated: p.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// Recorded from the live API (`futurama s05`, TV categories, first four hits; magnets
    /// trimmed to three trackers).
    const FUTURAMA: &str = include_str!("../tests/fixtures/knaben/futurama-s05.json");
    const IVY_HASH: &str = "5772ec994f69feb83f7100f592ccfdfbd49305c2";
    const NOW: OffsetDateTime = datetime!(2026-11-10 12:00 UTC);

    fn parse_tv(body: &[u8]) -> Parsed {
        parse(body, 2, "Knaben", &knaben_categories(&[5000]), &[], NOW).unwrap()
    }

    #[test]
    fn categories_map_to_video_only() {
        assert_eq!(
            knaben_categories(&[5000]),
            [
                2000000, 2001000, 2002000, 2003000, 2004000, 2005000, 2006000, 2007000, 2008000
            ]
        );
        assert_eq!(
            knaben_categories(&[2000]),
            [
                3000000, 3001000, 3002000, 3003000, 3004000, 3005000, 3006000, 3007000
            ]
        );
        assert_eq!(knaben_categories(&[5040]), [2001000]);
        assert_eq!(knaben_categories(&[2040, 2045]), [3001000, 3003000]);
        assert!(knaben_categories(&[3000, 4000, 7000, 8000]).is_empty());
        assert!(knaben_categories(&[]).is_empty());
    }

    #[test]
    fn request_bodies() {
        let b = request_body("futurama s05", &[2000000, 2001000], 100);
        assert_eq!(b["query"], "futurama s05");
        assert_eq!(b["search_field"], "title");
        assert_eq!(b["search_type"], "100%");
        assert_eq!(b["order_by"], "seeders");
        assert_eq!(b["categories"], json!([2000000, 2001000]));
        assert_eq!(
            (b["hide_unsafe"].clone(), b["hide_xxx"].clone()),
            (json!(true), json!(true))
        );
        let f = request_body("", &[2000000], 150);
        assert!(f.get("query").is_none() && f.get("search_field").is_none());
        assert_eq!(f["order_by"], "date");
    }

    #[test]
    fn parses_the_recorded_answer() {
        let p = parse_tv(FUTURAMA.as_bytes());
        assert_eq!((p.rows, p.releases.len(), p.dropped), (4, 4, 0));
        assert!(!p.confirmed_empty);
        // The pack TPB's own category filter misses: a "Movies / HD" row that also carries
        // the TV id. It is kept, as a TV release, with live counts (lastSeen is 2 days old).
        let r = &p.releases[0];
        assert_eq!(
            r.title.as_deref(),
            Some("Futurama-1999-S05 1080p WEBRip 10bit EAC3 2 0 x265-iVy")
        );
        assert_eq!(r.info_hash.as_deref(), Some(IVY_HASH));
        assert_eq!((r.seeders, r.leechers), (Some(39), Some(3)));
        assert_eq!(r.size, Some(4_133_906_022));
        assert_eq!(r.guid.as_deref(), Some(&*format!("knaben:{IVY_HASH}")));
        let cats: Vec<i64> = r.categories.iter().flatten().filter_map(|c| c.id).collect();
        assert_eq!(cats, [5000], "only the requested kind is reported");
        assert!(r.publish_date.as_deref().is_some_and(|d| d.ends_with('Z')));
        assert!(r.download_url.is_none());
        // The magnet is rebuilt from the hash: Knaben's tracker list is not copied.
        let m = r.magnet_url.as_ref().unwrap().expose();
        assert!(
            m.starts_with(&format!("magnet:?xt=urn:btih:{IVY_HASH}&dn=")),
            "{m}"
        );
        assert!(!m.contains("tr="), "no implicit trackers: {m}");
        assert!(!format!("{r:?}").contains("magnet:"), "links stay redacted");
        // lastSeen is 12 days old on the third row: the stored counts apply (10 seeders, 2 peers).
        let old = &p.releases[2];
        assert_eq!((old.seeders, old.leechers), (Some(10), Some(2)));
        assert_eq!(p.newest.as_deref().map(|d| d.len()), Some(20));
    }

    #[test]
    fn live_counts_need_a_recent_last_seen() {
        let row = |last_seen: Value| json!({ "seeders": 48, "peers": 3, "liveSeeders": 39, "livePeers": 5, "lastSeen": last_seen });
        assert_eq!(
            counts(&row(json!("2026-11-09T12:00:00+00:00")), NOW),
            (Some(39), Some(5))
        );
        assert_eq!(
            counts(&row(json!("2026-11-01T12:00:00+00:00")), NOW),
            (Some(48), Some(3))
        );
        assert_eq!(counts(&row(Value::Null), NOW), (Some(48), Some(3)));
        // No live seeders: stored counts. Live seeders without live peers: stored peers.
        let stored = json!({ "seeders": 7, "peers": 2, "lastSeen": "2026-11-10T11:00:00+00:00" });
        assert_eq!(counts(&stored, NOW), (Some(7), Some(2)));
        let half = json!({ "seeders": 7, "peers": 2, "liveSeeders": 0, "lastSeen": "2026-11-10T11:00:00" });
        assert_eq!(
            counts(&half, NOW),
            (Some(0), Some(2)),
            "offset-less timestamps are UTC"
        );
        // Negative or text counts are dropped, never trusted.
        let junk = json!({ "seeders": -5, "peers": "x" });
        assert_eq!(counts(&junk, NOW), (None, None));
    }

    #[test]
    fn junk_missing_fields_duplicates_and_empty_answers() {
        let hash_a = "A".repeat(40);
        let hash_b = "b".repeat(40);
        let body = json!({ "hits": [
            { "title": "Show S01 Season 1 Complete 720p", "hash": hash_a, "bytes": 1000, "seeders": 3, "peers": 1,
              "categoryId": [2001000], "date": "2026-01-02T03:04:05+00:00" },
            // The same torrent from another tracker with more seeders: one release, the better counts.
            { "title": "Show S01 720p", "hash": hash_a, "bytes": 1000, "seeders": 9, "peers": 2, "categoryId": [2000000] },
            // No hash: recovered from the magnet (base32 or hex), otherwise dropped.
            { "title": "From magnet S01", "hash": null, "magnetUrl": format!("magnet:?xt=urn:btih:{hash_b}&dn=x"),
              "seeders": 1, "categoryId": [2000000] },
            { "title": "No hash S01", "hash": null, "magnetUrl": null, "categoryId": [2000000] },
            { "title": "Zero hash S01", "hash": "0".repeat(40), "categoryId": [2000000] },
            { "title": "Short hash S01", "hash": "abc", "categoryId": [2000000] },
            { "title": "Control\u{0007}char S01", "hash": "c".repeat(40), "categoryId": [2000000] },
            { "title": "", "hash": "d".repeat(40), "categoryId": [2000000] },
            { "hash": "e".repeat(40), "categoryId": [2000000] },
            // Wrong kind, no category, text category ids and negative sizes.
            { "title": "A movie 2020", "hash": "f".repeat(40), "categoryId": [3001000] },
            { "title": "Uncategorised S01", "hash": "1".repeat(40) },
            { "title": "Text ids S01", "hash": "2".repeat(40), "categoryId": ["x", null, -1] },
            { "title": "Odd numbers S01", "hash": "3".repeat(40), "categoryId": [2000000], "bytes": -4, "seeders": "12", "peers": null, "date": "garbage" },
        ] });
        let bytes = serde_json::to_vec(&body).unwrap();
        let p = parse_tv(&bytes);
        assert_eq!(p.rows, 13);
        let titles: Vec<_> = p
            .releases
            .iter()
            .map(|r| r.title.clone().unwrap())
            .collect();
        assert_eq!(
            titles,
            ["Show S01 720p", "From magnet S01", "Odd numbers S01"],
            "{titles:?}"
        );
        assert_eq!(p.dropped, 9, "13 rows: 3 kept, 1 merged duplicate");
        assert_eq!(
            p.releases[0].seeders,
            Some(9),
            "the better-seeded copy wins"
        );
        assert_eq!(
            p.releases[0].info_hash.as_deref(),
            Some(&*hash_a.to_ascii_lowercase())
        );
        // The Season N Complete form is normalised only when it is the title in the answer.
        let odd = &p.releases[2];
        assert_eq!(
            (
                odd.size,
                odd.seeders,
                odd.leechers,
                odd.publish_date.clone()
            ),
            (None, Some(12), None, None)
        );
        let norm = parse_tv(
            &serde_json::to_vec(&json!({ "hits": [
                { "title": "Show Season 1 Complete 720p", "hash": "4".repeat(40), "categoryId": [2000000] }
            ] }))
            .unwrap(),
        );
        assert_eq!(norm.releases[0].title.as_deref(), Some("Show S01 720p"));

        let empty =
            json!({ "hits": [], "max_score": null, "total": { "relation": "eq", "value": 0 } });
        let e = parse_tv(&serde_json::to_vec(&empty).unwrap());
        assert!(e.confirmed_empty && e.releases.is_empty() && e.rows == 0);
        let unconfirmed = parse_tv(br#"{"hits":[]}"#);
        assert!(
            !unconfirmed.confirmed_empty,
            "an answer without a total is not a confirmed no"
        );
        let gte = parse_tv(br#"{"hits":[],"total":{"relation":"gte","value":0}}"#);
        assert!(!gte.confirmed_empty);
    }

    #[test]
    fn hostile_answers_are_errors_without_echoing_the_body() {
        for body in [
            &b"[]"[..],
            b"{\"hits\":3}",
            b"{\"error\":\"x\"}",
            b"not json",
            b"",
        ] {
            assert!(parse(body, 2, "Knaben", &[2000000], &[], NOW).is_err());
        }
        let err = parse(
            b"<html>apikey=SECRET</html>",
            2,
            "Knaben",
            &[2000000],
            &[],
            NOW,
        )
        .unwrap_err();
        assert!(!err.describe().contains("SECRET"));
    }
}
