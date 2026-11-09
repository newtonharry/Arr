//! Shared test harness: a synthetic native snapshot (same shapes as the real export, fake
//! values only), a loopback mock for Prowlarr/TVmaze/TMDB, and an in-process API server.
#![allow(dead_code)]

use arr::{api, config, fsutil, import};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub mod qb;

pub const FAKE_APIKEY: &str = "FAKE-TEST-APIKEY-0001";
pub const FAKE_PASSWORD: &str = "FAKE-TEST-PASSWORD-0001";
pub const FAKE_USER: &str = "FAKE-TEST-USER-0001";
pub const FAKE_COOKIE: &str = "FAKE-TEST-COOKIE-0001";
pub const FAKE_PASSKEY: &str = "FAKE-TEST-PASSKEY-0001";
pub const PROWLARR_KEY: &str = "FAKE-TEST-PROWLARR-KEY-0001";
pub const TOKEN: &str = "test-token-0123456789abcdef0123456789";
pub const TMDB_V3_KEY: &str = "FAKE-TEST-TMDB-V3-KEY-0001";
pub const TMDB_V4_TOKEN: &str = "eyJGAKE-TEST-TMDB-V4-TOKEN-0001";

// ------------------------------------------------------------------ snapshot

fn quality(id: i64, name: &str) -> Value {
    json!({ "id": id, "name": name, "source": "unknown", "resolution": 0 })
}

fn item(id: i64, name: &str, allowed: bool) -> Value {
    json!({ "quality": quality(id, name), "items": [], "allowed": allowed })
}

fn group(id: i64, name: &str, members: Vec<Value>, allowed: bool) -> Value {
    json!({ "id": id, "name": name, "items": members, "allowed": allowed })
}

/// Radarr profile 6 "HD - 720p/1080p" in native order (ascending preference).
fn radarr_profile() -> Value {
    json!({
        "id": 6, "name": "HD - 720p/1080p", "upgradeAllowed": false, "cutoff": 6,
        "minFormatScore": 0, "cutoffFormatScore": 0, "minUpgradeFormatScore": 1, "formatItems": [],
        "language": { "id": 1, "name": "English" },
        "items": [
            item(0, "Unknown", false), item(24, "WORKPRINT", false), item(25, "CAM", false), item(26, "TELESYNC", false),
            item(27, "TELECINE", false), item(29, "REGIONAL", false), item(28, "DVDSCR", false), item(1, "SDTV", false),
            item(2, "DVD", false), item(23, "DVD-R", false),
            group(1000, "WEB 480p", vec![item(8, "WEBDL-480p", false), item(12, "WEBRip-480p", false)], false),
            item(20, "Bluray-480p", false), item(21, "Bluray-576p", false), item(4, "HDTV-720p", true),
            group(1001, "WEB 720p", vec![item(5, "WEBDL-720p", true), item(14, "WEBRip-720p", true)], true),
            item(6, "Bluray-720p", true), item(9, "HDTV-1080p", true),
            group(1002, "WEB 1080p", vec![item(3, "WEBDL-1080p", true), item(15, "WEBRip-1080p", true)], true),
            item(7, "Bluray-1080p", false), item(30, "Remux-1080p", false), item(16, "HDTV-2160p", false),
            group(1003, "WEB 2160p", vec![item(18, "WEBDL-2160p", false), item(17, "WEBRip-2160p", false)], false),
            item(19, "Bluray-2160p", false), item(31, "Remux-2160p", false), item(22, "BR-DISK", false), item(10, "Raw-HD", false),
        ],
    })
}

fn sonarr_profile() -> Value {
    json!({
        "id": 6, "name": "HD - 720p/1080p", "upgradeAllowed": false, "cutoff": 4,
        "minFormatScore": 0, "cutoffFormatScore": 0, "minUpgradeFormatScore": 1, "formatItems": [],
        "items": [
            item(0, "Unknown", false), item(1, "SDTV", false),
            group(1000, "WEB 480p", vec![item(12, "WEBRip-480p", false), item(8, "WEBDL-480p", false)], false),
            item(2, "DVD", false), item(13, "Bluray-480p", false), item(22, "Bluray-576p", false),
            item(4, "HDTV-720p", true), item(9, "HDTV-1080p", true), item(10, "Raw-HD", false),
            group(1001, "WEB 720p", vec![item(14, "WEBRip-720p", true), item(5, "WEBDL-720p", true)], true),
            item(6, "Bluray-720p", true),
            group(1002, "WEB 1080p", vec![item(15, "WEBRip-1080p", true), item(3, "WEBDL-1080p", true)], true),
            item(7, "Bluray-1080p", false), item(20, "Bluray-1080p Remux", false), item(16, "HDTV-2160p", false),
            group(1003, "WEB 2160p", vec![item(17, "WEBRip-2160p", false), item(18, "WEBDL-2160p", false)], false),
            item(19, "Bluray-2160p", false), item(21, "Bluray-2160p Remux", false),
        ],
    })
}

fn defs(rows: &[(i64, &str, f64, Option<f64>)]) -> Value {
    Value::Array(
        rows.iter()
            .enumerate()
            .map(|(i, (id, name, min, max))| json!({ "quality": quality(*id, name), "title": name, "weight": i + 1, "minSize": min, "maxSize": max, "preferredSize": 95, "id": i + 1 }))
            .collect(),
    )
}

fn radarr_defs() -> Value {
    let capped = [
        (0, "Unknown"),
        (24, "WORKPRINT"),
        (25, "CAM"),
        (26, "TELESYNC"),
        (27, "TELECINE"),
        (29, "REGIONAL"),
        (28, "DVDSCR"),
        (1, "SDTV"),
        (2, "DVD"),
        (23, "DVD-R"),
        (8, "WEBDL-480p"),
        (12, "WEBRip-480p"),
        (20, "Bluray-480p"),
        (21, "Bluray-576p"),
        (4, "HDTV-720p"),
        (5, "WEBDL-720p"),
        (14, "WEBRip-720p"),
        (6, "Bluray-720p"),
        (9, "HDTV-1080p"),
        (3, "WEBDL-1080p"),
        (15, "WEBRip-1080p"),
    ];
    let open = [
        (7, "Bluray-1080p"),
        (30, "Remux-1080p"),
        (16, "HDTV-2160p"),
        (18, "WEBDL-2160p"),
        (17, "WEBRip-2160p"),
        (19, "Bluray-2160p"),
        (31, "Remux-2160p"),
        (22, "BR-DISK"),
        (10, "Raw-HD"),
    ];
    let rows: Vec<(i64, &str, f64, Option<f64>)> = capped
        .iter()
        .map(|(i, n)| (*i, *n, 0.0, Some(100.0)))
        .chain(open.iter().map(|(i, n)| (*i, *n, 0.0, None)))
        .collect();
    defs(&rows)
}

fn sonarr_defs() -> Value {
    defs(&[
        (0, "Unknown", 1.0, Some(199.9)),
        (1, "SDTV", 2.0, Some(100.0)),
        (12, "WEBRip-480p", 2.0, Some(100.0)),
        (8, "WEBDL-480p", 2.0, Some(100.0)),
        (2, "DVD", 2.0, Some(100.0)),
        (13, "Bluray-480p", 2.0, Some(100.0)),
        (22, "Bluray-576p", 2.0, Some(100.0)),
        (4, "HDTV-720p", 3.0, Some(125.0)),
        (9, "HDTV-1080p", 4.0, Some(125.0)),
        (10, "Raw-HD", 4.0, None),
        (14, "WEBRip-720p", 3.0, Some(130.0)),
        (5, "WEBDL-720p", 3.0, Some(130.0)),
        (6, "Bluray-720p", 4.0, Some(130.0)),
        (15, "WEBRip-1080p", 4.0, Some(130.0)),
        (3, "WEBDL-1080p", 4.0, Some(130.0)),
        (7, "Bluray-1080p", 4.0, Some(155.0)),
        (20, "Bluray-1080p Remux", 35.0, None),
        (16, "HDTV-2160p", 35.0, Some(199.9)),
        (17, "WEBRip-2160p", 35.0, None),
        (18, "WEBDL-2160p", 35.0, None),
        (19, "Bluray-2160p", 35.0, None),
        (21, "Bluray-2160p Remux", 35.0, None),
    ])
}

fn field(name: &str, value: Value, privacy: &str) -> Value {
    json!({ "name": name, "value": value, "privacy": privacy, "type": if privacy == "password" { "password" } else { "textbox" } })
}

fn indexer(categories: Value) -> Value {
    json!({
        "id": 1, "name": "The Pirate Bay (Prowlarr)", "protocol": "torrent", "priority": 25,
        "enableRss": true, "enableAutomaticSearch": true, "enableInteractiveSearch": true,
        "implementation": "Torznab", "configContract": "TorznabSettings", "tags": [], "downloadClientId": 0,
        "fields": [
            field("baseUrl", json!("http://prowlarr:9696/1/"), "normal"),
            field("apiPath", json!("/api"), "normal"),
            field("apiKey", json!(FAKE_APIKEY), "apiKey"),
            field("categories", categories, "normal"),
            field("multiLanguages", json!([]), "normal"),
            field("minimumSeeders", json!(1), "normal"),
            field("seedCriteria.seedRatio", json!(2), "normal"),
            field("requiredFlags", json!([]), "normal"),
        ],
    })
}

fn download_client(category_field: &str, category: &str) -> Value {
    json!({
        "id": 1, "name": "qBittorrent", "enable": true, "protocol": "torrent", "priority": 1,
        "implementation": "QBittorrent", "tags": [], "removeCompletedDownloads": true, "removeFailedDownloads": true,
        "fields": [
            field("host", json!("qbittorrent"), "normal"), field("port", json!(8080), "normal"), field("useSsl", json!(false), "normal"),
            field("username", json!(FAKE_USER), "userName"), field("password", json!(FAKE_PASSWORD), "password"),
            field(category_field, json!(category), "normal"), field("initialState", json!(0), "normal"),
        ],
    })
}

fn delay() -> Value {
    json!([{ "id": 1, "enableUsenet": true, "enableTorrent": true, "preferredProtocol": "usenet", "usenetDelay": 0, "torrentDelay": 0,
             "bypassIfHighestQuality": true, "bypassIfAboveCustomFormatScore": false, "minimumCustomFormatScore": 0, "order": 2147483647, "tags": [] }])
}

fn english() -> Value {
    json!({ "id": 1, "name": "English" })
}

fn episode(season: i64, n: i64, has_file: bool, aired: &str) -> Value {
    json!({ "seasonNumber": season, "episodeNumber": n, "title": format!("Episode {n}"), "airDateUtc": aired, "runtime": 50,
            "hasFile": has_file, "monitored": true })
}

/// Native media management as both apps export it (rename off, hardlinks, no extras).
fn media_management() -> Value {
    json!({ "downloadPropersAndRepacks": "preferAndUpgrade", "copyUsingHardlinks": true, "minimumFreeSpaceWhenImporting": 100,
            "skipFreeSpaceCheckWhenImporting": false, "importExtraFiles": false, "setPermissionsLinux": false, "useScriptImport": false,
            "recycleBin": "", "fileDate": "none", "enableMediaInfo": true, "id": 1 })
}

/// Synthetic snapshot: real native shapes, fictional/fake values.
/// A native-owned file named with the canonical franchise prefix in front of a longer
/// metadata alias (synthetic stand-in for a real library case).
pub const FRANCHISE_FILE: &str =
    "Nebula.The.Legend.of.Arin.The.Last.Keeper.2026.1080p.WEBRip.AAC2.0.x264-GROUP.mp4";
pub const FRANCHISE_FOLDER: &str = "Nebula Arin - The Last Keeper (2026)";
pub const FRANCHISE_ALIASES: [&str; 4] = [
    "Arin: The Last Keeper",
    "The Legend of Arin: The Last Keeper",
    "Arin – Der letzte Hüter",
    "Арин: Последний хранитель",
];

pub fn with_franchise_movie(mut v: Value) -> Value {
    let aliases: Vec<Value> = FRANCHISE_ALIASES
        .iter()
        .map(|t| json!({ "title": t }))
        .collect();
    v["radarr"]["movie"].as_array_mut().unwrap().push(json!({
        "id": 13, "title": "Nebula Arin: The Last Keeper", "originalTitle": "Nebula Arin: The Last Keeper",
        "originalLanguage": english(), "alternateTitles": aliases, "year": 2026, "runtime": 110, "tmdbId": 990001,
        "hasFile": true, "monitored": true, "qualityProfileId": 6, "path": format!("/movies/{FRANCHISE_FOLDER}"),
        "status": "released", "isAvailable": true,
        "movieFile": { "relativePath": FRANCHISE_FILE, "size": 1, "quality": { "quality": quality(15, "WEBRip-1080p") } },
    }));
    v
}

pub fn snapshot() -> Value {
    json!({
        "radarr": {
            "health": [{ "source": "UpdateCheck", "type": "warning", "message": "update available" }],
            "indexer": [indexer(json!([2000, 2040, 2045]))],
            "downloadclient": [download_client("movieCategory", "radarr")],
            "qualityprofile": [radarr_profile()],
            "qualitydefinition": radarr_defs(),
            "rootfolder": [{ "path": "/movies", "accessible": true, "freeSpace": 1000, "id": 1 }],
            "config/downloadclient": { "enableCompletedDownloadHandling": true, "autoRedownloadFailed": true, "id": 1 },
            "config/indexer": { "minimumAge": 0, "maximumSize": 0, "retention": 0, "rssSyncInterval": 30, "preferIndexerFlags": false,
                                "availabilityDelay": 0, "allowHardcodedSubs": false, "whitelistedHardcodedSubs": "", "id": 1 },
            "config/mediamanagement": media_management(),
            "config/naming": { "renameMovies": false, "replaceIllegalCharacters": true, "colonReplacementFormat": "smart",
                               "standardMovieFormat": "{Movie Title} ({Release Year}) {Quality Full}", "movieFolderFormat": "{Movie Title} ({Release Year})", "id": 1 },
            "delayprofile": delay(),
            "customformat": [], "tag": [], "remotepathmapping": [],
            "movie": [
                { "id": 10, "title": "The Matrix", "originalTitle": "The Matrix", "originalLanguage": english(), "alternateTitles": [],
                  "year": 1999, "runtime": 136, "imdbId": "tt0133093", "tmdbId": 603, "hasFile": false, "monitored": true,
                  "qualityProfileId": 6, "path": "/movies/The Matrix (1999)", "status": "released", "isAvailable": true },
                { "id": 11, "title": "The Matrix Reloaded", "originalLanguage": english(), "year": 2003, "runtime": 138, "tmdbId": 604,
                  "hasFile": true, "monitored": true, "qualityProfileId": 6, "path": "/movies/The Matrix Reloaded (2003)", "status": "released",
                  "isAvailable": true, "movieFile": { "quality": { "quality": quality(3, "WEBDL-1080p") } } },
                { "id": 12, "title": "Amélie", "originalTitle": "Le Fabuleux Destin d'Amélie Poulain", "originalLanguage": { "id": 2, "name": "French" },
                  "alternateTitles": [{ "title": "Amelie from Montmartre" }], "year": 2001, "runtime": 122, "tmdbId": 194,
                  "hasFile": false, "monitored": true, "qualityProfileId": 6, "rootFolderPath": "/movies", "isAvailable": true },
            ],
        },
        "sonarr": {
            "health": [],
            "indexer": [indexer(json!([5000, 5040, 5045]))],
            "downloadclient": [download_client("tvCategory", "tv-sonarr")],
            "qualityprofile": [sonarr_profile()],
            "qualitydefinition": sonarr_defs(),
            "languageprofile": [{ "id": 1, "name": "Deprecated", "upgradeAllowed": true, "cutoff": english(), "languages": [{ "language": english(), "allowed": true }] }],
            "rootfolder": [{ "path": "/tvshows", "accessible": true, "freeSpace": 1000, "id": 1 }],
            "config/downloadclient": { "enableCompletedDownloadHandling": true, "id": 1 },
            "config/indexer": { "minimumAge": 0, "retention": 0, "maximumSize": 0, "rssSyncInterval": 30, "id": 1 },
            "config/mediamanagement": media_management(),
            "config/naming": { "renameEpisodes": false, "replaceIllegalCharacters": true, "colonReplacementFormat": 4,
                               "seriesFolderFormat": "{Series Title}", "seasonFolderFormat": "Season {season}", "id": 1 },
            "delayprofile": delay(),
            "customformat": [], "tag": [], "remotepathmapping": [],
            "series": [
                { "id": 20, "title": "Slow Horses", "alternateTitles": [], "year": 2022, "tvdbId": 372264, "tvMazeId": 9001, "imdbId": "tt5875444",
                  "seriesType": "standard", "runtime": 50, "originalLanguage": english(), "status": "continuing", "monitored": true,
                  "qualityProfileId": 6, "languageProfileId": 1, "path": "/tvshows/Slow Horses",
                  "seasons": [
                      { "seasonNumber": 1, "monitored": true, "statistics": { "episodeFileCount": 2, "episodeCount": 6, "totalEpisodeCount": 6 } },
                      { "seasonNumber": 2, "monitored": true, "statistics": { "episodeFileCount": 0, "episodeCount": 6, "totalEpisodeCount": 6 } },
                  ] },
                { "id": 21, "title": "Fargo", "alternateTitles": [], "year": 2014, "tvdbId": 269613, "tvMazeId": 9002,
                  "seriesType": "standard", "runtime": 60, "originalLanguage": english(), "status": "continuing", "monitored": true,
                  "qualityProfileId": 6, "path": "/tvshows/Fargo",
                  "seasons": [
                      { "seasonNumber": 4, "monitored": true, "statistics": { "episodeFileCount": 3, "episodeCount": 11, "totalEpisodeCount": 11 } },
                      { "seasonNumber": 5, "monitored": true, "statistics": { "episodeFileCount": 0, "episodeCount": 10, "totalEpisodeCount": 10 } },
                  ] },
            ],
            "episode_samples": {
                "20": [
                    episode(1, 1, true, "2022-04-01T02:00:00Z"), episode(1, 2, false, "2022-04-01T02:00:00Z"),
                    episode(1, 3, true, "2022-04-08T02:00:00Z"), episode(1, 4, false, "2022-04-15T02:00:00Z"),
                    episode(1, 5, false, "2022-04-22T02:00:00Z"), episode(1, 6, false, "2022-04-29T02:00:00Z"),
                ],
            },
        },
        "prowlarr": {
            "health": [],
            "indexer": [{ "id": 1, "name": "The Pirate Bay", "enable": true, "protocol": "torrent", "privacy": "public", "priority": 25,
                          "definitionName": "thepiratebay", "tags": [],
                          "fields": [field("baseUrl", json!("https://thepiratebay.org/"), "normal"), field("cookie", json!(FAKE_COOKIE), "normal")] }],
            "applications": [{ "name": "Sonarr", "fields": [field("apiKey", json!(FAKE_APIKEY), "apiKey")] }],
        },
    })
}

// ------------------------------------------------------------------ mock upstreams

#[derive(Clone, Debug)]
pub enum Mode {
    Normal,
    Status(u16),
    Delay(Duration),
    Malformed,
    Huge,
    /// First page OK, later pages fail with this status.
    FailAfterFirstPage(u16),
}

pub struct MockState {
    pub mode: Mode,
    /// query -> release JSON objects
    pub releases: HashMap<String, Vec<Value>>,
    pub log: Vec<String>,
    pub header_keys: Vec<Option<String>>,
    /// Queries that fail with HTTP 500 regardless of mode.
    pub failing_queries: Vec<String>,
    /// Queries answered only after this delay.
    pub slow_queries: Vec<(String, Duration)>,
    /// `.torrent` bodies served by `/prowlarr/{id}/download?link=<key>`.
    pub torrents: HashMap<String, Vec<u8>>,
    /// `Location` answered (HTTP 302) for `/prowlarr/{id}/download?link=<key>`, like
    /// Prowlarr's proxy for magnet-only indexers.
    pub redirects: HashMap<String, String>,
    pub downloads: Vec<String>,
    /// Torznab feed XML per indexer id (`/prowlarr/{id}/api`).
    pub feeds: HashMap<String, String>,
    /// When set, feeds answer with this HTTP status.
    pub feed_status: Option<u16>,
    pub feed_log: Vec<String>,
    /// Plex section refreshes received (section ids), with the token check result.
    pub plex: Vec<(String, bool)>,
    /// Extra TVmaze episodes per show id (metadata that appears later).
    pub tvmaze_extra: HashMap<String, Vec<Value>>,
    /// TMDB movie details overriding the built-in ones, per id.
    pub tmdb_movies: HashMap<String, Value>,
    /// Entries of TVmaze's `schedule/web`, answered for every date.
    pub schedule: Vec<Value>,
    /// The `date` of each `schedule/web` request.
    pub schedule_dates: Vec<String>,
    /// `schedule/web` answers HTTP 500.
    pub schedule_fail: bool,
    /// When set, each download fetch is recorded, then answered only once notified.
    pub download_gate: Option<Arc<tokio::sync::Notify>>,
    /// Poster requests received by the TMDB and TVmaze image hosts (paths).
    pub image_log: Vec<String>,
    /// TVmaze `image` values overriding the built-in ones, per show id.
    pub tvmaze_images: HashMap<u32, Value>,
    /// Plex's metadata service answers HTTP 500.
    pub plex_meta_fail: bool,
    /// Each Plex metadata request: its path and query, and whether Arr's headers were right.
    pub plex_meta_log: Vec<(String, bool)>,
    /// Plex keys that have offers now, on top of the built-in ones.
    pub plex_extra_offers: Vec<String>,
    /// Managed stack: TVmaze also lists Slow Horses season 2 (the legacy snapshot's numbering
    /// tests rely on season 1 only).
    pub slow_horses_s2: bool,
}

pub type Mock = Arc<Mutex<MockState>>;

pub fn release(title: &str, size_gib: f64, seeders: i64) -> Value {
    let hash = format!("{:040x}", fxhash(title));
    json!({
        "guid": format!("https://tracker.example/details/{hash}?passkey={FAKE_PASSKEY}"),
        "title": title,
        "size": (size_gib * 1024.0 * 1024.0 * 1024.0) as i64,
        "indexerId": 1,
        "indexer": "The Pirate Bay",
        "seeders": seeders,
        "leechers": 1,
        "protocol": "torrent",
        "publishDate": "2026-01-01T00:00:00Z",
        "infoHash": hash,
        "downloadUrl": format!("http://prowlarr:9696/1/download?apikey={PROWLARR_KEY}&link={FAKE_PASSKEY}"),
        "magnetUrl": format!("magnet:?xt=urn:btih:{hash}&tr=http%3A%2F%2Ftracker.example%2F{FAKE_PASSKEY}%2Fannounce"),
        "infoUrl": format!("https://tracker.example/torrent/{hash}?passkey={FAKE_PASSKEY}"),
        "categories": [{ "id": 5040, "name": "TV/HD" }],
    })
}

fn fxhash(s: &str) -> u128 {
    s.bytes().fold(0xcbf29ce484222325u128, |h, b| {
        (h ^ u128::from(b)).wrapping_mul(0x100000001b3)
    })
}

async fn prowlarr_search(
    State(m): State<Mock>,
    headers: HeaderMap,
    uri: Uri,
    Query(q): Query<Vec<(String, String)>>,
) -> Response {
    let (mode, releases) = {
        let mut s = m.lock().unwrap();
        s.log.push(uri.query().unwrap_or("").to_string());
        s.header_keys.push(
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        );
        let query = q
            .iter()
            .find(|(k, _)| k == "query")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        if s.failing_queries.contains(&query) {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let delay = s
            .slow_queries
            .iter()
            .find(|(q, _)| *q == query)
            .map(|(_, d)| *d);
        (
            match delay {
                Some(d) => Mode::Delay(d),
                None => s.mode.clone(),
            },
            s.releases.get(&query).cloned().unwrap_or_default(),
        )
    };
    if headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some(PROWLARR_KEY) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let offset: usize = q
        .iter()
        .find(|(k, _)| k == "offset")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let limit: usize = q
        .iter()
        .find(|(k, _)| k == "limit")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(100);
    match mode {
        Mode::Status(code) => {
            return (
                StatusCode::from_u16(code).unwrap(),
                format!("error with key {PROWLARR_KEY}"),
            )
                .into_response();
        }
        Mode::Delay(d) => tokio::time::sleep(d).await,
        Mode::Malformed => {
            return (
                StatusCode::OK,
                [("content-type", "application/json")],
                format!("[{{\"title\": \"{FAKE_PASSKEY}\", \"size\": \"x\""),
            )
                .into_response();
        }
        Mode::Huge => {
            return (
                StatusCode::OK,
                "[".to_string() + &"{\"title\":\"x\"},".repeat(200_000) + "{}]",
            )
                .into_response();
        }
        Mode::FailAfterFirstPage(code) if offset > 0 => {
            return StatusCode::from_u16(code).unwrap().into_response();
        }
        _ => {}
    }
    let page: Vec<Value> = releases.into_iter().skip(offset).take(limit).collect();
    axum::Json(page).into_response()
}

fn tvmaze_show(id: u32, name: &str, premiered: &str, tvdb: Option<u32>) -> Value {
    json!({ "id": id, "name": name, "type": "Scripted", "language": "English", "status": "Running", "runtime": 60,
            "premiered": premiered, "externals": { "tvrage": null, "thetvdb": tvdb, "imdb": null } })
}

fn shows() -> Vec<Value> {
    vec![
        tvmaze_show(9001, "Slow Horses", "2022-04-01", Some(372264)),
        tvmaze_show(9002, "Fargo", "2014-04-15", Some(269613)),
        tvmaze_show(9003, "WAR", "2026-01-05", Some(999001)),
        tvmaze_show(9004, "War", "2019-03-01", None),
        tvmaze_show(9005, "Parity Partial", "2025-01-10", Some(999005)),
        tvmaze_show(9006, "Coming Soon Show", "2099-01-10", Some(999006)),
        tvmaze_show(9007, "Parity Show", "2024-05-01", Some(999002)),
        tvmaze_show(9100, "Backlog Show", "2010-01-01", Some(999100)),
        tvmaze_show(9200, "Scan Show", "2025-01-01", Some(999200)),
        tvmaze_show(9467, "WAR (2026)", "2025-12-30", Some(467092)),
    ]
}

/// Parity Partial (TVmaze 9005, TVDB 999005): S1E1-E2 aired, S1E3 in 2099; S2E1-E2 aired.
fn parity_partial_episodes() -> Vec<Value> {
    let ep = |s: u32, n: u32, at: &str| json!({ "season": s, "number": n, "name": format!("Ep {n}"), "type": "regular", "airstamp": at, "runtime": 1 });
    vec![
        ep(1, 1, "2025-01-10T02:00:00+00:00"),
        ep(1, 2, "2025-01-17T02:00:00+00:00"),
        ep(1, 3, "2099-01-24T02:00:00+00:00"),
        ep(2, 1, "2025-06-01T02:00:00+00:00"),
        ep(2, 2, "2025-06-08T02:00:00+00:00"),
    ]
}

async fn tvmaze_lookup(Query(q): Query<HashMap<String, String>>) -> Response {
    let tvdb: Option<u32> = q.get("thetvdb").and_then(|v| v.parse().ok());
    match shows()
        .into_iter()
        .find(|s| s["externals"]["thetvdb"].as_u64().map(|v| v as u32) == tvdb)
    {
        Some(s) => (
            StatusCode::MOVED_PERMANENTLY,
            [("location", format!("/tvmaze/shows/{}", s["id"]))],
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn tvmaze_show_by_id(
    State(m): State<Mock>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match shows()
        .into_iter()
        .find(|s| s["id"].as_u64().map(|n| n.to_string()).as_deref() == Some(id.as_str()))
    {
        Some(mut s) => {
            let n = s["id"].as_u64().unwrap_or(0) as u32;
            let host = headers["host"].to_str().unwrap_or_default().to_string();
            s["image"] = m
                .lock()
                .unwrap()
                .tvmaze_images
                .get(&n)
                .cloned()
                .unwrap_or(json!({ "medium": format!("http://{host}/uploads/{n}.jpg") }));
            axum::Json(s).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn tvmaze_episodes(
    State(m): State<Mock>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let extra = m
        .lock()
        .unwrap()
        .tvmaze_extra
        .get(&id)
        .cloned()
        .unwrap_or_default();
    if id == "9006" {
        return axum::Json(vec![json!({ "season": 1, "number": 1, "name": "Pilot", "type": "regular", "airstamp": "2099-01-10T02:00:00+00:00", "runtime": 30 })]).into_response();
    }
    if id == "9007" {
        let eps: Vec<Value> = (1..=3)
            .map(|n| json!({ "season": 1, "number": n, "name": format!("Ep {n}"), "type": "regular", "airstamp": format!("2024-05-0{n}T02:00:00+00:00"), "runtime": 50 }))
            .collect();
        return axum::Json(eps).into_response();
    }
    if id == "9005" {
        let mut eps = parity_partial_episodes();
        eps.extend(extra);
        return axum::Json(eps).into_response();
    }
    let s2 = m.lock().unwrap().slow_horses_s2;
    let seasons: &[(u32, u32)] = match id.as_str() {
        "9002" => &[(4, 11), (5, 10)],
        "9003" => &[(1, 8)],
        "9001" if s2 => &[(1, 6), (2, 6)],
        "9001" => &[(1, 6)],
        "9200" => &[(1, 12)],
        // WAR (2026): its episodes come from `tvmaze_extra`.
        "9467" => &[],
        // Backlog Show: its episodes come from `tvmaze_extra`.
        "9100" => &[],
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut eps: Vec<Value> = vec![];
    for (season, count) in seasons {
        eps.extend((1..=*count).map(|n| json!({ "season": season, "number": n, "name": format!("Ep {n}"), "type": "regular", "airstamp": format!("2025-02-{:02}T02:00:00+00:00", n), "runtime": 60 })));
        eps.push(json!({ "season": season, "number": null, "name": "Special", "type": "significant_special", "airstamp": "2025-03-01T02:00:00+00:00" }));
    }
    eps.extend(extra);
    axum::Json(eps).into_response()
}

async fn tvmaze_schedule(
    State(m): State<Mock>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut m = m.lock().unwrap();
    m.schedule_dates
        .push(q.get("date").cloned().unwrap_or_default());
    if m.schedule_fail {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    axum::Json(m.schedule.clone()).into_response()
}

async fn tvmaze_search(Query(q): Query<HashMap<String, String>>) -> Response {
    let needle = q.get("q").cloned().unwrap_or_default().to_lowercase();
    let hits: Vec<Value> = shows()
        .into_iter()
        .filter(|s| {
            s["name"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains(&needle)
        })
        .map(|s| json!({ "score": 1.0, "show": s }))
        .collect();
    axum::Json(hits).into_response()
}

fn tmdb_authorized(headers: &HeaderMap, q: &HashMap<String, String>) -> bool {
    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {TMDB_V4_TOKEN}"));
    bearer || q.get("api_key").map(String::as_str) == Some(TMDB_V3_KEY)
}

fn tmdb_movie() -> Value {
    json!({ "id": 900001, "title": "Leviticus", "original_title": "Leviticus", "original_language": "en",
            "poster_path": "/leviticus.jpg",
            "release_date": "2025-06-01", "runtime": 86, "imdb_id": "tt9000001", "status": "Released",
            "alternative_titles": { "titles": [{ "iso_3166_1": "US", "title": "Leviticus (Synthetic)" }] },
            "release_dates": { "results": [{ "iso_3166_1": "US", "release_dates": [
                { "type": 3, "release_date": "2025-06-01T00:00:00.000Z" }, { "type": 4, "release_date": "2025-08-15T00:00:00.000Z" } ] }] } })
}

/// A TMDB movie with the given digital release date (YYYY-MM-DD).
pub fn tmdb_movie_with(id: u32, title: &str, year: u16, digital: &str) -> Value {
    json!({ "id": id, "title": title, "original_title": title, "original_language": "en",
            "release_date": format!("{year}-01-01"), "runtime": 1, "status": "Post Production",
            "alternative_titles": { "titles": [] },
            "release_dates": { "results": [{ "iso_3166_1": "US", "release_dates": [ { "type": 4, "release_date": format!("{digital}T00:00:00.000Z") } ] }] } })
}

async fn tmdb_search(headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    if !tmdb_authorized(&headers, &q) {
        return (StatusCode::UNAUTHORIZED, "{\"status_code\":7}").into_response();
    }
    let hits: Vec<Value> = if q
        .get("query")
        .is_some_and(|v| v.eq_ignore_ascii_case("leviticus"))
    {
        vec![tmdb_movie()]
    } else {
        vec![]
    };
    axum::Json(json!({ "page": 1, "results": hits, "total_pages": 1, "total_results": hits.len() }))
        .into_response()
}

async fn tmdb_details(
    State(m): State<Mock>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !tmdb_authorized(&headers, &q) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(v) = m.lock().unwrap().tmdb_movies.get(&id).cloned() {
        return axum::Json(v).into_response();
    }
    if id == "900001" {
        axum::Json(tmdb_movie()).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

// ------------------------------------------------------------------ Plex metadata service

/// A Plex movie: `(key, tmdb id, title, original title, year, premiere, country, offers)`.
/// Released with offers; unreleased with none; foreign with an original title; no TMDB guid.
pub const PLEX_RELEASED: u32 = 910001;
pub const PLEX_UNRELEASED: u32 = 910002;
pub const PLEX_FOREIGN: u32 = 910003;

fn plex_fixtures() -> Vec<Value> {
    let movie = |key: &str,
                 imdb: &str,
                 tmdb: Option<u32>,
                 title: &str,
                 original: Option<&str>,
                 date: &str,
                 country: &str| {
        let mut guids = vec![json!({ "id": format!("imdb://{imdb}") })];
        if let Some(t) = tmdb {
            guids.push(json!({ "id": format!("tmdb://{t}") }));
        }
        let mut v = json!({
            "ratingKey": key, "guid": format!("plex://movie/{key}"), "type": "movie", "title": title,
            "year": date[..4].parse::<u16>().unwrap(), "originallyAvailableAt": date, "duration": 6_000_000,
            "Guid": guids, "Country": [{ "tag": country }],
            // The host is the mock's (127.0.0.1), except the foreign movie's: not Plex's image host.
            "thumb": if key == "for0001" { format!("https://elsewhere.example/uploads/{key}.jpg") } else { format!("http://127.0.0.1/uploads/{key}.jpg") },
        });
        if let Some(o) = original {
            v["originalTitle"] = json!(o);
        }
        v
    };
    vec![
        movie(
            "rel0001",
            "tt9100001",
            Some(PLEX_RELEASED),
            "Plex Released Movie",
            None,
            "2024-05-01",
            "United States of America",
        ),
        movie(
            "fut0001",
            "tt9100002",
            Some(PLEX_UNRELEASED),
            "Plex Future Movie",
            None,
            "2099-01-01",
            "United Kingdom",
        ),
        movie(
            "for0001",
            "tt9100003",
            Some(PLEX_FOREIGN),
            "Plex Foreign Movie",
            Some("Le Film Etranger"),
            "2001-04-25",
            "France",
        ),
        movie(
            "nog0001",
            "tt9100004",
            None,
            "Plex Unmatched Movie",
            None,
            "2020-02-02",
            "Canada",
        ),
    ]
}

/// Records the request; false when Arr's Plex headers are missing or wrong.
fn plex_meta_seen(m: &Mock, headers: &HeaderMap, uri: &Uri) -> bool {
    let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    let good = header("x-plex-token") == Some(PLEX_TOKEN)
        && header("x-plex-product") == Some("Arr")
        && header("x-plex-client-identifier").is_some_and(|c| !c.is_empty())
        && header("accept") == Some("application/json");
    let mut s = m.lock().unwrap();
    s.plex_meta_log.push((uri.to_string(), good));
    good && !s.plex_meta_fail
}

fn plex_denied(m: &Mock) -> Response {
    if m.lock().unwrap().plex_meta_fail {
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn plex_search(
    State(m): State<Mock>,
    headers: HeaderMap,
    uri: Uri,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !plex_meta_seen(&m, &headers, &uri) {
        return plex_denied(&m);
    }
    let query = q.get("query").map(|s| s.to_lowercase()).unwrap_or_default();
    let hits: Vec<Value> = plex_fixtures()
        .into_iter()
        .filter(|f| {
            f["title"]
                .as_str()
                .is_some_and(|t| t.to_lowercase().contains(&query))
        })
        .map(|f| {
            // Search hits carry no external ids.
            let mut hit = f.clone();
            hit.as_object_mut().unwrap().remove("Guid");
            json!({ "Metadata": hit })
        })
        .collect();
    axum::Json(json!({ "MediaContainer": { "SearchResults": [{ "SearchResult": hits }] } }))
        .into_response()
}

async fn plex_matches(
    State(m): State<Mock>,
    headers: HeaderMap,
    uri: Uri,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !plex_meta_seen(&m, &headers, &uri) {
        return plex_denied(&m);
    }
    let guid = q.get("guid").cloned().unwrap_or_default();
    let found: Vec<Value> = plex_fixtures()
        .into_iter()
        .filter(|f| {
            f["Guid"]
                .as_array()
                .unwrap()
                .iter()
                .any(|g| g["id"] == guid.as_str())
        })
        .collect();
    axum::Json(json!({ "MediaContainer": { "Metadata": found } })).into_response()
}

async fn plex_details(
    State(m): State<Mock>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(key): axum::extract::Path<String>,
) -> Response {
    if !plex_meta_seen(&m, &headers, &uri) {
        return plex_denied(&m);
    }
    match plex_fixtures()
        .into_iter()
        .find(|f| f["ratingKey"] == key.as_str())
    {
        Some(f) => axum::Json(json!({ "MediaContainer": { "Metadata": [f] } })).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn plex_offers(
    State(m): State<Mock>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(key): axum::extract::Path<String>,
) -> Response {
    if !plex_meta_seen(&m, &headers, &uri) {
        return plex_denied(&m);
    }
    let offers = match key.as_str() {
        "rel0001" => vec![
            json!({ "platform": "netflix", "offerType": "subscription" }),
            json!({ "platform": "apple", "offerType": "buy" }),
        ],
        "for0001" => vec![json!({ "platform": "apple", "offerType": "rent" })],
        _ if m.lock().unwrap().plex_extra_offers.contains(&key) => {
            vec![json!({ "platform": "apple", "offerType": "buy" })]
        }
        _ => vec![],
    };
    axum::Json(json!({ "MediaContainer": { "Availability": offers } })).into_response()
}

/// A tiny JPEG (the header bytes are all the app checks).
const TINY_JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 0xff, 0xd9,
];

async fn image_host(State(m): State<Mock>, uri: Uri) -> Response {
    m.lock().unwrap().image_log.push(uri.path().to_string());
    ([("content-type", "image/jpeg")], TINY_JPEG).into_response()
}

async fn prowlarr_download(
    State(m): State<Mock>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some(PROWLARR_KEY) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let link = q.get("link").cloned().unwrap_or_default();
    let gate = {
        let mut s = m.lock().unwrap();
        s.downloads.push(link.clone());
        s.download_gate.clone()
    };
    if let Some(gate) = gate {
        gate.notified().await;
    }
    let s = m.lock().unwrap();
    if let Some(location) = s.redirects.get(&link) {
        return (StatusCode::FOUND, [("location", location.clone())]).into_response();
    }
    match s.torrents.get(&link) {
        Some(b) => ([("content-type", "application/x-bittorrent")], b.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn prowlarr_feed(
    State(m): State<Mock>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut s = m.lock().unwrap();
    let key_ok = headers.get("x-api-key").and_then(|v| v.to_str().ok()) == Some(PROWLARR_KEY)
        && q.get("apikey").map(String::as_str) == Some(PROWLARR_KEY);
    s.feed_log.push(format!(
        "{id} t={} cat={} limit={}",
        q.get("t").cloned().unwrap_or_default(),
        q.get("cat").cloned().unwrap_or_default(),
        q.get("limit").cloned().unwrap_or_default()
    ));
    if !key_ok {
        return (
            StatusCode::OK,
            "<?xml version=\"1.0\"?><error code=\"100\" description=\"Invalid API Key\"/>",
        )
            .into_response();
    }
    if let Some(code) = s.feed_status {
        return StatusCode::from_u16(code).unwrap().into_response();
    }
    match s.feeds.get(&id) {
        Some(x) => ([("content-type", "application/rss+xml")], x.clone()).into_response(),
        None => ([("content-type", "application/rss+xml")], feed_xml(&[])).into_response(),
    }
}

// ------------------------------------------------------------------ native Torznab endpoint

pub const TZ_KEY: &str = "FAKE-TEST-TORZNAB-KEY-0001";

const TZ_CAPS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><caps><server title="Mock"/><limits max="100" default="50"/><searching><search available="yes" supportedParams="q"/><tv-search available="yes" supportedParams="q,season,ep"/><movie-search available="yes" supportedParams="q"/></searching><categories><category id="2000" name="Movies"/><category id="5000" name="TV"><subcat id="5040" name="TV/HD"/></category></categories></caps>"#;

/// A Prowlarr-style release as a Torznab `<item>` of this endpoint: the download link is the
/// endpoint's own `/tz/dl/<link>` (what Prowlarr's `link` parameter named), the magnet and
/// the other attributes as given.
fn torznab_item(r: &Value, host: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('"', "&quot;")
    };
    let link = r["downloadUrl"].as_str().and_then(|u| {
        reqwest::Url::parse(u).ok().and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "link")
                .map(|(_, v)| format!("http://{host}/tz/dl/{v}"))
        })
    });
    let date = r["publishDate"]
        .as_str()
        .and_then(|d| {
            time::OffsetDateTime::parse(d, &time::format_description::well_known::Rfc3339).ok()
        })
        .and_then(|d| {
            d.format(&time::format_description::well_known::Rfc2822)
                .ok()
        })
        .unwrap_or_else(|| "Mon, 26 Oct 2026 08:00:00 +0000".into());
    let size = r["size"].as_i64().unwrap_or(0);
    let seeders_known = r["seeders"].as_i64();
    let seeders = seeders_known.unwrap_or(0);
    let leechers = r["leechers"].as_i64().unwrap_or(0);
    let cats: String = r["categories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_i64())
        .map(|c| format!("<torznab:attr name=\"category\" value=\"{c}\" />"))
        .collect();
    format!(
        "<item><title>{}</title><guid isPermaLink=\"false\">{}</guid><prowlarrindexer id=\"{}\">{}</prowlarrindexer>{}<pubDate>{date}</pubDate><size>{size}</size>{}{cats}{}{}{}{}</item>\n",
        esc(r["title"].as_str().unwrap_or("")),
        esc(r["guid"].as_str().unwrap_or("")),
        r["indexerId"].as_i64().unwrap_or(1),
        esc(r["indexer"].as_str().unwrap_or("The Pirate Bay")),
        link.as_ref()
            .map(|l| format!("<link>{}</link>", esc(l)))
            .unwrap_or_default(),
        link.as_ref()
            .map(|l| format!(
                "<enclosure url=\"{}\" length=\"{size}\" type=\"application/x-bittorrent\" />",
                esc(l)
            ))
            .unwrap_or_default(),
        seeders_known
            .map(|n| format!("<torznab:attr name=\"seeders\" value=\"{n}\" />"))
            .unwrap_or_default(),
        seeders_known
            .map(|_| format!(
                "<torznab:attr name=\"peers\" value=\"{}\" />",
                seeders + leechers
            ))
            .unwrap_or_default(),
        r["infoHash"]
            .as_str()
            .map(|h| format!("<torznab:attr name=\"infohash\" value=\"{h}\" />"))
            .unwrap_or_default(),
        r["magnetUrl"]
            .as_str()
            .map(|m| format!("<torznab:attr name=\"magneturl\" value=\"{}\" />", esc(m)))
            .unwrap_or_default(),
    )
}

fn torznab_rss(items: &str) -> Response {
    (
        [("content-type", "application/rss+xml")],
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\" xmlns:torznab=\"http://torznab.com/schemas/2015/feed\">\n<channel><title>Mock</title>\n{items}</channel></rss>"
        ),
    )
        .into_response()
}

/// The native Torznab endpoint (`/tz/api`): `t=caps`, `t=search&q=` answered from
/// `releases[q]` (the same data the Prowlarr mock serves), and `t=search` without a query
/// (RSS) answered from `feeds["1"]`. The modes and faults of the Prowlarr mock apply.
async fn tz_api(
    State(m): State<Mock>,
    headers: HeaderMap,
    uri: Uri,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if q.get("apikey").map(String::as_str) != Some(TZ_KEY) {
        return (
            [("content-type", "application/rss+xml")],
            "<?xml version=\"1.0\"?><error code=\"100\" description=\"Incorrect user credentials\"/>",
        )
            .into_response();
    }
    if q.get("t").map(String::as_str) == Some("caps") {
        return ([("content-type", "application/rss+xml")], TZ_CAPS).into_response();
    }
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let query = q.get("q").cloned().unwrap_or_default();
    let offset: usize = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100);
    if query.is_empty() {
        // RSS.
        let mut s = m.lock().unwrap();
        s.feed_log.push(format!(
            "1 t=search cat={} limit={}",
            q.get("cat").cloned().unwrap_or_default(),
            q.get("limit").cloned().unwrap_or_default()
        ));
        if let Some(code) = s.feed_status {
            return StatusCode::from_u16(code).unwrap().into_response();
        }
        return match s.feeds.get("1") {
            Some(x) => ([("content-type", "application/rss+xml")], x.clone()).into_response(),
            None => ([("content-type", "application/rss+xml")], feed_xml(&[])).into_response(),
        };
    }
    let (mode, releases) = {
        let mut s = m.lock().unwrap();
        s.log.push(uri.query().unwrap_or("").to_string());
        if s.failing_queries.contains(&query) {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let delay = s
            .slow_queries
            .iter()
            .find(|(q, _)| *q == query)
            .map(|(_, d)| *d);
        (
            match delay {
                Some(d) => Mode::Delay(d),
                None => s.mode.clone(),
            },
            s.releases.get(&query).cloned().unwrap_or_default(),
        )
    };
    match mode {
        Mode::Status(code) => return StatusCode::from_u16(code).unwrap().into_response(),
        Mode::Delay(d) => tokio::time::sleep(d).await,
        Mode::Malformed => {
            return (
                StatusCode::OK,
                [("content-type", "application/rss+xml")],
                "<rss><channel><item><title>",
            )
                .into_response();
        }
        Mode::Huge => {
            return torznab_rss(&"<item><title>x</title></item>".repeat(300_000));
        }
        Mode::FailAfterFirstPage(code) if offset > 0 => {
            return StatusCode::from_u16(code).unwrap().into_response();
        }
        _ => {}
    }
    let items: String = releases
        .iter()
        .skip(offset)
        .take(limit)
        .map(|r| torznab_item(r, &host))
        .collect();
    torznab_rss(&items)
}

async fn tz_download(
    State(m): State<Mock>,
    axum::extract::Path(link): axum::extract::Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if q.get("apikey").map(String::as_str) != Some(TZ_KEY) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let gate = {
        let mut s = m.lock().unwrap();
        s.downloads.push(link.clone());
        s.download_gate.clone()
    };
    if let Some(gate) = gate {
        gate.notified().await;
    }
    let s = m.lock().unwrap();
    if let Some(location) = s.redirects.get(&link) {
        return (StatusCode::FOUND, [("location", location.clone())]).into_response();
    }
    match s.torrents.get(&link) {
        Some(b) => ([("content-type", "application/x-bittorrent")], b.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn plex_refresh(
    State(m): State<Mock>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let ok = headers.get("x-plex-token").and_then(|v| v.to_str().ok()) == Some(PLEX_TOKEN);
    m.lock().unwrap().plex.push((id, ok));
    if ok {
        StatusCode::OK.into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// A Torznab RSS document with `(title, download url, size, seeders, infohash, categories, pubDate)` items.
pub fn feed_xml(items: &[FeedItem]) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\" xmlns:torznab=\"http://torznab.com/schemas/2015/feed\">\n<channel><atom:link rel=\"self\" type=\"application/rss+xml\" href=\"http://example/\" /><title>Mock</title>\n",
    );
    for i in items {
        let esc = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('"', "&quot;")
        };
        x.push_str(&format!(
            "<item><title>{}</title><guid isPermaLink=\"false\">{}</guid><prowlarrindexer id=\"1\">The Pirate Bay</prowlarrindexer><description>&lt;img src=\"http://evil.example/x.png\"&gt;</description><link>{}</link><comments>https://tracker.example/details/1</comments><pubDate>{}</pubDate><size>{}</size>{}<enclosure url=\"{}\" length=\"{}\" type=\"application/x-bittorrent\" /><torznab:attr name=\"seeders\" value=\"{}\" /><torznab:attr name=\"peers\" value=\"{}\" />{}{}</item>\n",
            esc(&i.title), esc(&i.guid), esc(&i.url), i.pub_date, i.size,
            i.categories.iter().map(|c| format!("<category>{c}</category>")).collect::<String>(),
            esc(&i.url), i.size, i.seeders, i.seeders + 2,
            i.categories.iter().map(|c| format!("<torznab:attr name=\"category\" value=\"{c}\" />")).collect::<String>(),
            i.info_hash.as_ref().map(|h| format!("<torznab:attr name=\"infohash\" value=\"{h}\" />")).unwrap_or_default(),
        ));
    }
    x.push_str("</channel></rss>");
    x
}

#[derive(Clone, Debug)]
pub struct FeedItem {
    pub title: String,
    pub guid: String,
    pub url: String,
    pub size: i64,
    pub seeders: i64,
    pub info_hash: Option<String>,
    pub categories: Vec<u32>,
    pub pub_date: String,
}

pub const PLEX_TOKEN: &str = "FAKE-TEST-PLEX-TOKEN-0001";

pub async fn start_mock() -> (SocketAddr, Mock) {
    let state: Mock = Arc::new(Mutex::new(MockState {
        mode: Mode::Normal,
        releases: HashMap::new(),
        log: vec![],
        header_keys: vec![],
        failing_queries: vec![],
        slow_queries: vec![],
        torrents: HashMap::new(),
        redirects: HashMap::new(),
        downloads: vec![],
        feeds: HashMap::new(),
        feed_status: None,
        feed_log: vec![],
        plex: vec![],
        tvmaze_extra: HashMap::new(),
        tmdb_movies: HashMap::new(),
        schedule: vec![],
        schedule_dates: vec![],
        schedule_fail: false,
        download_gate: None,
        image_log: vec![],
        tvmaze_images: HashMap::new(),
        plex_meta_fail: false,
        plex_meta_log: vec![],
        plex_extra_offers: vec![],
        slow_horses_s2: false,
    }));
    let app = Router::new()
        .route("/prowlarr/api/v1/search", get(prowlarr_search))
        .route("/tvmaze/lookup/shows", get(tvmaze_lookup))
        .route("/tvmaze/shows/{id}", get(tvmaze_show_by_id))
        .route("/tvmaze/shows/{id}/episodes", get(tvmaze_episodes))
        .route("/tvmaze/search/shows", get(tvmaze_search))
        .route("/tvmaze/schedule/web", get(tvmaze_schedule))
        .route("/tmdb/3/search/movie", get(tmdb_search))
        .route("/tmdb/3/movie/{id}", get(tmdb_details))
        .route("/plexmeta/library/search", get(plex_search))
        .route("/plexmeta/library/metadata/matches", get(plex_matches))
        .route("/plexmeta/library/metadata/{key}", get(plex_details))
        .route(
            "/plexmeta/library/metadata/{key}/availabilities",
            get(plex_offers),
        )
        .route("/t/p/w342/{file}", get(image_host))
        .route("/uploads/{file}", get(image_host))
        .route("/prowlarr/{id}/download", get(prowlarr_download))
        .route("/prowlarr/{id}/api", get(prowlarr_feed))
        .route("/tz/api", get(tz_api))
        .route("/tz/dl/{link}", get(tz_download))
        .route(
            "/plex/library/sections/{id}/refresh",
            axum::routing::post(plex_refresh),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, state)
}

// ------------------------------------------------------------------ app harness

pub struct App {
    pub base: String,
    pub dir: PathBuf,
    pub client: reqwest::Client,
    _tmp: Option<tempfile::TempDir>,
}

impl App {
    pub fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
    }

    pub fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
    }
}

pub fn write_state(
    dir: &Path,
    snapshot: &Value,
    extra_config: &str,
    mock: Option<SocketAddr>,
    tvmaze: bool,
) {
    fsutil::ensure_private_dir(dir).unwrap();
    fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
    let write = |p: PathBuf, b: &[u8]| {
        if !p.exists() {
            fsutil::write_new_private_file(&p, b).unwrap();
        }
    };
    write(dir.join("secrets/api_token"), TOKEN.as_bytes());
    write(
        dir.join("secrets/prowlarr_api_key"),
        PROWLARR_KEY.as_bytes(),
    );
    let mut cfg = "[server]\napi_token_file = \"secrets/api_token\"\nrequest_timeout_secs = 30\nmax_body_bytes = 4096\n".to_string();
    if let Some(addr) = mock {
        cfg.push_str(&format!(
            "[prowlarr]\nbase_url = \"http://{addr}/prowlarr\"\napi_key_file = \"secrets/prowlarr_api_key\"\ntimeout_secs = 2\npage_size = 100\nmax_pages = 2\nmax_response_bytes = 1048576\n"
        ));
        if tvmaze {
            cfg.push_str(&format!(
                "[tvmaze]\nenabled = true\nbase_url = \"http://{addr}/tvmaze\"\nimage_base_url = \"http://{addr}\"\ntimeout_secs = 2\n"
            ));
        }
    }
    cfg.push_str(extra_config);
    // Existing suites model the legacy behaviour (imported titles acquire); coexistence tests
    // opt in with their own [migration] section.
    if !extra_config.contains("[migration]") {
        cfg.push_str("[migration]\nmode = \"legacy\"\n");
    }
    let cfg_path = dir.join("config.toml");
    let _ = std::fs::remove_file(&cfg_path);
    fsutil::write_new_private_file(&cfg_path, cfg.as_bytes()).unwrap();
    let snap = dir.join("native.json");
    let _ = std::fs::remove_file(&snap);
    fsutil::write_new_private_file(&snap, &serde_json::to_vec(snapshot).unwrap()).unwrap();
    import::run_import(&import::ImportOptions {
        state_dir: dir.to_path_buf(),
        snapshot: snap,
        observed_at: Some(time::macros::datetime!(2026-10-01 12:00 UTC)),
        dry_run: false,
    })
    .unwrap();
}

/// Starts the API in-process on a loopback port against `dir` (already populated).
pub async fn serve_dir(dir: &Path) -> String {
    let cfg = config::load(&dir.join("config.toml")).unwrap();
    let state = Arc::new(api::AppState::new(cfg).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, api::router(state)).await.unwrap() });
    format!("http://{addr}")
}

pub async fn start_app(
    snapshot: &Value,
    extra_config: &str,
    mock: Option<SocketAddr>,
    tvmaze: bool,
) -> App {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    write_state(&dir, snapshot, extra_config, mock, tvmaze);
    let base = serve_dir(&dir).await;
    App {
        base,
        dir,
        client: reqwest::Client::new(),
        _tmp: Some(tmp),
    }
}

pub fn assert_no_leaks(body: &str) {
    for secret in [
        FAKE_APIKEY,
        FAKE_PASSWORD,
        FAKE_USER,
        FAKE_COOKIE,
        FAKE_PASSKEY,
        PROWLARR_KEY,
        "magnet:",
        "downloadUrl",
        "infoHash",
        "btih",
        "/download?",
    ] {
        assert!(
            !body.contains(secret),
            "response leaked {secret:?}: {}",
            &body[..body.len().min(400)]
        );
    }
}

// ------------------------------------------------------------------ execution harness

pub const QB_API_4_3_9: &str = "2.8.2";
pub const QB_API_5_0: &str = "2.11.2";

/// Real playable media generated once per duration (ffmpeg testsrc, MPEG-4 in Matroska).
pub fn mkv(seconds: u32) -> Vec<u8> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<u32, Vec<u8>>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(b) = cache.lock().unwrap().get(&seconds) {
        return b.clone();
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("clip.mkv");
    let bytes = arr::fixture::generate_mkv(&ff("ffmpeg"), &out, seconds)
        .expect("these tests need /usr/bin/ffmpeg and /usr/bin/ffprobe (Debian package ffmpeg)");
    cache.lock().unwrap().insert(seconds, bytes.clone());
    bytes
}

type SizedCache = HashMap<(u32, u32, u32), Vec<u8>>;

/// Real playable media of a given size (1 fps), cached per (seconds, width, height).
pub fn mkv_sized(seconds: u32, width: u32, height: u32) -> Vec<u8> {
    static CACHE: std::sync::OnceLock<Mutex<SizedCache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(b) = cache.lock().unwrap().get(&(seconds, width, height)) {
        return b.clone();
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("clip.mkv");
    let bytes = arr::fixture::generate_video(&ff("ffmpeg"), &out, seconds, width, height).unwrap();
    cache
        .lock()
        .unwrap()
        .insert((seconds, width, height), bytes.clone());
    bytes
}

/// ffmpeg or ffprobe: Debian's on Unix; on Windows, the first on the PATH (CI puts FFmpeg's
/// real bin folder there).
pub fn ff(tool: &str) -> PathBuf {
    if cfg!(unix) {
        return PathBuf::from(format!("/usr/bin/{tool}"));
    }
    std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(format!("{tool}.exe")))
                .find(|f| f.is_file())
        })
        .unwrap_or_else(|| PathBuf::from(format!("{tool}.exe")))
}

/// An audio-only Matroska file (no video stream).
pub fn audio_only() -> Vec<u8> {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("a.mka");
    let ok = std::process::Command::new(ff("ffmpeg"))
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=2",
            "-c:a",
            "pcm_s16le",
            "-f",
            "matroska",
        ])
        .arg(&out)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    std::fs::read(out).unwrap()
}

/// Which stack `start_exec` builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stack {
    /// The real default stack: managed setup (an empty catalog plus the titles the test adds),
    /// a native Torznab indexer, the in-memory download client standing in for the embedded
    /// engine, `[migration] mode = "coexistence"`, and the Plex/TVmaze/TMDB mocks.
    Managed,
    /// LEGACY ONLY. A synthetic Sonarr/Radarr snapshot import, `[prowlarr]`, the qBittorrent
    /// mock and `[migration] mode = "legacy"`. Kept for the suites that test those
    /// integrations (import, coexistence, migration_review, links, execution, sandbox) until
    /// they are deleted with them; nothing new belongs here.
    Legacy,
}

pub struct ExecOptions {
    pub stack: Stack,
    pub sample_detection: bool,
    pub cleanup: bool,
    pub tvmaze: bool,
    /// Extra TOML appended (whole sections only).
    pub extra: String,
    /// Configure [tmdb] against the mock (v3 key).
    pub tmdb: bool,
    /// Download categories: `[qbittorrent]` movie/TV categories (legacy; default the imported
    /// native ones) or the managed setup's `[download]` (default `um-movies` / `um-tv`).
    pub categories: Option<(&'static str, &'static str)>,
    /// `[automation] stall_timeout_minutes` (default: the configuration's default).
    pub stall_timeout_minutes: Option<u64>,
    /// Managed: add the standard titles (see `seed_library`) before the test starts.
    pub library: bool,
    /// Managed: configure `[plex]` against the mock (the default stack has it).
    pub plex: bool,
    /// Managed: execution enabled (the embedded engine's stand-in, the worker's writes).
    /// False is the shadow-mode stack: planning only, no download client.
    pub execution: bool,
    /// Managed: configure the native Torznab indexer (false: no search backend at all).
    pub indexer: bool,
    /// Managed: where the Torznab indexer is (default: the mock), its result cache (seconds)
    /// and page size.
    pub indexer_addr: Option<SocketAddr>,
    pub indexer_cache_secs: u64,
    pub page_size: u32,
    /// Managed: run the managed setup (false: a server with nothing set up yet).
    pub setup: bool,
    /// Managed: extra keys for the `[server]` section.
    pub server_extra: String,
    /// Managed: `[[profiles]]` entries for the setup document (replacing the default one).
    pub profiles: String,
    /// Managed: extra keys for the setup document's `[download]`, `[tv]` and `[movies]` tables.
    pub download_extra: String,
    pub tv_extra: String,
    pub movies_extra: String,
    /// LEGACY: qBittorrent WebAPI version of the mock.
    pub api: &'static str,
    /// LEGACY: the Sonarr/Radarr/Prowlarr snapshot to import.
    pub snapshot: Value,
    /// LEGACY: `[migration] mode` (`legacy` by default for the legacy suites).
    pub mode: &'static str,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            stack: Stack::Managed,
            sample_detection: false,
            cleanup: true,
            tvmaze: true,
            extra: String::new(),
            tmdb: false,
            categories: None,
            stall_timeout_minutes: None,
            library: true,
            plex: true,
            execution: true,
            indexer: true,
            indexer_addr: None,
            indexer_cache_secs: 0,
            page_size: 100,
            setup: true,
            server_extra: String::new(),
            profiles: String::new(),
            download_extra: String::new(),
            tv_extra: String::new(),
            movies_extra: String::new(),
            api: QB_API_4_3_9,
            snapshot: snapshot(),
            mode: "legacy",
        }
    }
}

impl ExecOptions {
    /// The managed stack in shadow mode: execution off, so plans are never executed.
    pub fn shadow() -> Self {
        ExecOptions {
            execution: false,
            ..Default::default()
        }
    }

    /// The legacy stack (snapshot import, Prowlarr, qBittorrent mock).
    pub fn legacy() -> Self {
        ExecOptions {
            stack: Stack::Legacy,
            ..Default::default()
        }
    }
}

pub struct Exec {
    pub base: String,
    pub client: reqwest::Client,
    pub state: Arc<api::AppState>,
    pub dir: PathBuf,
    pub downloads: PathBuf,
    pub movies: PathBuf,
    pub tv: PathBuf,
    /// LEGACY stack: the qBittorrent mock (an empty, unused state on the managed stack).
    pub qb: qb::Qb,
    pub qb_addr: SocketAddr,
    /// Managed stack: the in-memory download client the application uses (the same state, kept
    /// across `restart`).
    pub fake: arr::client::fake::Handle,
    pub stack: Stack,
    pub mock: Mock,
    pub mock_addr: SocketAddr,
    pub _tmp: tempfile::TempDir,
}

pub const QB_USER_FILE: &str = "secrets/qb_user";

impl Exec {
    pub fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
    }

    pub fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
    }

    pub fn patch(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .patch(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
    }

    pub fn delete(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .delete(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
    }

    pub async fn json(&self, rb: reqwest::RequestBuilder) -> (u16, Value, String) {
        let r = rb.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
        )
    }

    /// One worker pass at `now`.
    pub async fn tick_at(&self, now: time::OffsetDateTime) -> arr::worker::TickReport {
        arr::worker::tick(&self.state, now).await.unwrap()
    }

    pub async fn tick(&self) -> arr::worker::TickReport {
        self.tick_at(time::OffsetDateTime::now_utc()).await
    }

    /// Simulates a process restart: a new AppState (and server) over the same state dir,
    /// with startup recovery.
    pub async fn restart(&mut self) -> Value {
        let cfg = config::load(&self.dir.join("config.toml")).unwrap();
        let state = Arc::new(api::AppState::new(cfg).unwrap());
        let recovery = arr::worker::recover(&state, time::OffsetDateTime::now_utc()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let s2 = state.clone();
        tokio::spawn(async move { axum::serve(listener, api::router(s2)).await.unwrap() });
        self.base = format!("http://{addr}");
        self.state = state;
        recovery
    }

    /// Builds a torrent over `files`, registers its data with the qBittorrent mock (written
    /// when the download "completes") and its `.torrent` with the Prowlarr mock. Returns the
    /// info-hash and a search-result JSON pointing at the mock download proxy.
    pub fn publish(
        &self,
        title: &str,
        name: &str,
        files: Vec<(String, Vec<u8>)>,
        claimed_gib: f64,
        magnet: bool,
    ) -> (String, Value) {
        let t = arr::fixture::build_torrent(name, &files);
        match self.stack {
            Stack::Managed => {
                self.fake
                    .lock()
                    .unwrap()
                    .fixtures
                    .insert(t.info_hash.clone(), (t.name.clone(), files));
            }
            Stack::Legacy => {
                self.qb
                    .lock()
                    .unwrap()
                    .fixtures
                    .insert(t.info_hash.clone(), (t.name.clone(), files));
            }
        }
        self.mock
            .lock()
            .unwrap()
            .torrents
            .insert(t.info_hash.clone(), t.torrent.clone());
        let mut r = release(title, claimed_gib, 50);
        r["guid"] = json!(format!("fixture-{}", t.info_hash));
        r["infoHash"] = json!(t.info_hash);
        r["downloadUrl"] = json!(format!(
            "http://{}/prowlarr/1/download?apikey={PROWLARR_KEY}&link={}",
            self.mock_addr, t.info_hash
        ));
        r["magnetUrl"] = if magnet { json!(t.magnet) } else { Value::Null };
        (t.info_hash, r)
    }

    pub fn search_results(&self, query: &str, releases: Vec<Value>) {
        self.mock
            .lock()
            .unwrap()
            .releases
            .insert(query.into(), releases);
    }

    pub fn grab_row(&self, id: &str) -> arr::downloads::Grab {
        let id = id.to_string();
        self.state
            .store
            .blocking(|c| arr::downloads::get(c, &id))
            .unwrap()
            .unwrap()
    }
}

async fn start_legacy(o: ExecOptions) -> Exec {
    let (mock_addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let dir = root.join("state");
    let downloads = root.join("downloads");
    let movies = root.join("library/movies");
    let tv = root.join("library/tv");
    for d in [&downloads, &movies, &tv] {
        std::fs::create_dir_all(d).unwrap();
    }
    let (qb_addr, qb) = qb::start(o.api, downloads.clone()).await;
    fsutil::ensure_private_dir(&dir).unwrap();
    fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
    fsutil::write_new_private_file(&dir.join(QB_USER_FILE), FAKE_USER.as_bytes()).unwrap();
    fsutil::write_new_private_file(&dir.join("secrets/qb_pass"), FAKE_PASSWORD.as_bytes()).unwrap();
    fsutil::write_new_private_file(&dir.join("secrets/plex_token"), PLEX_TOKEN.as_bytes()).unwrap();
    fsutil::write_new_private_file(&dir.join("secrets/tmdb"), TMDB_V3_KEY.as_bytes()).unwrap();
    let tmdb = if o.tmdb {
        format!(
            "[tmdb]\nenabled = true\nbase_url = \"http://{mock_addr}/tmdb\"\nimage_base_url = \"http://{mock_addr}\"\ncredential_file = \"secrets/tmdb\"\n"
        )
    } else {
        String::new()
    };
    let extra = format!(
        "[execution]\nenabled = true\ndownload_cleanup = {cleanup}\n\
         [qbittorrent]\nbase_url = \"http://{qb_addr}/\"\nusername_file = '{QB_USER_FILE}'\npassword_file = \"secrets/qb_pass\"\nsave_path = \"/downloads\"\nlocal_path = '{dl}'\ntimeout_secs = 5\n{cats}\
         [migration]\nmode = '{mode}'\n\
         [library]\nmovies = '{movies}'\ntv = '{tv}'\nsample_detection = {sample}\nprobe_timeout_secs = 30\n\
         [automation]\npoll_seconds = 3600\n{stall}\
         [plex]\nbase_url = \"http://{mock_addr}/plex\"\ntoken_file = \"secrets/plex_token\"\nmovie_section = 1\ntv_section = 2\n\
         discover_url = \"http://{mock_addr}/plexmeta\"\nmetadata_url = \"http://{mock_addr}/plexmeta\"\nimage_base_url = \"http://{mock_addr}\"\n{tmdb}{extra}",
        cleanup = o.cleanup,
        dl = downloads.display(),
        movies = movies.display(),
        tv = tv.display(),
        sample = o.sample_detection,
        extra = o.extra,
        tmdb = tmdb,
        mode = o.mode,
        stall = o
            .stall_timeout_minutes
            .map(|m| format!("stall_timeout_minutes = {m}\n"))
            .unwrap_or_default(),
        cats = o
            .categories
            .map(|(m, t)| format!("movie_category = '{m}'\ntv_category = '{t}'\n"))
            .unwrap_or_default(),
    );
    write_state(&dir, &o.snapshot, &extra, Some(mock_addr), o.tvmaze);
    let cfg = config::load(&dir.join("config.toml")).unwrap();
    let state = Arc::new(api::AppState::new(cfg).unwrap());
    arr::worker::recover(&state, time::OffsetDateTime::now_utc()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s2 = state.clone();
    tokio::spawn(async move { axum::serve(listener, api::router(s2)).await.unwrap() });
    Exec {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
        state,
        dir,
        downloads,
        movies,
        tv,
        qb,
        qb_addr,
        fake: arr::client::fake::handle("legacy-unused"),
        stack: Stack::Legacy,
        mock,
        mock_addr,
        _tmp: tmp,
    }
}

pub async fn start_exec(o: ExecOptions) -> Exec {
    match o.stack {
        Stack::Managed => start_managed(o).await,
        Stack::Legacy => start_legacy(o).await,
    }
}

// ------------------------------------------------------------------ managed stack

/// The managed setup document of the harness: the default HD profiles, the download
/// categories, and size limits that bound a release's size per minute of runtime the way the
/// legacy snapshot's quality definitions did (so claimed sizes in the suites stay valid).
fn setup_document(o: &ExecOptions) -> Value {
    let (movies, tv) = o.categories.unwrap_or(("um-movies", "um-tv"));
    let mut doc = format!(
        "[download]\nmovie_category = \"{movies}\"\ntv_category = \"{tv}\"\n{}\
         [tv]\n{}[movies]\n{}",
        o.download_extra, o.tv_extra, o.movies_extra
    );
    if o.profiles.is_empty() {
        // The legacy snapshot's "HD - 720p/1080p" profiles: no Bluray-1080p; movies want
        // English, unmarked releases are taken as the original language.
        let tiers = "tiers = [[\"hdtv_720p\"], [\"webdl_720p\", \"webrip_720p\"], [\"bluray_720p\"], [\"hdtv_1080p\"], [\"webdl_1080p\", \"webrip_1080p\"]]\n";
        doc.push_str(&format!(
            "[[profiles]]\nid = \"movies-hd\"\nmedia = \"movie\"\n{tiers}language = [\"english\"]\nunmarked = \"assume_original\"\n[[profiles]]\nid = \"tv-hd\"\nmedia = \"series\"\n{tiers}"
        ));
    }
    doc.push_str(&o.profiles);
    if o.indexer {
        doc.push_str("[[indexers]]\nid = 1\nseed_ratio = 2.0\nseed_time_minutes = -1\n");
    }
    for q in [
        "hdtv_720p",
        "webdl_720p",
        "webrip_720p",
        "bluray_720p",
        "hdtv_1080p",
        "webdl_1080p",
        "webrip_1080p",
    ] {
        doc.push_str(&format!(
            "[[size_limits]]\nmedia = \"movie\"\nquality = \"{q}\"\nmin_mib_per_minute = 0.0\nmax_mib_per_minute = 100.0\n"
        ));
    }
    doc.push_str(
        "[[size_limits]]\nmedia = \"movie\"\nquality = \"bluray_1080p\"\nmin_mib_per_minute = 0.0\n",
    );
    for (q, min, max) in [
        ("hdtv_720p", 3.0, 125.0),
        ("webdl_720p", 3.0, 130.0),
        ("webrip_720p", 3.0, 130.0),
        ("bluray_720p", 4.0, 130.0),
        ("hdtv_1080p", 4.0, 125.0),
        ("webdl_1080p", 4.0, 130.0),
        ("webrip_1080p", 4.0, 130.0),
        ("bluray_1080p", 4.0, 155.0),
    ] {
        doc.push_str(&format!(
            "[[size_limits]]\nmedia = \"series\"\nquality = \"{q}\"\nmin_mib_per_minute = {min:?}\nmax_mib_per_minute = {max:?}\n"
        ));
    }
    let table: toml::Table = toml::from_str(&doc).unwrap();
    serde_json::to_value(table).unwrap()
}

/// The harness config for the managed stack (the real default layout: native Torznab
/// indexer, embedded download engine replaced by the in-memory client, coexistence).
#[allow(clippy::too_many_arguments)]
fn managed_config(
    o: &ExecOptions,
    mock: SocketAddr,
    fake_id: &str,
    downloads: &Path,
    movies: &Path,
    tv: &Path,
) -> String {
    let tmdb = if o.tmdb {
        format!(
            "[tmdb]\nenabled = true\nbase_url = \"http://{mock}/tmdb\"\nimage_base_url = \"http://{mock}\"\ncredential_file = \"secrets/tmdb\"\n"
        )
    } else {
        String::new()
    };
    let tvmaze = if o.tvmaze {
        format!(
            "[tvmaze]\nenabled = true\nbase_url = \"http://{mock}/tvmaze\"\nimage_base_url = \"http://{mock}\"\ntimeout_secs = 2\n"
        )
    } else {
        String::new()
    };
    let plex = if o.plex {
        format!(
            "[plex]\nbase_url = \"http://{mock}/plex\"\ntoken_file = \"secrets/plex_token\"\nmovie_section = 1\ntv_section = 2\n\
             discover_url = \"http://{mock}/plexmeta\"\nmetadata_url = \"http://{mock}/plexmeta\"\nimage_base_url = \"http://{mock}\"\n"
        )
    } else {
        String::new()
    };
    let indexer = if o.indexer {
        format!(
            "[[indexer]]\nid = 1\nname = \"The Pirate Bay\"\nkind = \"torznab\"\nbase_url = \"http://{}/tz/\"\napi_key_file = \"secrets/tz_key\"\nmin_interval_ms = 0\ncache_ttl_secs = {}\ntimeout_secs = 2\npage_size = {}\nmax_pages = 2\n",
            o.indexer_addr.unwrap_or(mock),
            o.indexer_cache_secs,
            o.page_size
        )
    } else {
        String::new()
    };
    let execution = if o.execution {
        format!(
            "[execution]\nenabled = true\ndownload_cleanup = {cleanup}\n\
             [embedded]\ndownload_dir = '{dl}'\nlisten = \"127.0.0.1:0\"\ntrackers = false\ndht = true\nlocal_discovery = false\n\
             [test_client]\nid = '{fake_id}'\n",
            cleanup = o.cleanup,
            dl = downloads.display(),
        )
    } else {
        String::new()
    };
    format!(
        "[server]\napi_token_file = \"secrets/api_token\"\n{timeout}{server}\
         {indexer}{tvmaze}{tmdb}{execution}\
         [migration]\nmode = \"coexistence\"\n\
         [library]\nmovies = '{movies}'\ntv = '{tv}'\nsample_detection = {sample}\nprobe_timeout_secs = 30\n\
         [automation]\npoll_seconds = 3600\n{stall}\
         {plex}{extra}",
        timeout = if o.server_extra.contains("request_timeout_secs") {
            ""
        } else {
            "request_timeout_secs = 30\n"
        },
        server = o.server_extra,
        movies = movies.display(),
        tv = tv.display(),
        sample = o.sample_detection,
        extra = o.extra,
        stall = o
            .stall_timeout_minutes
            .map(|m| format!("stall_timeout_minutes = {m}\n"))
            .unwrap_or_default(),
    )
}

async fn start_managed(o: ExecOptions) -> Exec {
    let (mock_addr, mock) = start_mock().await;
    mock.lock().unwrap().slow_horses_s2 = true;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let dir = root.join("state");
    let downloads = root.join("downloads");
    let movies = root.join("library/movies");
    let tv = root.join("library/tv");
    for d in [&downloads, &movies, &tv] {
        std::fs::create_dir_all(d).unwrap();
    }
    fsutil::ensure_private_dir(&dir).unwrap();
    fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
    for (name, value) in [
        ("api_token", TOKEN),
        ("tz_key", TZ_KEY),
        ("plex_token", PLEX_TOKEN),
        ("tmdb", TMDB_V3_KEY),
    ] {
        fsutil::write_new_private_file(&dir.join("secrets").join(name), value.as_bytes()).unwrap();
    }
    let fake_id = format!("exec-{}", root.display());
    let cfg_text = managed_config(&o, mock_addr, &fake_id, &downloads, &movies, &tv);
    fsutil::write_new_private_file(&dir.join("config.toml"), cfg_text.as_bytes()).unwrap();
    let cfg = config::load(&dir.join("config.toml")).unwrap();
    let state = Arc::new(api::AppState::new(cfg).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s2 = state.clone();
    tokio::spawn(async move { axum::serve(listener, api::router(s2)).await.unwrap() });
    let (qb_addr, qb) = (
        SocketAddr::from(([127, 0, 0, 1], 0)),
        Arc::new(Mutex::new(qb::QbState::default())),
    );
    let e = Exec {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
        state,
        dir,
        downloads,
        movies,
        tv,
        qb,
        qb_addr,
        fake: arr::client::fake::handle(&fake_id),
        stack: Stack::Managed,
        mock,
        mock_addr,
        _tmp: tmp,
    };
    if !o.setup {
        return e;
    }
    let (s, v, text) = e
        .json(
            e.post("/api/v1/setup")
                .json(&json!({ "document": setup_document(&o) })),
        )
        .await;
    assert_eq!(s, 201, "managed setup: {text}");
    assert_eq!(v["status"], "created", "{v}");
    arr::worker::recover(&e.state, time::OffsetDateTime::now_utc()).unwrap();
    if o.library {
        e.seed_library(o.tvmaze).await;
    }
    e
}

/// A file written under `root` (parents created).
pub fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

impl Exec {
    /// Managed: the in-memory client completes the download of `hash` (writes its data).
    pub fn complete(&self, hash: &str) {
        self.fake.lock().unwrap().complete(hash);
    }

    /// Managed: the client reports a share ratio and seeding time, optionally stopped.
    pub fn seeded(&self, hash: &str, ratio: f64, seeding_secs: i64, stopped: bool) {
        self.fake
            .lock()
            .unwrap()
            .seeded(hash, ratio, seeding_secs, stopped);
    }

    /// Managed: forces the client's state and progress for `hash`.
    pub fn set_state(&self, hash: &str, state: &str, progress: f64) {
        self.fake.lock().unwrap().set_state(hash, state, progress);
    }

    /// Adds `media_id` through the API without searching (monitored, as a user would).
    pub async fn add(&self, media_id: &str) -> Value {
        add_title(&self.base, &self.client, media_id).await
    }

    /// Adds a movie from operator-supplied metadata (released long ago, no TMDB needed).
    pub async fn add_movie(
        &self,
        tmdb: u32,
        title: &str,
        year: u16,
        runtime: u32,
        language: &str,
        alternates: &[&str],
    ) {
        add_movie_to(
            &self.base,
            &self.client,
            tmdb,
            title,
            year,
            runtime,
            language,
            alternates,
        )
        .await
    }

    /// The managed counterpart of the legacy `snapshot()` (see `seed_standard_library`).
    pub async fn seed_library(&self, tvmaze: bool) {
        seed_standard_library(&self.base, &self.client, &self.movies, &self.tv, tvmaze).await;
        // Nothing the seeding asked is a test's business.
        let mut m = self.mock.lock().unwrap();
        m.log.clear();
        m.plex_meta_log.clear();
        m.schedule_dates.clear();
        m.image_log.clear();
    }
}

async fn post_ok(base: &str, client: &reqwest::Client, path: &str, body: Value) -> Value {
    let r = client
        .post(format!("{base}{path}"))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    assert!(
        status == 200 || status == 201,
        "POST {path}: {status} {text}"
    );
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// Adds `media_id` (no search) to the server at `base`.
pub async fn add_title(base: &str, client: &reqwest::Client, media_id: &str) -> Value {
    post_ok(
        base,
        client,
        "/api/v1/library",
        json!({ "media_id": media_id, "search": false }),
    )
    .await
}

/// Adds a movie from operator-supplied metadata (no search).
#[allow(clippy::too_many_arguments)]
pub async fn add_movie_to(
    base: &str,
    client: &reqwest::Client,
    tmdb: u32,
    title: &str,
    year: u16,
    runtime: u32,
    language: &str,
    alternates: &[&str],
) {
    post_ok(
        base,
        client,
        "/api/v1/library",
        json!({
            "media_id": format!("movie:tmdb:{tmdb}"), "search": false,
            "metadata": { "title": title, "year": year, "runtime_minutes": runtime,
                "original_language": language.to_lowercase(), "alternate_titles": alternates,
                "digital_release": format!("{year}-06-01") },
        }),
    )
    .await;
}

/// The managed setup of the harness (`POST /api/v1/setup`), for tests that start their own
/// server over `write_managed_config`.
pub async fn setup_managed(base: &str, client: &reqwest::Client) {
    let v = post_ok(
        base,
        client,
        "/api/v1/setup",
        json!({ "document": setup_document(&ExecOptions::default()) }),
    )
    .await;
    assert_eq!(v["status"], "created", "{v}");
}

/// The managed counterpart of the legacy `snapshot()`: The Matrix (603, missing), The Matrix
/// Reloaded (604, on disk), Amélie (194, French, missing), Slow Horses (TVDB 372264: S01E01
/// and S01E03 on disk) and Fargo (TVDB 269613: S04E01-E03 on disk), all added and monitored.
pub async fn seed_standard_library(
    base: &str,
    client: &reqwest::Client,
    movies: &Path,
    tv: &Path,
    tvmaze: bool,
) {
    put(
        movies,
        "The Matrix Reloaded (2003)/The.Matrix.Reloaded.2003.1080p.WEB-DL.x264-OLD.mkv",
        &mkv(1),
    );
    add_movie_to(base, client, 603, "The Matrix", 1999, 136, "English", &[]).await;
    add_movie_to(
        base,
        client,
        604,
        "The Matrix Reloaded",
        2003,
        138,
        "English",
        &[],
    )
    .await;
    add_movie_to(
        base,
        client,
        194,
        "Amélie",
        2001,
        122,
        "French",
        &[
            "Amelie from Montmartre",
            "Le Fabuleux Destin d'Amélie Poulain",
        ],
    )
    .await;
    if tvmaze {
        for n in [1, 3] {
            put(
                tv,
                &format!("Slow Horses/Season 1/Slow.Horses.S01E0{n}.1080p.WEB-DL.x264-OLD.mkv"),
                &mkv(1),
            );
        }
        for n in 1..=3 {
            put(
                tv,
                &format!("Fargo/Season 4/Fargo.S04E0{n}.1080p.WEB-DL.x264-OLD.mkv"),
                &mkv(1),
            );
        }
        add_title(base, client, "series:tvdb:372264").await;
        add_title(base, client, "series:tvdb:269613").await;
    }
}

/// The settings the managed setup of the harness produces (profiles, size limits, download and
/// indexer policy), for tests that evaluate decisions without a server.
pub fn managed_settings(o: &ExecOptions) -> arr::settings::Settings {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let (movies, tv) = (tmp.path().join("movies"), tmp.path().join("tv"));
    std::fs::create_dir_all(&movies).unwrap();
    std::fs::create_dir_all(&tv).unwrap();
    write_managed_config(
        &dir,
        SocketAddr::from(([127, 0, 0, 1], 9)),
        false,
        &format!(
            "[library]\nmovies = '{}'\ntv = '{}'\n[migration]\nmode = \"coexistence\"\n",
            movies.display(),
            tv.display()
        ),
    );
    let cfg = config::load(&dir.join("config.toml")).unwrap();
    let doc: arr::setup::SetupDoc =
        serde_json::from_value(setup_document(o)).unwrap();
    arr::setup::normalize(&doc, &cfg).unwrap().0
}

/// Config and secrets of the managed stack for tests that run their own server or engine:
/// `[server]`, the native Torznab `[[indexer]]` (id 1) on the mock, `[tvmaze]` when asked,
/// then `extra` (whole sections). The state is not set up (`setup_managed` does that).
pub fn write_managed_config(dir: &Path, mock: SocketAddr, tvmaze: bool, extra: &str) {
    fsutil::ensure_private_dir(dir).unwrap();
    fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
    for (name, value) in [("api_token", TOKEN), ("tz_key", TZ_KEY)] {
        let p = dir.join("secrets").join(name);
        if !p.exists() {
            fsutil::write_new_private_file(&p, value.as_bytes()).unwrap();
        }
    }
    let mut cfg = format!(
        "[server]\napi_token_file = \"secrets/api_token\"\nrequest_timeout_secs = 30\n\
         [[indexer]]\nid = 1\nname = \"The Pirate Bay\"\nkind = \"torznab\"\nbase_url = \"http://{mock}/tz/\"\napi_key_file = \"secrets/tz_key\"\nmin_interval_ms = 0\ncache_ttl_secs = 0\ntimeout_secs = 2\npage_size = 100\nmax_pages = 2\n"
    );
    if tvmaze {
        cfg.push_str(&format!(
            "[tvmaze]\nenabled = true\nbase_url = \"http://{mock}/tvmaze\"\nimage_base_url = \"http://{mock}\"\ntimeout_secs = 2\n"
        ));
    }
    cfg.push_str(extra);
    let cfg_path = dir.join("config.toml");
    let _ = std::fs::remove_file(&cfg_path);
    fsutil::write_new_private_file(&cfg_path, cfg.as_bytes()).unwrap();
}

/// ffprobe accepts the file (the library copy is real, readable media).
pub async fn playable(path: &Path) -> bool {
    let root = path.parent().unwrap();
    let canonical = arr::probe::validate(path, &[root]).unwrap();
    arr::probe::run(&ff("ffprobe"), &canonical, Duration::from_secs(30))
        .await
        .is_ok()
}

pub fn ino(path: &Path) -> u64 {
    arr::fsutil::file_id(path).unwrap().1
}
