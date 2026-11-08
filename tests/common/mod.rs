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

async fn tvmaze_show_by_id(axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    match shows()
        .into_iter()
        .find(|s| s["id"].as_u64().map(|n| n.to_string()).as_deref() == Some(id.as_str()))
    {
        Some(s) => axum::Json(s).into_response(),
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
    if id == "9005" {
        let mut eps = parity_partial_episodes();
        eps.extend(extra);
        return axum::Json(eps).into_response();
    }
    let seasons: &[(u32, u32)] = match id.as_str() {
        "9002" => &[(4, 11), (5, 10)],
        "9003" => &[(1, 8)],
        "9001" => &[(1, 6)],
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
        .route("/prowlarr/{id}/download", get(prowlarr_download))
        .route("/prowlarr/{id}/api", get(prowlarr_feed))
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
                "[tvmaze]\nenabled = true\nbase_url = \"http://{addr}/tvmaze\"\ntimeout_secs = 2\n"
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

pub struct ExecOptions {
    pub api: &'static str,
    pub snapshot: Value,
    pub sample_detection: bool,
    pub cleanup: bool,
    pub tvmaze: bool,
    /// Extra TOML appended (whole sections only).
    pub extra: String,
    /// Configure [tmdb] against the mock (v3 key).
    pub tmdb: bool,
    /// `[migration] mode` (`legacy` by default for the existing suites).
    pub mode: &'static str,
    /// Dedicated `[qbittorrent]` movie/TV categories (default: the imported native ones).
    pub categories: Option<(&'static str, &'static str)>,
    /// `[automation] stall_timeout_minutes` (default: the configuration's default).
    pub stall_timeout_minutes: Option<u64>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            api: QB_API_4_3_9,
            snapshot: snapshot(),
            sample_detection: false,
            cleanup: true,
            tvmaze: true,
            extra: String::new(),
            tmdb: false,
            mode: "legacy",
            categories: None,
            stall_timeout_minutes: None,
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
    pub qb: qb::Qb,
    pub qb_addr: SocketAddr,
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
        self.qb
            .lock()
            .unwrap()
            .fixtures
            .insert(t.info_hash.clone(), (t.name.clone(), files));
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

pub async fn start_exec(o: ExecOptions) -> Exec {
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
            "[tmdb]\nenabled = true\nbase_url = \"http://{mock_addr}/tmdb\"\ncredential_file = \"secrets/tmdb\"\n"
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
         [plex]\nbase_url = \"http://{mock_addr}/plex\"\ntoken_file = \"secrets/plex_token\"\nmovie_section = 1\ntv_section = 2\n{tmdb}{extra}",
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
        mock,
        mock_addr,
        _tmp: tmp,
    }
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
