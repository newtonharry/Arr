//! Optional Plex hook: refreshes the configured library section after an import
//! (`POST /library/sections/{id}/refresh`, `X-Plex-Token` header; `GET` when a server answers
//! 405), limited to the imported folder (`?path=`) when the library root's Plex path is
//! configured and the section already has items (Plex ignores folder scans in an empty
//! library, so the first title gets a whole-section scan). Bounded by the configured timeout; retried by the worker's command attempts. A
//! failed refresh never undoes an import. The token never reaches logs or responses.
//!
//! Setup discovery reads `GET /library/sections` (read-only, bounded) to list sections and
//! check the configured section ids and path mapping.

use crate::config::PlexConfig;
use crate::settings::MediaKind;
use crate::upstream::{self, UpstreamError};
use serde::Deserialize;

const MAX_SECTIONS_BYTES: usize = 1024 * 1024;

/// A Plex library section as discovery reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub key: u32,
    pub title: String,
    /// `movie`, `show`, `artist`, `photo`.
    pub kind: String,
    pub locations: Vec<String>,
}

#[derive(Deserialize)]
struct SectionsDoc {
    #[serde(rename = "MediaContainer")]
    container: Container,
}

#[derive(Deserialize)]
struct Container {
    #[serde(rename = "Directory", default)]
    directories: Vec<Directory>,
}

#[derive(Deserialize)]
struct Directory {
    #[serde(default)]
    key: serde_json::Value,
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(rename = "Location", default)]
    locations: Vec<Location>,
}

#[derive(Deserialize)]
struct Location {
    #[serde(default)]
    path: Option<String>,
}

/// Largest page of library items read at once.
pub const MAX_PAGE: u32 = 200;
const MAX_ITEMS_BYTES: usize = 32 * 1024 * 1024;

/// One library item (movie, show or episode) as enumeration reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Item {
    pub rating_key: String,
    pub kind: String,
    pub title: String,
    pub year: Option<u16>,
    pub duration_ms: Option<u64>,
    pub originally_available: Option<String>,
    /// Provider ids: `tmdb://603`, `imdb://tt0133093`, `tvdb://372264` (and the legacy
    /// agent `guid`, e.g. `com.plexapp.agents.themoviedb://603?lang=en`).
    pub guids: Vec<String>,
    /// Episodes: season and episode numbers.
    pub parent_index: Option<u32>,
    pub index: Option<u32>,
    /// Media part files as Plex sees them, with their sizes.
    pub parts: Vec<(String, Option<u64>)>,
}

impl Item {
    /// The id for `provider` (`tmdb`, `imdb`, `tvdb`), from the new or legacy agents.
    pub fn provider_id(&self, provider: &str) -> Option<String> {
        let legacy = match provider {
            "tmdb" => "com.plexapp.agents.themoviedb://",
            "imdb" => "com.plexapp.agents.imdb://",
            "tvdb" => "com.plexapp.agents.thetvdb://",
            _ => return None,
        };
        let new = format!("{provider}://");
        self.guids.iter().find_map(|g| {
            let rest = g.strip_prefix(&new).or_else(|| g.strip_prefix(legacy))?;
            let id: String = rest
                .split(['?', '/'])
                .next()
                .unwrap_or("")
                .chars()
                .take(20)
                .collect();
            (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).then_some(id)
        })
    }
}

/// A page of items with the total the server reports (when it does).
#[derive(Debug, Default)]
pub struct Page {
    pub items: Vec<Item>,
    pub total: Option<u64>,
}

#[derive(Deserialize)]
struct ItemsDoc {
    #[serde(rename = "MediaContainer")]
    container: ItemsContainer,
}

#[derive(Deserialize)]
struct ItemsContainer {
    #[serde(rename = "totalSize", default)]
    total_size: Option<u64>,
    #[serde(rename = "Metadata", default)]
    metadata: Vec<RawItem>,
}

#[derive(Deserialize)]
struct RawItem {
    #[serde(rename = "ratingKey", default)]
    rating_key: serde_json::Value,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    year: Option<serde_json::Value>,
    #[serde(default)]
    duration: Option<serde_json::Value>,
    #[serde(rename = "originallyAvailableAt", default)]
    originally_available: Option<String>,
    #[serde(default)]
    guid: Option<String>,
    #[serde(rename = "Guid", default)]
    guids: Vec<RawGuid>,
    #[serde(rename = "parentIndex", default)]
    parent_index: Option<serde_json::Value>,
    #[serde(default)]
    index: Option<serde_json::Value>,
    #[serde(rename = "Media", default)]
    media: Vec<RawMedia>,
}

#[derive(Deserialize)]
struct RawGuid {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
struct RawMedia {
    #[serde(rename = "Part", default)]
    parts: Vec<RawPart>,
}

#[derive(Deserialize)]
struct RawPart {
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    size: Option<serde_json::Value>,
}

fn as_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Parses an untrusted library items answer (bounded counts and lengths; never echoes it).
pub fn parse_items(body: &[u8]) -> Result<Page, UpstreamError> {
    let doc: ItemsDoc = serde_json::from_slice(body).map_err(|e| UpstreamError::BadResponse {
        reason: format!(
            "invalid library JSON ({:?} error at line {} column {})",
            e.classify(),
            e.line(),
            e.column()
        ),
    })?;
    let clip = |s: &str, n: usize| {
        s.chars()
            .filter(|c| !c.is_control())
            .take(n)
            .collect::<String>()
    };
    let items = doc
        .container
        .metadata
        .into_iter()
        .take(MAX_PAGE as usize * 25)
        .filter_map(|r| {
            let rating_key = match &r.rating_key {
                serde_json::Value::String(s) => s.trim().to_string(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => return None,
            };
            if rating_key.is_empty()
                || rating_key.len() > 20
                || !rating_key.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            let mut guids: Vec<String> = r
                .guids
                .iter()
                .filter_map(|g| g.id.as_deref().map(|g| clip(g, 200)))
                .take(20)
                .collect();
            if let Some(g) = &r.guid {
                guids.push(clip(g, 200));
            }
            Some(Item {
                rating_key,
                kind: clip(r.kind.as_deref().unwrap_or(""), 20),
                title: clip(r.title.as_deref().unwrap_or(""), 300),
                year: r
                    .year
                    .as_ref()
                    .and_then(as_u64)
                    .and_then(|y| u16::try_from(y).ok()),
                duration_ms: r.duration.as_ref().and_then(as_u64),
                originally_available: r
                    .originally_available
                    .as_deref()
                    .filter(|d| d.len() == 10)
                    .map(|d| clip(d, 10)),
                guids,
                parent_index: r
                    .parent_index
                    .as_ref()
                    .and_then(as_u64)
                    .and_then(|v| u32::try_from(v).ok()),
                index: r
                    .index
                    .as_ref()
                    .and_then(as_u64)
                    .and_then(|v| u32::try_from(v).ok()),
                parts: r
                    .media
                    .iter()
                    .take(20)
                    .flat_map(|m| m.parts.iter().take(20))
                    .filter_map(|p| {
                        let f = p.file.as_deref()?;
                        (f.len() <= 4096 && !f.chars().any(char::is_control))
                            .then(|| (f.to_string(), p.size.as_ref().and_then(as_u64)))
                    })
                    .collect(),
            })
        })
        .collect();
    Ok(Page {
        items,
        total: doc.container.total_size,
    })
}

/// Parses an untrusted `GET /library/sections` JSON answer (bounded counts and lengths).
pub fn parse_sections(body: &[u8]) -> Result<Vec<Section>, UpstreamError> {
    let doc: SectionsDoc =
        serde_json::from_slice(body).map_err(|e| UpstreamError::BadResponse {
            reason: format!(
                "invalid sections JSON ({:?} error at line {} column {})",
                e.classify(),
                e.line(),
                e.column()
            ),
        })?;
    let clip = |s: &str| {
        s.chars()
            .filter(|c| !c.is_control())
            .take(300)
            .collect::<String>()
    };
    Ok(doc
        .container
        .directories
        .iter()
        .take(200)
        .filter_map(|d| {
            let key = match &d.key {
                serde_json::Value::String(s) => s.trim().parse().ok(),
                serde_json::Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
                _ => None,
            }?;
            Some(Section {
                key,
                title: clip(d.title.as_deref().unwrap_or("")),
                kind: clip(d.kind.as_deref().unwrap_or("")),
                locations: d
                    .locations
                    .iter()
                    .take(50)
                    .filter_map(|l| l.path.as_deref().map(clip))
                    .collect(),
            })
        })
        .collect())
}

pub struct Plex {
    cfg: PlexConfig,
    http: reqwest::Client,
}

impl Plex {
    pub fn new(cfg: PlexConfig) -> Self {
        Plex {
            http: upstream::http_client(cfg.timeout, reqwest::redirect::Policy::none()),
            cfg,
        }
    }

    pub fn section_for(&self, media: MediaKind) -> Option<u32> {
        match media {
            MediaKind::Movie => self.cfg.movie_section,
            MediaKind::Series => self.cfg.tv_section,
        }
    }

    pub fn config(&self) -> &PlexConfig {
        &self.cfg
    }

    /// The Plex-side path of a folder below a library root (`None` without a mapping).
    pub fn plex_path(&self, media: MediaKind, folder: &str) -> Option<String> {
        let root = match media {
            MediaKind::Movie => self.cfg.movies_path.as_deref(),
            MediaKind::Series => self.cfg.tv_path.as_deref(),
        }?;
        let folder = crate::paths::safe_rel(folder)?;
        Some(format!(
            "{}/{}",
            root.trim_end_matches('/'),
            folder.to_string_lossy()
        ))
    }

    /// The server's library sections (read-only).
    pub async fn sections(&self) -> Result<Vec<Section>, UpstreamError> {
        let url =
            self.cfg
                .base_url
                .join("library/sections")
                .map_err(|_| UpstreamError::BadResponse {
                    reason: "invalid base URL".into(),
                })?;
        let resp = upstream::send(
            self.http
                .get(url)
                .header("X-Plex-Token", self.cfg.token.expose())
                .header("Accept", "application/json"),
        )
        .await?;
        parse_sections(&upstream::read_bytes(resp, MAX_SECTIONS_BYTES).await?)
    }

    async fn page(
        &self,
        path: &str,
        extra: &[(&str, &str)],
        start: u64,
        size: u32,
    ) -> Result<Page, UpstreamError> {
        let mut url = self
            .cfg
            .base_url
            .join(path)
            .map_err(|_| UpstreamError::BadResponse {
                reason: "invalid base URL".into(),
            })?;
        {
            let mut q = url.query_pairs_mut();
            for (k, v) in extra {
                q.append_pair(k, v);
            }
            q.append_pair("X-Plex-Container-Start", &start.to_string());
            q.append_pair(
                "X-Plex-Container-Size",
                &size.clamp(1, MAX_PAGE).to_string(),
            );
        }
        let resp = upstream::send(
            self.http
                .get(url)
                .header("X-Plex-Token", self.cfg.token.expose())
                .header("Accept", "application/json"),
        )
        .await?;
        parse_items(&upstream::read_bytes(resp, MAX_ITEMS_BYTES).await?)
    }

    /// One page of a section's top-level items (movies or shows), with provider ids.
    pub async fn section_items(
        &self,
        section: u32,
        start: u64,
        size: u32,
    ) -> Result<Page, UpstreamError> {
        self.page(
            &format!("library/sections/{section}/all"),
            &[("includeGuids", "1")],
            start,
            size,
        )
        .await
    }

    /// One page of a show's episodes (`allLeaves`).
    pub async fn leaves(
        &self,
        rating_key: &str,
        start: u64,
        size: u32,
    ) -> Result<Page, UpstreamError> {
        if rating_key.is_empty() || !rating_key.bytes().all(|b| b.is_ascii_digit()) {
            return Err(UpstreamError::BadResponse {
                reason: "invalid rating key".into(),
            });
        }
        self.page(
            &format!("library/metadata/{rating_key}/allLeaves"),
            &[],
            start,
            size,
        )
        .await
    }

    /// Scans what an import added: only `path` (a Plex-side folder) when given, unless the
    /// section is empty. Plex ignores a folder scan in an empty library (it logs "Refreshing 0
    /// IDs" and scans nothing), so the first title in a library gets a whole-section scan: the
    /// only scan Plex will run there, even when the folders already hold many files Plex never
    /// scanned. When the check itself fails, the folder scan goes ahead as before.
    /// Returns whether the scan was limited to the folder.
    pub async fn refresh_import(
        &self,
        section: u32,
        path: Option<&str>,
    ) -> Result<bool, UpstreamError> {
        let path = match path {
            Some(p) if !self.section_is_empty(section).await => Some(p),
            _ => None,
        };
        self.refresh(section, path).await?;
        Ok(path.is_some())
    }

    /// True only when Plex says the section has no items.
    async fn section_is_empty(&self, section: u32) -> bool {
        match self.section_items(section, 0, 1).await {
            Ok(page) => page.total.map_or(page.items.is_empty(), |t| t == 0),
            Err(_) => false,
        }
    }

    /// Refreshes a section, only `path` (a Plex-side folder) when given.
    pub async fn refresh(&self, section: u32, path: Option<&str>) -> Result<(), UpstreamError> {
        let mut url = self
            .cfg
            .base_url
            .join(&format!("library/sections/{section}/refresh"))
            .map_err(|_| UpstreamError::BadResponse {
                reason: "invalid base URL".into(),
            })?;
        if let Some(p) = path {
            url.query_pairs_mut().append_pair("path", p);
        }
        let send = |post: bool| {
            let req = if post {
                self.http.post(url.clone())
            } else {
                self.http.get(url.clone())
            };
            upstream::send(
                req.header("X-Plex-Token", self.cfg.token.expose())
                    .header("Accept", "application/json"),
            )
        };
        let mut resp = send(true).await?;
        if resp.status().as_u16() == 405 {
            resp = send(false).await?;
        }
        match upstream::classify_status(resp.status()) {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use url::Url;

    /// A first import into an empty library scans the whole section (Plex ignores a folder
    /// scan there); otherwise only the folder, including when the emptiness check fails. The
    /// check asks for one item with the token; a server that answers POST with 405 gets GET.
    #[tokio::test]
    async fn import_refresh_scans_the_whole_section_while_it_is_empty() {
        use axum::extract::{Path, RawQuery, State};
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        type Log = Arc<Mutex<Vec<String>>>;
        fn token(h: &HeaderMap) -> bool {
            h.get("x-plex-token").and_then(|v| v.to_str().ok()) == Some("t")
        }
        async fn all(
            State(log): State<Log>,
            h: HeaderMap,
            Path(id): Path<u32>,
            RawQuery(q): RawQuery,
        ) -> axum::response::Response {
            let q = q.unwrap_or_default();
            log.lock().unwrap().push(format!("all {id}?{q}"));
            if !token(&h) {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            let one = serde_json::json!([{ "ratingKey": "1", "type": "movie", "title": "A" }]);
            let doc = match id {
                1 => serde_json::json!({ "size": 0, "totalSize": 0, "offset": 0 }),
                2 => serde_json::json!({ "size": 1, "totalSize": 3, "offset": 0, "Metadata": one }),
                // No totalSize: the page itself decides.
                4 => serde_json::json!({ "size": 0, "offset": 0 }),
                5 | 6 => serde_json::json!({ "size": 1, "offset": 0, "Metadata": one }),
                _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            };
            axum::Json(serde_json::json!({ "MediaContainer": doc })).into_response()
        }
        async fn refresh(
            State(log): State<Log>,
            method: axum::http::Method,
            h: HeaderMap,
            Path(id): Path<u32>,
            RawQuery(q): RawQuery,
        ) -> StatusCode {
            if !token(&h) {
                return StatusCode::UNAUTHORIZED;
            }
            // Section 6 is a server that only takes GET.
            if id == 6 && method == axum::http::Method::POST {
                return StatusCode::METHOD_NOT_ALLOWED;
            }
            log.lock()
                .unwrap()
                .push(format!("{method} {id}?{}", q.unwrap_or_default()));
            StatusCode::OK
        }
        let log: Log = Arc::default();
        let app = axum::Router::new()
            .route("/library/sections/{id}/all", axum::routing::get(all))
            .route(
                "/library/sections/{id}/refresh",
                axum::routing::post(refresh).get(refresh),
            )
            .with_state(log.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let plex = Plex::new(PlexConfig {
            base_url: base.clone(),
            token: crate::config::Secret::new("t".into()),
            movie_section: Some(1),
            tv_section: Some(2),
            movies_path: Some("/m".into()),
            tv_path: Some("/t".into()),
            timeout: std::time::Duration::from_secs(5),
            discover_url: base.clone(),
            metadata_url: base.clone(),
            image_base_url: base,
        });
        let scans = |log: &Log| -> Vec<String> {
            let mut l = log.lock().unwrap();
            let out = l
                .iter()
                .filter(|e| !e.starts_with("all "))
                .cloned()
                .collect();
            l.clear();
            out
        };
        // Empty (totalSize 0, or no total and no items): the whole section.
        assert!(!plex.refresh_import(1, Some("/m/A (2000)")).await.unwrap());
        assert!(!plex.refresh_import(4, Some("/m/D")).await.unwrap());
        assert_eq!(scans(&log), ["POST 1?", "POST 4?"]);
        // Has items (with or without a total), or the check failed: only the folder.
        assert!(plex.refresh_import(2, Some("/t/B")).await.unwrap());
        assert!(plex.refresh_import(5, Some("/t/E")).await.unwrap());
        assert!(plex.refresh_import(3, Some("/m/C")).await.unwrap());
        assert_eq!(
            scans(&log),
            [
                "POST 2?path=%2Ft%2FB",
                "POST 5?path=%2Ft%2FE",
                "POST 3?path=%2Fm%2FC"
            ]
        );
        // POST answered 405: GET, still only the folder.
        assert!(plex.refresh_import(6, Some("/m/F")).await.unwrap());
        assert_eq!(scans(&log), ["GET 6?path=%2Fm%2FF"]);
        // No folder: the whole section, without asking whether it is empty.
        assert!(!plex.refresh_import(2, None).await.unwrap());
        assert_eq!(*log.lock().unwrap(), ["POST 2?"]);
        // The emptiness check asked for one item with the token.
        log.lock().unwrap().clear();
        plex.refresh_import(1, Some("/m/A")).await.unwrap();
        let first = log.lock().unwrap()[0].clone();
        assert!(
            first.starts_with("all 1?")
                && first.contains("X-Plex-Container-Start=0")
                && first.contains("X-Plex-Container-Size=1"),
            "{first}"
        );
    }

    #[test]
    fn items_and_provider_ids() {
        let body = br#"{"MediaContainer":{"size":2,"totalSize":81,"offset":0,"Metadata":[
          {"ratingKey":"101","type":"movie","title":"The Matrix","year":1999,"duration":8160000,"originallyAvailableAt":"1999-03-31",
           "Guid":[{"id":"imdb://tt0133093"},{"id":"tmdb://603"},{"id":"tvdb://169"}],
           "Media":[{"Part":[{"file":"/media_library/movies/The Matrix (1999)/The Matrix (1999).mkv","size":123456}]}]},
          {"ratingKey":102,"type":"movie","title":"Old Agent","year":"2001","guid":"com.plexapp.agents.imdb://tt0000001?lang=en",
           "Media":[{"Part":[{"file":"/media_library/movies/Old Agent (2001)/a.mkv"}]},{"Part":[{"file":"/media_library/movies/Old Agent (2001)/b.mkv","size":"5"}]}]},
          {"ratingKey":"x","title":"bad key"}]}}"#;
        let p = parse_items(body).unwrap();
        assert_eq!(p.total, Some(81));
        assert_eq!(p.items.len(), 2);
        let m = &p.items[0];
        assert_eq!(m.provider_id("tmdb").as_deref(), Some("603"));
        assert_eq!(m.provider_id("imdb").as_deref(), Some("tt0133093"));
        assert_eq!(m.duration_ms, Some(8_160_000));
        assert_eq!(
            m.parts,
            [(
                "/media_library/movies/The Matrix (1999)/The Matrix (1999).mkv".to_string(),
                Some(123_456)
            )]
        );
        let o = &p.items[1];
        assert_eq!(
            o.provider_id("tmdb"),
            None,
            "IMDB-only stays unresolved here"
        );
        assert_eq!(o.provider_id("imdb").as_deref(), Some("tt0000001"));
        assert_eq!((o.year, o.parts.len()), (Some(2001), 2));
        let ep = br#"{"MediaContainer":{"Metadata":[{"ratingKey":"7","type":"episode","parentIndex":1,"index":2,"title":"E2","Media":[{"Part":[{"file":"/tv/Show/Season 1/Show S01E02.mkv"}]}]}]}}"#;
        let e = &parse_items(ep).unwrap().items[0];
        assert_eq!((e.parent_index, e.index), (Some(1), Some(2)));
        assert!(parse_items(b"[]").is_err());
    }

    #[test]
    fn sections_are_parsed_and_bounded() {
        let body = br#"{"MediaContainer":{"size":3,"Directory":[
            {"key":"1","title":"Movies","type":"movie","Location":[{"id":1,"path":"/data/movies"}]},
            {"key":2,"title":"TV Shows","type":"show","Location":[{"id":2,"path":"/data/tv"},{"id":3,"path":"/more/tv"}]},
            {"key":"x","title":"Broken","type":"movie"}]}}"#;
        let s = parse_sections(body).unwrap();
        assert_eq!(s.len(), 2, "a section without a numeric key is skipped");
        assert_eq!(
            s[0],
            Section {
                key: 1,
                title: "Movies".into(),
                kind: "movie".into(),
                locations: vec!["/data/movies".into()]
            }
        );
        assert_eq!(s[1].locations, ["/data/tv", "/more/tv"]);
        assert!(
            parse_sections(b"<MediaContainer/>").is_err(),
            "XML is refused, not guessed"
        );
        let err = parse_sections(b"{\"token\":\"SECRET\"").unwrap_err();
        assert!(!err.describe().contains("SECRET"));
    }
}
