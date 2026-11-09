//! Posters for the web app: fetched from TMDB or TVmaze by the server, kept in a private
//! folder, and served from `/app/art/{media_id}`. Browsers never contact those sites.

use crate::api::AppState;
use crate::fsutil;
use crate::ids::{MediaId, SeriesKey};
use crate::upstream;
use anyhow::Result;
use reqwest::Url;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::Semaphore;

const MAX_POSTER_BYTES: usize = 2 * 1024 * 1024;
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
    http: reqwest::Client,
    permits: Semaphore,
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

impl Art {
    pub fn new(state_dir: &Path) -> Result<Self> {
        let dir = state_dir.join("art");
        fsutil::ensure_private_dir(&dir)?;
        Ok(Art {
            dir,
            http: upstream::http_client(Duration::from_secs(15), reqwest::redirect::Policy::none()),
            permits: Semaphore::new(MAX_IN_FLIGHT),
        })
    }

    /// The poster of a media id, if there is one.
    pub async fn poster(&self, st: &AppState, id: &MediaId) -> Option<Poster> {
        // Every upstream call (the TVDB lookup included) runs under one permit, taken when needed.
        let mut permit = None;
        let source = match id {
            MediaId::Movie { tmdb } => {
                st.tmdb.as_ref()?;
                Source::Movie(*tmdb)
            }
            _ => {
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
            return hit;
        }
        if permit.is_none() {
            permit = Some(self.permits.acquire().await.ok()?);
        }
        let _permit = permit;
        // Another request may have fetched it while this one waited.
        if let Some(hit) = self.cached(&key) {
            return hit;
        }
        let url = match self.image_url(st, &source).await {
            Ok(Some(u)) => u,
            Ok(None) => {
                self.remember(&key, None);
                return None;
            }
            Err(()) => return None,
        };
        let resp = upstream::send(self.http.get(url)).await.ok()?;
        let allowed = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| matches!(t, "image/jpeg" | "image/png" | "image/webp"));
        if !allowed {
            return None;
        }
        let bytes = upstream::read_bytes(resp, MAX_POSTER_BYTES).await.ok()?;
        let kind = sniff(&bytes)?;
        self.remember(&key, Some(&bytes));
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

    /// The poster's address, on the configured image host only. `Err` when the provider could
    /// not be asked (nothing is remembered); `Ok(None)` when it has no usable poster.
    async fn image_url(&self, st: &AppState, source: &Source) -> Result<Option<Url>, ()> {
        match source {
            Source::Movie(id) => {
                let tmdb = st.tmdb.as_ref().ok_or(())?;
                let movie = tmdb.movie(*id).await.map_err(|_| ())?;
                let path = movie.and_then(|m| m.poster_path).filter(|p| {
                    p.starts_with('/')
                        && p.len() < 100
                        && p[1..]
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                });
                Ok(path.and_then(|p| {
                    let base = &st.cfg.tmdb.as_ref()?.image_base_url;
                    on_base(base, &format!("t/p/w342{p}"))
                }))
            }
            Source::Show(id) => {
                let tv = st.tvmaze.as_ref().ok_or(())?;
                let show = tv.by_id(*id).await.map_err(|_| ())?;
                let given = show
                    .and_then(|s| s.image?.medium)
                    .and_then(|u| Url::parse(&u).ok());
                Ok(given.and_then(|u| {
                    let base = &st.cfg.tvmaze.as_ref()?.image_base_url;
                    // Only the path is used: the address is always the configured host's.
                    (u.host_str() == base.host_str()).then_some(())?;
                    on_base(base, u.path().trim_start_matches('/'))
                }))
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
