//! The Pirate Bay's JSON API, natively (the contract of Prowlarr's Cardigann
//! `thepiratebay` definition, v11):
//!
//! - search: `GET {base}q.php?q=<keywords>&cat=<TPB categories>`, built as a raw query with
//!   spaces as `%20` and literal commas: the API answers a comma-separated category list
//!   with `+`-encoded spaces (form encoding) with its "no results" row, a false empty;
//! - latest releases (no query): `GET {base}precompiled/data_top100_recent.json`;
//! - results: `id`, `name`, `info_hash`, `category`, `added` (epoch seconds), `size`,
//!   `seeders`, `leechers`, `username`, `imdb`, as strings or numbers;
//! - no results: a single row with `id` 0 (the site's sentinel).
//!
//! Requested Torznab categories map to TPB's video categories only (`CATEGORIES`); a request
//! that maps to none is refused rather than searching every category, and results outside the
//! requested TPB categories are dropped (counted). A search returns one page of at most 100
//! rows: a full page is reported as truncated. The site's pack naming ("Season 1 Complete")
//! is normalised to `S01`. Magnets are built from the info-hash with the release name and
//! only the operator's `magnet_trackers` (none by default). The optional `uploaders`
//! allowlist is the only site filter; there is no implicit one.

use crate::import::rfc3339;
use crate::indexer::{FeedWindow, Indexer, Release};
use crate::prowlarr::{Category, PrivateLink, SearchOutcome, SearchRequest};
use crate::upstream::{self, UpstreamError};
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};
use time::OffsetDateTime;

/// Rows one search answers at most (no paging).
pub const PAGE_LIMIT: usize = 100;
/// Rows read from one answer at most.
const MAX_ROWS: usize = 1000;

/// TPB video categories: (TPB id, Torznab id, whether a request for the Torznab parent
/// (2000/5000) includes it). Clips, "video other" and handheld TV are only searched when
/// their own Torznab category (2020/5050) is requested; "Video" (200) and music videos
/// (203) never are, because they span unrelated content.
pub const CATEGORIES: &[(u32, u32, bool)] = &[
    (201, 2000, true),  // Movies
    (202, 2070, true),  // Movies DVDR
    (204, 2020, false), // Movie clips
    (207, 2040, true),  // HD movies
    (209, 2060, true),  // 3D
    (210, 2030, true),  // CAM/TS
    (211, 2045, true),  // UHD/4k movies
    (299, 2020, false), // Video other
    (205, 5000, true),  // TV shows
    (206, 5050, false), // Handheld
    (208, 5040, true),  // HD TV shows
    (212, 5045, true),  // UHD/4k TV shows
];

/// TPB categories for requested Torznab categories (sorted, deduplicated).
pub fn tpb_categories(torznab: &[u32]) -> Vec<u32> {
    let mut out: BTreeSet<u32> = BTreeSet::new();
    for &c in torznab {
        for &(tpb, tz, in_parent) in CATEGORIES {
            if tz == c || (in_parent && c % 1000 == 0 && tz / 1000 == c / 1000) {
                out.insert(tpb);
            }
        }
    }
    out.into_iter().collect()
}

/// Torznab categories a TPB category reports (specific, then parent).
fn torznab_of(tpb: u32) -> Vec<Category> {
    CATEGORIES
        .iter()
        .find(|(t, _, _)| *t == tpb)
        .map(|&(_, tz, _)| {
            let parent = tz / 1000 * 1000;
            let mut v = vec![Category {
                id: Some(i64::from(tz)),
            }];
            if parent != tz {
                v.push(Category {
                    id: Some(i64::from(parent)),
                });
            }
            v
        })
        .unwrap_or_default()
}

static SEASON_COMPLETE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bseason[\s._-]*0*(\d{1,2})[\s._-]*complete\b").expect("static regex")
});
static COMPLETE_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bcomplete[\s._-]+season[\s._-]*0*(\d{1,2})\b").expect("static regex")
});

/// "Show Season 1 Complete 720p" and "Show Complete Season 1" -> "Show S01 720p".
pub fn normalize_title(name: &str) -> String {
    let pad = |c: &regex::Captures| format!("S{:02}", c[1].parse::<u16>().unwrap_or(0));
    let s = SEASON_COMPLETE.replace_all(name, pad);
    let s = COMPLETE_SEASON.replace_all(&s, pad);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A magnet for an info-hash: the release name and only the configured trackers.
pub fn magnet(hash: &str, name: &str, trackers: &[String]) -> String {
    let mut m = reqwest::Url::parse("magnet:?").expect("static magnet prefix");
    {
        let mut q = m.query_pairs_mut();
        q.append_pair("xt", &format!("urn:btih:{hash}"));
        q.append_pair("dn", name);
        for t in trackers {
            q.append_pair("tr", t);
        }
    }
    // `Url` would percent-encode `:` in `urn:btih:`; keep the conventional form.
    m.as_str().replacen("xt=urn%3Abtih%3A", "xt=urn:btih:", 1)
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

/// A parsed answer.
#[derive(Debug, Default)]
pub struct Parsed {
    pub releases: Vec<Release>,
    /// Rows in the answer (the sentinel row is not counted).
    pub rows: usize,
    pub dropped: usize,
    pub confirmed_empty: bool,
    pub oldest: Option<String>,
    pub newest: Option<String>,
    pub truncated: bool,
}

/// Parses an untrusted answer; keeps rows in `allowed` TPB categories (and from `uploaders`
/// when that allowlist is set).
pub fn parse(
    body: &[u8],
    indexer_id: u32,
    indexer_name: &str,
    allowed: &[u32],
    uploaders: &[String],
    trackers: &[String],
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
    let rows = doc
        .as_array()
        .ok_or_else(|| bad("the answer is not a JSON array"))?;
    let mut out = Parsed::default();
    if let [only] = rows.as_slice()
        && num(only.get("id")) == Some(0)
    {
        out.confirmed_empty = true;
        return Ok(out);
    }
    out.truncated = rows.len() > MAX_ROWS;
    for row in rows.iter().take(MAX_ROWS) {
        out.rows += 1;
        let added = num(row.get("added"))
            .filter(|t| *t > 0)
            .and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok())
            .map(rfc3339);
        if let Some(a) = &added {
            if out.oldest.as_ref().is_none_or(|o| a < o) {
                out.oldest = Some(a.clone());
            }
            if out.newest.as_ref().is_none_or(|n| a > n) {
                out.newest = Some(a.clone());
            }
        }
        let id = num(row.get("id")).filter(|i| *i > 0);
        let name = text(row.get("name"))
            .filter(|n| n.chars().count() <= 1000 && !n.chars().any(char::is_control));
        let hash = text(row.get("info_hash"))
            .map(str::to_ascii_lowercase)
            .filter(|h| crate::torrent::is_info_hash(h) && h.bytes().any(|b| b != b'0'));
        let category = num(row.get("category")).and_then(|c| u32::try_from(c).ok());
        let (Some(id), Some(name), Some(hash), Some(category)) = (id, name, hash, category) else {
            out.dropped += 1;
            continue;
        };
        if !allowed.contains(&category) {
            out.dropped += 1;
            continue;
        }
        if !uploaders.is_empty()
            && !text(row.get("username")).is_some_and(|u| uploaders.iter().any(|x| x == u))
        {
            out.dropped += 1;
            continue;
        }
        out.releases.push(Release {
            guid: Some(format!("tpb:{id}")),
            title: Some(normalize_title(name)),
            size: num(row.get("size")).filter(|s| *s > 0),
            indexer_id: Some(i64::from(indexer_id)),
            indexer: Some(indexer_name.chars().take(200).collect()),
            seeders: num(row.get("seeders")).filter(|s| *s >= 0),
            leechers: num(row.get("leechers")).filter(|s| *s >= 0),
            protocol: Some("torrent".into()),
            publish_date: added,
            magnet_url: Some(PrivateLink::new(magnet(&hash, name, trackers))),
            info_hash: Some(hash),
            download_url: None,
            categories: Some(torznab_of(category)),
            minimum_ratio: None,
            minimum_seed_time: None,
            download_volume_factor: None,
            upload_volume_factor: None,
        });
    }
    Ok(out)
}

/// RFC 3986 percent-encoding of a query value: unreserved characters stay, everything else
/// (spaces included, as `%20`, never `+`) is encoded byte by byte.
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The search URL: `{base}q.php?q=<%20-encoded keywords>&cat=<comma list>`.
pub fn search_url(
    base: &reqwest::Url,
    query: &str,
    categories: &[u32],
) -> Result<reqwest::Url, UpstreamError> {
    let mut url = base.join("q.php").map_err(|_| UpstreamError::BadResponse {
        reason: "invalid base URL".into(),
    })?;
    let cats = categories
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    url.set_query(Some(&format!("q={}&cat={cats}", encode_component(query))));
    Ok(url)
}

fn base_join(ix: &Indexer, path: &str) -> Result<reqwest::Url, UpstreamError> {
    ix.cfg
        .base_url
        .join(path)
        .map_err(|_| UpstreamError::BadResponse {
            reason: "invalid base URL".into(),
        })
}

async fn get(ix: &Indexer, url: reqwest::Url) -> Result<Vec<u8>, UpstreamError> {
    let resp = ix
        .send(
            ix.http().get(url).header("Accept", "application/json"),
            true,
        )
        .await?;
    upstream::read_bytes(resp, ix.cfg.max_response_bytes).await
}

fn no_categories(requested: &[u32]) -> UpstreamError {
    UpstreamError::BadResponse {
        reason: format!(
            "no TPB category maps to the requested categories {requested:?}; refusing to search every category"
        ),
    }
}

pub async fn search(ix: &Indexer, req: &SearchRequest) -> Result<SearchOutcome, UpstreamError> {
    let cats = tpb_categories(&req.categories);
    if cats.is_empty() {
        return Err(no_categories(&req.categories));
    }
    let q = req.query.trim();
    let url = if q.is_empty() {
        base_join(ix, "precompiled/data_top100_recent.json")?
    } else {
        search_url(&ix.cfg.base_url, q, &cats)?
    };
    let body = get(ix, url).await?;
    let p = parse(
        &body,
        ix.cfg.id,
        &ix.cfg.name,
        &cats,
        &ix.cfg.uploaders,
        &ix.cfg.magnet_trackers,
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
    let cats = tpb_categories(categories);
    if cats.is_empty() {
        return Err(no_categories(categories));
    }
    let body = get(ix, base_join(ix, "precompiled/data_top100_recent.json")?).await?;
    let p = parse(
        &body,
        ix.cfg.id,
        &ix.cfg.name,
        &cats,
        &ix.cfg.uploaders,
        &ix.cfg.magnet_trackers,
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
    use crate::release::{self, Numbering};
    use serde_json::json;

    #[test]
    fn categories_map_to_video_only() {
        assert_eq!(tpb_categories(&[2000]), [201, 202, 207, 209, 210, 211]);
        assert_eq!(tpb_categories(&[5000]), [205, 208, 212]);
        assert_eq!(tpb_categories(&[2040, 2045]), [207, 211]);
        assert_eq!(tpb_categories(&[5040]), [208]);
        assert_eq!(tpb_categories(&[2020]), [204, 299]);
        assert_eq!(tpb_categories(&[5050]), [206]);
        assert!(tpb_categories(&[2010, 3000, 8000]).is_empty());
        let both = tpb_categories(&[2000, 5000, 5040]);
        assert_eq!(both, [201, 202, 205, 207, 208, 209, 210, 211, 212]);
    }

    #[test]
    fn search_urls_encode_spaces_as_percent_20() {
        let base = reqwest::Url::parse("https://apibay.example/").unwrap();
        let u = search_url(&base, "Slow Horses S01", &tpb_categories(&[5000])).unwrap();
        assert_eq!(
            u.as_str(),
            "https://apibay.example/q.php?q=Slow%20Horses%20S01&cat=205,208,212"
        );
        assert!(!u.as_str().contains('+'), "never form encoding");
        let m = search_url(&base, "The Menu 2022", &tpb_categories(&[2000, 2040])).unwrap();
        assert_eq!(
            m.query(),
            Some("q=The%20Menu%202022&cat=201,202,207,209,210,211")
        );
        let odd = search_url(&base, "Amélie & co+1/2", &[201]).unwrap();
        assert_eq!(
            odd.query(),
            Some("q=Am%C3%A9lie%20%26%20co%2B1%2F2&cat=201")
        );
        let prefixed = reqwest::Url::parse("http://127.0.0.1:9/tpb/").unwrap();
        assert_eq!(
            search_url(&prefixed, "x y", &[201]).unwrap().as_str(),
            "http://127.0.0.1:9/tpb/q.php?q=x%20y&cat=201"
        );
    }

    #[test]
    fn titles_and_magnets() {
        assert_eq!(
            normalize_title("Slow Horses Season 1 Complete 720p WEBRip x264"),
            "Slow Horses S01 720p WEBRip x264"
        );
        assert_eq!(
            normalize_title("Fargo.Season.04.Complete.1080p.WEB-DL"),
            "Fargo.S04.1080p.WEB-DL"
        );
        assert_eq!(
            normalize_title("Fargo Complete Season 5 1080p"),
            "Fargo S05 1080p"
        );
        assert_eq!(
            normalize_title("The Matrix 1999 1080p"),
            "The Matrix 1999 1080p"
        );
        // The policy parser reads the normalised and the original pack forms as packs.
        for t in [
            "Slow Horses S01 720p WEBRip x264",
            "Slow Horses Season 1 Complete 720p WEBRip x264",
            "Fargo.S04.1080p.WEB-DL",
        ] {
            let p = release::parse(&normalize_title(t));
            assert!(
                matches!(p.numbering, Numbering::Season { .. }),
                "{t}: {:?}",
                p.numbering
            );
        }
        let hash = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";
        let m = magnet(hash, "The Matrix 1999 1080p & more", &[]);
        assert!(
            m.starts_with(&format!("magnet:?xt=urn:btih:{hash}&dn=")),
            "{m}"
        );
        assert!(!m.contains("tr="), "no implicit trackers: {m}");
        assert_eq!(crate::torrent::magnet_hash(&m).unwrap(), hash);
        let with = magnet(hash, "x", &["udp://tracker.example:1337/announce".into()]);
        assert!(
            with.contains("&tr=udp%3A%2F%2Ftracker.example%3A1337%2Fannounce"),
            "{with}"
        );
    }

    #[test]
    fn parses_rows_sentinel_filters_and_hostile_input() {
        let hash = "C12FE1C06BBA254A9DC9F519B335AA7C1367A88A";
        let body = json!([
            { "id": "11", "name": "The Matrix 1999 1080p BluRay x264", "info_hash": hash, "leechers": "3", "seeders": "40",
              "num_files": "1", "size": "2147483648", "username": "uploader1", "added": "1700000000", "status": "vip", "category": "207", "imdb": "tt0133093" },
            { "id": 12, "name": "The Matrix 1999 soundtrack", "info_hash": "a".repeat(40), "seeders": 9, "leechers": 0,
              "size": 1000, "username": "uploader1", "added": 1700000100, "category": 101 },
            { "id": "13", "name": "No hash", "info_hash": "zz", "category": "207", "added": "1700000200" },
            { "id": "14", "name": "Other uploader 1999 1080p", "info_hash": "b".repeat(40), "category": "201", "username": "someone", "seeders": "1", "added": "1699999000" },
        ]);
        let bytes = serde_json::to_vec(&body).unwrap();
        let p = parse(
            &bytes,
            1,
            "The Pirate Bay",
            &tpb_categories(&[2000]),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(p.rows, 4);
        assert_eq!(p.releases.len(), 2, "music and an invalid hash are dropped");
        assert_eq!(p.dropped, 2);
        let r = &p.releases[0];
        assert_eq!(r.info_hash.as_deref(), Some(&*hash.to_ascii_lowercase()));
        assert_eq!(r.seeders, Some(40));
        assert_eq!(r.size, Some(2_147_483_648));
        assert_eq!(r.publish_date.as_deref(), Some("2023-11-14T22:13:20Z"));
        assert_eq!(r.guid.as_deref(), Some("tpb:11"));
        let cats: Vec<i64> = r.categories.iter().flatten().filter_map(|c| c.id).collect();
        assert_eq!(cats, [2040, 2000]);
        assert!(r.download_url.is_none());
        assert_eq!(
            crate::torrent::magnet_hash(r.magnet_url.as_ref().unwrap().expose()).unwrap(),
            hash.to_ascii_lowercase()
        );
        assert!(!format!("{r:?}").contains("magnet:"), "links stay redacted");
        assert_eq!(p.oldest.as_deref(), Some("2023-11-14T21:56:40Z"));
        assert_eq!(p.newest.as_deref(), Some("2023-11-14T22:16:40Z"));
        let only = parse(
            &bytes,
            1,
            "TPB",
            &tpb_categories(&[2000]),
            &["uploader1".into()],
            &[],
        )
        .unwrap();
        assert_eq!(only.releases.len(), 1, "the uploader allowlist filters");
        let sentinel = json!([{ "id": "0", "name": "No results returned", "info_hash": "0000000000000000000000000000000000000000",
            "leechers": "0", "seeders": "0", "num_files": "0", "size": "0", "username": "", "added": "0", "status": "member", "category": "0", "imdb": "" }]);
        let e = parse(
            &serde_json::to_vec(&sentinel).unwrap(),
            1,
            "TPB",
            &[201],
            &[],
            &[],
        )
        .unwrap();
        assert!(e.confirmed_empty && e.releases.is_empty() && e.rows == 0);
        let unconfirmed = parse(b"[]", 1, "TPB", &[201], &[], &[]).unwrap();
        assert!(
            !unconfirmed.confirmed_empty,
            "an empty array is not the site's sentinel"
        );
        assert!(parse(b"{\"error\":\"x\"}", 1, "TPB", &[201], &[], &[]).is_err());
        let err = parse(b"<html>apikey=SECRET</html>", 1, "TPB", &[201], &[], &[]).unwrap_err();
        assert!(!err.describe().contains("SECRET"));
        let control = json!([{ "id": "5", "name": "bad\u{0007}name", "info_hash": "c".repeat(40), "category": "201" }]);
        assert_eq!(
            parse(
                &serde_json::to_vec(&control).unwrap(),
                1,
                "TPB",
                &[201],
                &[],
                &[]
            )
            .unwrap()
            .releases
            .len(),
            0
        );
    }
}
