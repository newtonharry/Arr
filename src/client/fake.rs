//! A deterministic in-memory download client for the integration tests, compiled only with
//! the `test-client` feature (the dev-dependency on this crate switches it on for
//! `cargo test`; a release build never contains it).
//!
//! It stands in for the embedded engine: `[test_client] id = "<name>"` next to a normal
//! `[embedded]` section swaps the engine for this client, so everything else (configuration,
//! categories and share limits, the journaled cleanup, import, the grab state machine) is the
//! real code. Its state lives in a process-wide registry keyed by that id, so a test holds
//! the same handle the application uses and the state survives a simulated restart.
//!
//! Like the engine, it reports torrents in qBittorrent's normalised vocabulary, lays out
//! files under the download directory, and never deletes data itself (removal is the
//! application's journaled job). Tests drive time and progress: `complete`, `seeded`,
//! `set_state`, and the fault switches (`down`, `down_after_add`, `withhold_add_answer`,
//! `refuse_next_add`).
use crate::config::EmbeddedConfig;
use crate::prowlarr::Payload;
use crate::qbit::{AddRequest, Preferences, Properties, TorrentFile, TorrentInfo};
use crate::upstream::UpstreamError;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Torrent name and its files (relative path, bytes): what "downloading" produces.
pub type Fixture = (String, Vec<(String, Vec<u8>)>);

#[derive(Clone, Debug)]
pub struct FakeTorrent {
    pub hash: String,
    pub name: String,
    pub category: String,
    pub tags: String,
    pub save_path: String,
    pub state: String,
    pub progress: f64,
    /// Relative to the save path (`<name>/<path>` for multi-file torrents).
    pub files: Vec<(String, u64)>,
    pub ratio: f64,
    pub ratio_limit: f64,
    pub seeding_time: i64,
    pub seeding_time_limit: i64,
    pub priorities: Vec<i64>,
}

/// What an add carried (the controls a test asserts on).
#[derive(Clone, Debug)]
pub struct AddRecord {
    pub hash: String,
    pub magnet: bool,
    pub save_path: String,
    pub category: String,
    pub tags: String,
    pub paused: bool,
    pub ratio_limit: Option<f64>,
    pub seeding_time_limit_minutes: Option<i64>,
}

#[derive(Default)]
pub struct FakeState {
    pub torrents: BTreeMap<String, FakeTorrent>,
    /// Release data by info-hash, registered by the test before the add.
    pub fixtures: HashMap<String, Fixture>,
    /// Calls received, oldest first (`add`, `info <hashes>`, `pause <h>`, `delete <h>` ...).
    pub log: Vec<String>,
    /// Every add that reached the client (also the withheld ones).
    pub add_fields: Vec<AddRecord>,
    /// Accept the add, then answer with an error (an ambiguous outcome).
    pub withhold_add_answer: bool,
    /// After accepting an add, go down (every later call fails).
    pub down_after_add: bool,
    /// Every call fails as unavailable.
    pub down: bool,
    /// The next add is refused (the client answers "not added").
    pub refuse_next_add: bool,
    /// Where data is written, and the save path the application knows (set when attached).
    pub download_dir: PathBuf,
    pub save_path: String,
    pub ratio_limit: Option<f64>,
    pub seeding_time_limit_minutes: Option<i64>,
}

pub type Handle = Arc<Mutex<FakeState>>;

fn registry() -> &'static Mutex<HashMap<String, Handle>> {
    static R: OnceLock<Mutex<HashMap<String, Handle>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// The state registered under `id` (created on first use).
pub fn handle(id: &str) -> Handle {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(id.to_string())
        .or_default()
        .clone()
}

fn down() -> UpstreamError {
    UpstreamError::Unavailable {
        reason: "HTTP 503".into(),
    }
}

fn effective<T: Copy + PartialOrd + Default>(own: T, global: Option<T>, minus_two: T) -> Option<T> {
    if own >= T::default() {
        Some(own)
    } else if own == minus_two {
        global
    } else {
        None
    }
}

impl FakeState {
    fn gate(&mut self, call: String) -> Result<(), UpstreamError> {
        self.log.push(call);
        if self.down { Err(down()) } else { Ok(()) }
    }

    fn local(&self, save_path: &str) -> PathBuf {
        let rest = save_path
            .strip_prefix(&self.save_path)
            .unwrap_or("")
            .trim_start_matches('/');
        if rest.is_empty() {
            self.download_dir.clone()
        } else {
            self.download_dir.join(rest)
        }
    }

    /// Simulates the download finishing: writes the release data under the download
    /// directory, marks the torrent complete and seeding.
    pub fn complete(&mut self, hash: &str) {
        let (name, files) = self
            .fixtures
            .get(hash)
            .cloned()
            .expect("fixture registered");
        let t = self.torrents.get(hash).cloned().expect("torrent added");
        let local = self.local(&t.save_path);
        let single = files.len() == 1 && !files[0].0.contains('/');
        for (p, b) in &files {
            let path = if single {
                local.join(p)
            } else {
                local.join(&name).join(p)
            };
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b).unwrap();
        }
        let t = self.torrents.get_mut(hash).unwrap();
        t.progress = 1.0;
        t.state = "stalledUP".into();
    }

    /// Simulates the client reaching its share limit.
    pub fn seeded(&mut self, hash: &str, ratio: f64, seeding_secs: i64, stopped: bool) {
        let t = self.torrents.get_mut(hash).unwrap();
        t.ratio = ratio;
        t.seeding_time = seeding_secs;
        if stopped {
            t.state = "pausedUP".into();
        }
    }

    pub fn set_state(&mut self, hash: &str, state: &str, progress: f64) {
        let t = self.torrents.get_mut(hash).unwrap();
        t.state = state.into();
        t.progress = progress;
    }
}

pub struct Fake {
    state: Handle,
}

impl Fake {
    /// Attaches to the registry entry `id` for the embedded configuration `cfg`.
    pub fn attach(id: &str, cfg: &EmbeddedConfig) -> Fake {
        let state = handle(id);
        {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.download_dir = cfg.download_dir.clone();
            s.save_path = cfg.save_path.clone();
            s.ratio_limit = cfg.ratio_limit;
            s.seeding_time_limit_minutes = cfg.seeding_time_limit_minutes;
        }
        Fake { state }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn save_path(&self) -> String {
        self.lock().save_path.clone()
    }

    fn view(t: &FakeTorrent, s: &FakeState) -> TorrentInfo {
        let size: u64 = t.files.iter().map(|f| f.1).sum();
        let multi = t.files.len() > 1 || t.files.iter().any(|f| f.0.contains('/'));
        TorrentInfo {
            hash: t.hash.clone(),
            name: t.name.clone(),
            category: t.category.clone(),
            tags: t.tags.clone(),
            state: t.state.clone(),
            progress: t.progress,
            size: size as i64,
            amount_left: ((1.0 - t.progress) * size as f64) as i64,
            save_path: t.save_path.clone(),
            dlspeed: 0,
            eta: 8_640_000,
            content_path: Some(if multi {
                format!("{}/{}", t.save_path, t.name)
            } else {
                format!(
                    "{}/{}",
                    t.save_path,
                    t.files.first().map_or(&t.name, |f| &f.0)
                )
            }),
            ratio: Some(t.ratio),
            ratio_limit: Some(t.ratio_limit),
            max_ratio: Some(effective(t.ratio_limit, s.ratio_limit, -2.0).unwrap_or(-1.0)),
            seeding_time: Some(t.seeding_time),
            seeding_time_limit: Some(t.seeding_time_limit),
            max_seeding_time: Some(
                effective(t.seeding_time_limit, s.seeding_time_limit_minutes, -2).unwrap_or(-1),
            ),
            downloaded: Some(size as i64),
            uploaded: Some((t.ratio * size as f64) as i64),
        }
    }

    pub async fn info(&self, hashes: &[String]) -> Result<Vec<TorrentInfo>, UpstreamError> {
        let mut s = self.lock();
        s.gate(format!("info {}", hashes.join("|")))?;
        let want: Vec<String> = hashes.iter().map(|h| h.to_ascii_lowercase()).collect();
        Ok(s.torrents
            .values()
            .filter(|t| want.contains(&t.hash))
            .map(|t| Self::view(t, &s))
            .collect())
    }

    pub async fn files(&self, hash: &str) -> Result<Option<Vec<TorrentFile>>, UpstreamError> {
        let mut s = self.lock();
        s.gate(format!("files {hash}"))?;
        Ok(s.torrents.get(&hash.to_ascii_lowercase()).map(|t| {
            t.files
                .iter()
                .enumerate()
                .map(|(i, (n, size))| TorrentFile {
                    index: Some(i as i64),
                    name: n.clone(),
                    size: *size as i64,
                    progress: t.progress,
                    priority: t.priorities.get(i).copied().unwrap_or(1),
                })
                .collect()
        }))
    }

    pub async fn add(&self, req: &AddRequest) -> Result<bool, UpstreamError> {
        let mut s = self.lock();
        s.gate("add".into())?;
        let (hash, magnet) = match &req.payload {
            Payload::Torrent(b) => (
                crate::torrent::info_hash(b).map_err(|_| UpstreamError::BadResponse {
                    reason: "the client rejected the torrent file as invalid".into(),
                })?,
                false,
            ),
            Payload::Magnet(m) => (
                crate::torrent::magnet_hash(m).map_err(|_| UpstreamError::BadResponse {
                    reason: "malformed magnet link".into(),
                })?,
                true,
            ),
        };
        s.add_fields.push(AddRecord {
            hash: hash.clone(),
            magnet,
            save_path: req.save_path.clone(),
            category: req.category.clone(),
            tags: req.tags.clone(),
            paused: req.paused,
            ratio_limit: req.ratio_limit,
            seeding_time_limit_minutes: req.seeding_time_limit_minutes,
        });
        if s.refuse_next_add {
            s.refuse_next_add = false;
            return Ok(false);
        }
        // Already managed (a replayed add): confirmed, reconciliation decides whether it is ours.
        if !s.torrents.contains_key(&hash) {
            let name = s
                .fixtures
                .get(&hash)
                .map(|f| f.0.clone())
                .unwrap_or_else(|| {
                    if magnet {
                        "metadata pending"
                    } else {
                        "unnamed"
                    }
                    .into()
                });
            let files: Vec<(String, u64)> = s
                .fixtures
                .get(&hash)
                .map(|(n, f)| {
                    let single = f.len() == 1 && !f[0].0.contains('/');
                    f.iter()
                        .map(|(p, b)| {
                            (
                                if single {
                                    p.clone()
                                } else {
                                    format!("{n}/{p}")
                                },
                                b.len() as u64,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let state = if req.paused {
                "pausedDL"
            } else if files.is_empty() {
                "metaDL"
            } else {
                "downloading"
            };
            let t = FakeTorrent {
                hash: hash.clone(),
                name,
                category: req.category.clone(),
                tags: req.tags.clone(),
                save_path: req.save_path.trim_end_matches('/').to_string(),
                state: state.into(),
                progress: 0.0,
                priorities: vec![1; files.len()],
                files,
                ratio: 0.0,
                ratio_limit: req.ratio_limit.unwrap_or(-2.0),
                seeding_time: 0,
                seeding_time_limit: req.seeding_time_limit_minutes.unwrap_or(-2),
            };
            s.torrents.insert(hash, t);
        }
        if s.down_after_add {
            s.down_after_add = false;
            s.down = true;
        }
        if s.withhold_add_answer {
            s.withhold_add_answer = false;
            return Err(UpstreamError::Unavailable {
                reason: "HTTP 500".into(),
            });
        }
        Ok(true)
    }

    pub async fn pause(&self, hash: &str) -> Result<(), UpstreamError> {
        let mut s = self.lock();
        s.gate(format!("pause {hash}"))?;
        if let Some(t) = s.torrents.get_mut(&hash.to_ascii_lowercase()) {
            t.state = if t.progress >= 1.0 {
                "pausedUP"
            } else {
                "pausedDL"
            }
            .into();
        }
        Ok(())
    }

    pub async fn resume(&self, hash: &str) -> Result<(), UpstreamError> {
        let mut s = self.lock();
        s.gate(format!("resume {hash}"))?;
        if let Some(t) = s.torrents.get_mut(&hash.to_ascii_lowercase()) {
            t.state = if t.progress >= 1.0 {
                "stalledUP"
            } else {
                "downloading"
            }
            .into();
        }
        Ok(())
    }

    /// Removes the torrent; data is never deleted here (as with the engine).
    pub async fn delete(&self, hash: &str) -> Result<(), UpstreamError> {
        let mut s = self.lock();
        s.gate(format!("delete {hash}"))?;
        s.torrents.remove(&hash.to_ascii_lowercase());
        Ok(())
    }

    pub async fn set_category(&self, hash: &str, category: &str) -> Result<(), UpstreamError> {
        let mut s = self.lock();
        s.gate(format!("setCategory {hash}"))?;
        if let Some(t) = s.torrents.get_mut(&hash.to_ascii_lowercase()) {
            t.category = category.into();
        }
        Ok(())
    }

    pub fn preferences(&self) -> Preferences {
        let s = self.lock();
        Preferences {
            max_ratio_enabled: Some(s.ratio_limit.is_some()),
            max_ratio: s.ratio_limit,
            max_seeding_time_enabled: Some(s.seeding_time_limit_minutes.is_some()),
            max_seeding_time: s.seeding_time_limit_minutes,
        }
    }

    pub async fn properties(&self, hash: &str) -> Result<Option<Properties>, UpstreamError> {
        let mut s = self.lock();
        s.gate(format!("properties {hash}"))?;
        Ok(s.torrents
            .get(&hash.to_ascii_lowercase())
            .map(|t| Properties {
                share_ratio: Some(t.ratio),
                seeding_time: Some(t.seeding_time),
                total_downloaded: Some(1),
                total_uploaded: Some(1),
                save_path: Some(t.save_path.clone()),
            }))
    }
}
