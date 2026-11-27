//! Posters for the web app: fetched from Plex, TMDB or TVmaze by the server, kept in a private
//! folder, and served from `/app/art/{media_id}`. Browsers never contact those sites.

use crate::api::AppState;
use crate::config::PlexConfig;
use crate::fsutil;
use crate::ids::{MediaId, SeriesKey};
use crate::upstream;
use anyhow::Result;
use reqwest::Url;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio::sync::Semaphore;

const MAX_POSTER_BYTES: usize = 2 * 1024 * 1024;
/// Target poster dimensions when asking the local Plex server to resize full-resolution artwork
/// (matches TMDB's `w342` 2:3 size: 3x sharp for the app's largest 116px card).
const POSTER_WIDTH: &str = "342";
const POSTER_HEIGHT: &str = "513";
/// Cached movie posters larger than this are likely full-resolution originals; try once per run
/// to replace them with a resized copy.
const SHRINK_ABOVE_BYTES: usize = 128 * 1024;
/// A poster is fetched again after this long (TMDB's terms limit how long its data is kept).
const POSTER_TTL: Duration = Duration::from_secs(30 * 86_400);
/// A source with no poster is asked again after this long.
const NONE_TTL: Duration = Duration::from_secs(86_400);
const MAX_FILES: usize = 2000;
const TRIM_FILES: usize = 500;
const MAX_IN_FLIGHT: usize = 4;

/// A poster and its content type.
pub type Poster = (Vec<u8>, &'static str);

pub struct Art {
    dir: PathBuf,
    http: upstream::Http,
    permits: Semaphore,
    shrink_tried: Mutex<HashSet<String>>,
}

/// Where a poster comes from.
enum Source {
    Movie(u32),
    /// A TVmaze show id.
    Show(u32),
}

impl Source {
    fn key(&self) -> String {
        match self {
            Source::Movie(id) => format!("movie-{id}"),
            Source::Show(id) => format!("show-{id}"),
        }
    }
}

/// Validated upstream image candidates for one title.
enum Candidates {
    /// A movie: validated Plex poster URL (usually full-res) and/or TMDB `w342` URL.
    Movie {
        plex: Option<Url>,
        tmdb: Option<Url>,
    },
    /// A TVmaze show poster (`medium` size, already compact).
    Show(Option<Url>),
}

/// The image type of `bytes` when it is one of the three allowed ones.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn fresh(path: &Path, ttl: Duration) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < ttl)
}

/// Local Plex Media Server photo transcoder URL for an already-validated poster address.
fn plex_transcode_url(plex: &PlexConfig, source: &Url) -> Option<Url> {
    let base = format!(
        "{}/photo/:/transcode",
        plex.base_url.as_str().trim_end_matches('/')
    );
    let mut url = Url::parse(&base).ok()?;
    url.query_pairs_mut()
        .append_pair("width", POSTER_WIDTH)
        .append_pair("height", POSTER_HEIGHT)
        .append_pair("minSize", "1")
        .append_pair("upscale", "0")
        .append_pair("format", "jpg")
        .append_pair("url", source.as_str());
    Some(url)
}

impl Art {
    pub fn new(state_dir: &Path) -> Result<Self> {
        let dir = state_dir.join("art");
        fsutil::ensure_private_dir(&dir)?;
        Ok(Art {
            dir,
            http: upstream::http_client(Duration::from_secs(15)),
            permits: Semaphore::new(MAX_IN_FLIGHT),
            shrink_tried: Mutex::new(HashSet::new()),
        })
    }

    /// The poster of a media id, if there is one.
    pub async fn poster(&self, st: &AppState, id: &MediaId) -> Option<Poster> {
        // Every upstream call (the TVDB lookup included) runs under one permit, taken when needed.
        let mut permit = None;
        let source = match id {
            MediaId::Movie { tmdb } => {
                st.movies.as_ref()?;
                Source::Movie(*tmdb)
            }
            // (a sport event has no poster here)
            MediaId::Event(_) => return None,
            MediaId::Series(_) | MediaId::Season(..) | MediaId::Episode(..) => {
                let tv = st.tvmaze.as_ref()?;
                Source::Show(match id.series_key()? {
                    SeriesKey::Tvmaze(id) => id,
                    SeriesKey::Tvdb(tvdb) => {
                        let known = st
                            .index()
                            .series(SeriesKey::Tvdb(tvdb))
                            .and_then(|s| s.tvmaze_id);
                        match known {
                            Some(id) => id,
                            None => {
                                permit = Some(self.permits.acquire().await.ok()?);
                                tv.by_tvdb(tvdb).await.ok()??.id
                            }
                        }
                    }
                })
            }
        };
        let key = source.key();
        if let Some(hit) = self.cached(&key) {
            // If an older cache entry holds a full-resolution movie poster, try once to replace it
            // with a compact resized version so existing installs benefit without waiting 30 days.
            if let Some((bytes, kind)) = hit {
                if matches!(source, Source::Movie(_))
                    && bytes.len() > SHRINK_ABOVE_BYTES
                    && self.claim_shrink(&key)
                {
                    if permit.is_none() {
                        permit = self.permits.acquire().await.ok();
                    }
                    let _permit = permit;
                    if let Ok(Candidates::Movie { plex, tmdb }) = self.candidates(st, &source).await
                        && let Some((smaller, skind)) = self
                            .fetch_compact_movie(st, plex.as_ref(), tmdb.as_ref())
                            .await
                        && smaller.len() < bytes.len()
                    {
                        self.remember(&key, Some(&smaller));
                        return Some((smaller, skind));
                    }
                }
                return Some((bytes, kind));
            }
            return None;
        }
        if permit.is_none() {
            permit = Some(self.permits.acquire().await.ok()?);
        }
        let _permit = permit;
        // Another request may have fetched it while this one waited.
        if let Some(hit) = self.cached(&key) {
            return hit;
        }
        let poster = match self.candidates(st, &source).await {
            Ok(Candidates::Movie {
                plex: None,
                tmdb: None,
            })
            | Ok(Candidates::Show(None)) => {
                self.remember(&key, None);
                return None;
            }
            Ok(Candidates::Movie { plex, tmdb }) => {
                if let Some(p) = self
                    .fetch_compact_movie(st, plex.as_ref(), tmdb.as_ref())
                    .await
                {
                    Some(p)
                } else if let Some(u) = plex {
                    self.fetch_url(u).await
                } else {
                    None
                }
            }
            Ok(Candidates::Show(Some(u))) => self.fetch_url(u).await,
            Err(()) => return None,
        }?;
        self.remember(&key, Some(&poster.0));
        Some(poster)
    }

    fn claim_shrink(&self, key: &str) -> bool {
        self.shrink_tried
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.to_string())
    }

    /// Tries to fetch a compact (~342px) movie poster: first through the local Plex server's photo
    /// transcoder (when Plex is configured), then from TMDB's `w342` endpoint.
    async fn fetch_compact_movie(
        &self,
        st: &AppState,
        plex: Option<&Url>,
        tmdb: Option<&Url>,
    ) -> Option<Poster> {
        if let (Some(src), Some(cfg)) = (plex, st.cfg.plex.as_ref())
            && let Some(transcode) = plex_transcode_url(cfg, src)
            && let Some(p) = self
                .fetch_request(
                    self.http
                        .get(transcode)
                        .header("X-Plex-Token", cfg.token.expose()),
                )
                .await
        {
            return Some(p);
        }
        if let Some(u) = tmdb {
            return self.fetch_url(u.clone()).await;
        }
        None
    }

    async fn fetch_url(&self, url: Url) -> Option<Poster> {
        self.fetch_request(self.http.get(url)).await
    }

    async fn fetch_request(&self, req: reqwest::RequestBuilder) -> Option<Poster> {
        let resp = upstream::send(req).await.ok()?;
        let allowed = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|t| t.split(';').next())
            .map(str::trim)
            .is_some_and(|t| matches!(t, "image/jpeg" | "image/png" | "image/webp"));
        if !allowed {
            return None;
        }
        let bytes = upstream::read_bytes(resp, MAX_POSTER_BYTES).await.ok()?;
        let kind = sniff(&bytes)?;
        Some((bytes, kind))
    }

    /// `Some(None)` when a recent answer says there is no poster.
    fn cached(&self, key: &str) -> Option<Option<Poster>> {
        let file = self.dir.join(key);
        if fresh(&file, POSTER_TTL)
            && let Ok(bytes) = fsutil::read_bounded(&file, MAX_POSTER_BYTES as u64)
            && let Some(kind) = sniff(&bytes)
        {
            return Some(Some((bytes, kind)));
        }
        fresh(&self.dir.join(format!("{key}.none")), NONE_TTL).then_some(None)
    }

    /// Keeps a poster, or a marker that there is none; trims the folder when it grows.
    fn remember(&self, key: &str, poster: Option<&[u8]>) {
        let marker = self.dir.join(format!("{key}.none"));
        let _ = match poster {
            Some(bytes) => {
                let _ = fs::remove_file(&marker);
                fsutil::atomic_replace_private(&self.dir.join(key), bytes)
            }
            None => fsutil::atomic_replace_private(&marker, b""),
        };
        let mut files: Vec<_> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        if files.len() > MAX_FILES {
            files.sort();
            for (_, path) in files.into_iter().take(TRIM_FILES) {
                let _ = fs::remove_file(path);
            }
        }
    }

    /// The poster addresses, on the configured image hosts only. `Err` when the provider could
    /// not be asked (nothing is remembered); `Ok(Candidates::... { None })` when it has no usable poster.
    async fn candidates(&self, st: &AppState, source: &Source) -> Result<Candidates, ()> {
        match source {
            Source::Movie(id) => {
                let movies = st.movies.as_ref().ok_or(())?;
                let movie = movies.movie(*id).await.map_err(|_| ())?;
                let plex = movie
                    .as_ref()
                    .and_then(|m| m.plex_thumb.as_deref())
                    .and_then(|thumb| Url::parse(thumb).ok())
                    .and_then(|given| {
                        let base = &st.cfg.plex.as_ref()?.image_base_url;
                        (given.host_str() == base.host_str()).then_some(())?;
                        on_base(base, given.path().trim_start_matches('/'))
                    });
                let path = movie.and_then(|m| m.poster_path).filter(|p| {
                    p.starts_with('/')
                        && p.len() < 100
                        && p[1..]
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                });
                let tmdb = path.and_then(|p| {
                    let base = &st.cfg.tmdb.as_ref()?.image_base_url;
                    on_base(base, &format!("t/p/w342{p}"))
                });
                Ok(Candidates::Movie { plex, tmdb })
            }
            Source::Show(id) => {
                let tv = st.tvmaze.as_ref().ok_or(())?;
                let show = tv.by_id(*id).await.map_err(|_| ())?;
                let given = show
                    .and_then(|s| s.image?.medium)
                    .and_then(|u| Url::parse(&u).ok());
                let url = given.and_then(|u| {
                    let base = &st.cfg.tvmaze.as_ref()?.image_base_url;
                    // Only the path is used: the address is always the configured host's.
                    (u.host_str() == base.host_str()).then_some(())?;
                    on_base(base, u.path().trim_start_matches('/'))
                });
                Ok(Candidates::Show(url))
            }
        }
    }
}

/// `rel` joined onto `base`, only if the result stays on `base`'s origin (a path such as
/// `http://elsewhere/x` would otherwise replace the host).
fn on_base(base: &Url, rel: &str) -> Option<Url> {
    let u = base.join(rel).ok()?;
    (u.origin() == base.origin()).then_some(u)
}
