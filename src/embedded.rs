//! The embedded BitTorrent engine: one librqbit 9.0.1 session inside this process, used
//! instead of qBittorrent when `[embedded]` is configured. There is no HTTP daemon, web UI,
//! qBittorrent URL, credential or API.
//!
//! The rest of the app sees the same normalised client view as with qBittorrent
//! (`TorrentInfo`, `TorrentFile`, the qBittorrent state vocabulary), filled from the engine's
//! real state and from this module's own records:
//! - **Ownership.** Category and the `unified-media` tag are what the app assigned when it
//!   added the torrent (`embedded_torrents`). A torrent in the engine without that record
//!   reports neither, so it is treated as foreign.
//! - **Layout.** Files land where qBittorrent's default ("Original") layout would put them:
//!   a single-file torrent in the download directory, a multi-file torrent in
//!   `download_dir/<name>/`. Names are checked before anything is written.
//! - **Accounting.** The engine's upload and download counters exist only while a torrent is
//!   live and restart at every pause and session start. Lifetime bytes and seeding time are
//!   kept in SQLite, adding only increments observed within one live state, so a restart can
//!   lose a few unobserved bytes but never invents any. Seeding time counts only intervals
//!   whose both ends saw the torrent complete and live in the same live state.
//! - **Share limits.** The engine has none. They are enforced here: a complete torrent is
//!   paused once its ratio or seeding-time limit is met (qBittorrent's pause action), which
//!   is what lets the unchanged cleanup logic remove it.
//! - **Verification.** The engine checks each piece as it arrives, but trusts its fast-resume
//!   bitfield after a restart. Before a torrent is reported complete (and so before any
//!   import), every piece is checked once more against its hash from the files on disk.
//! - **Data.** Removal never deletes data in the engine. The app's journaled removal deletes
//!   exactly the tracked files, after its inode, size and symlink checks.

use crate::config::EmbeddedConfig;
use crate::fsutil;
use crate::paths;
use crate::prowlarr::Payload;
use crate::qbit::{AddRequest, Preferences, Properties, TorrentFile, TorrentInfo};
use crate::store::Store;
use crate::upstream::UpstreamError;
use librqbit::api::TorrentIdOrHash;
use librqbit::dht::{DhtPersistenceConfig, Id20};
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerMode,
    ListenerOptions, ManagedTorrent, Session, SessionOptions, SessionPersistenceConfig,
    TorrentStats, TorrentStatsState,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::OnceCell;

pub const ENGINE: &str = "librqbit 9.0.1";

pub struct Embedded {
    cfg: EmbeddedConfig,
    store: Store,
    session: OnceCell<Arc<Session>>,
    /// Serialises observations with engine controls (pause, resume, removal, shutdown), so a
    /// counter reading always belongs to the live state it was taken from.
    ops: tokio::sync::Mutex<()>,
    seen: Mutex<HashMap<String, Seen>>,
    verifying: Arc<Mutex<HashSet<String>>>,
    /// Running full piece checks, drained before the engine lock is released.
    checks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Tests hold this to keep spawned piece checks from running until they release it.
    #[cfg(test)]
    check_gate: Arc<tokio::sync::RwLock<()>>,
    /// The state directory's exclusive engine lock, held until the session has stopped.
    lock: Mutex<Option<std::fs::File>>,
}

/// Identity of one engine live state. A weak reference keeps its allocation, so the address
/// cannot be reused by a later live state while it is remembered.
type LiveRef = std::sync::Weak<dyn std::any::Any + Send + Sync>;

/// One reading of a live state's own counters.
#[derive(Debug, Clone, Copy)]
struct Sample {
    uploaded: u64,
    fetched: u64,
    finished: bool,
    at: Instant,
}

/// The previous reading of a torrent in this process.
struct Seen {
    live: LiveRef,
    sample: Sample,
    /// Seeding milliseconds not yet recorded as a whole second.
    carry_ms: u64,
}

/// What a reading adds to the lifetime counters.
#[derive(Debug, PartialEq, Eq)]
struct Delta {
    uploaded: u64,
    fetched: u64,
    seeding_secs: u64,
    carry_ms: u64,
    consistent: bool,
}

/// The increments from `prev`, the previous reading of the same live state (with its carry),
/// to `cur`. A live state starts its counters at zero, so without a previous reading of it
/// everything it counted is new. Seeding time accrues only between two readings that both
/// saw the torrent complete.
fn delta(prev: Option<(&Sample, u64)>, cur: &Sample) -> Delta {
    let Some((p, carry)) = prev else {
        return Delta {
            uploaded: cur.uploaded,
            fetched: cur.fetched,
            seeding_secs: 0,
            carry_ms: 0,
            consistent: true,
        };
    };
    match (
        cur.uploaded.checked_sub(p.uploaded),
        cur.fetched.checked_sub(p.fetched),
    ) {
        (Some(uploaded), Some(fetched)) => {
            let seeding = if p.finished && cur.finished {
                cur.at.saturating_duration_since(p.at).as_millis() as u64
            } else {
                0
            };
            let ms = seeding + carry;
            Delta {
                uploaded,
                fetched,
                seeding_secs: ms / 1000,
                carry_ms: ms % 1000,
                consistent: true,
            }
        }
        // A live state's counters only grow: anything else is not trusted.
        _ => Delta {
            uploaded: 0,
            fetched: 0,
            seeding_secs: 0,
            carry_ms: 0,
            consistent: false,
        },
    }
}

/// This app's record of an engine torrent.
#[derive(Debug, Clone)]
struct Row {
    name: String,
    category: String,
    tags: String,
    save_path: String,
    output_folder: String,
    multi_file: bool,
    ratio_limit: f64,
    seeding_time_limit: i64,
    uploaded: i64,
    downloaded: i64,
    seeding_secs: i64,
    counters_valid: bool,
    removed: bool,
    verified: bool,
    verify_error: Option<String>,
    admitted: bool,
}

/// What a full piece check reads: piece length, the concatenated piece hashes, and the
/// files in torrent order (relative path, length, padding).
pub struct Layout {
    pub piece_length: u64,
    pub pieces: Vec<u8>,
    pub files: Vec<(PathBuf, u64, bool)>,
}

impl Layout {
    pub fn of<B: AsRef<[u8]>>(info: &librqbit::ValidatedTorrentMetaV1Info<B>) -> Layout {
        Layout {
            piece_length: u64::from(info.info().piece_length),
            pieces: info.info().pieces.as_ref().to_vec(),
            files: info
                .iter_file_details()
                .map(|f| (f.filename.to_pathbuf(), f.len, f.attrs().padding))
                .collect(),
        }
    }
}

/// Checks every piece of the files below `root` against its hash. Each file must be a
/// regular file of exactly its length, reached without symlinks; padding entries are zeros.
pub fn verify_pieces(root: &Path, layout: &Layout) -> Result<(), String> {
    use sha1::{Digest, Sha1};
    let piece = layout.piece_length;
    if piece == 0 || !layout.pieces.len().is_multiple_of(20) {
        return Err("malformed piece table".into());
    }
    let mut hasher = Sha1::new();
    let (mut filled, mut index) = (0u64, 0usize);
    let mut buf = vec![0u8; 256 * 1024];
    let finish = |hasher: &mut Sha1, index: &mut usize| -> Result<(), String> {
        let digest = hasher.finalize_reset();
        let expected = layout
            .pieces
            .get(*index * 20..*index * 20 + 20)
            .ok_or("more data than the piece table covers")?;
        if digest.as_slice() != expected {
            return Err(format!("piece {} does not match its hash", *index));
        }
        *index += 1;
        Ok(())
    };
    for (rel, len, padding) in &layout.files {
        let mut reader: Box<dyn Read> = if *padding {
            Box::new(std::io::repeat(0).take(*len))
        } else {
            let path = root.join(rel);
            paths::check_no_symlinks(root, &path)?;
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|e| format!("{}: {}", rel.display(), e.kind()))?;
            if !meta.file_type().is_file() || meta.len() != *len {
                return Err(format!(
                    "{} is not a regular file of {len} bytes",
                    rel.display()
                ));
            }
            Box::new(
                std::fs::File::open(&path)
                    .map_err(|e| format!("{}: {}", rel.display(), e.kind()))?,
            )
        };
        let mut remaining = *len;
        while remaining > 0 {
            let want = (buf.len() as u64).min(remaining).min(piece - filled) as usize;
            reader
                .read_exact(&mut buf[..want])
                .map_err(|e| format!("{}: {}", rel.display(), e.kind()))?;
            hasher.update(&buf[..want]);
            filled += want as u64;
            remaining -= want as u64;
            if filled == piece {
                finish(&mut hasher, &mut index)?;
                filled = 0;
            }
        }
    }
    if filled > 0 {
        finish(&mut hasher, &mut index)?;
    }
    if index * 20 != layout.pieces.len() {
        return Err("the files are shorter than the piece table".into());
    }
    Ok(())
}

/// A torrent's metadata as the engine will lay it out, after validation.
#[derive(Debug)]
struct Inspected {
    hash: String,
    name: String,
    multi: bool,
    /// Paths relative to the output folder, as the engine writes them.
    files: Vec<PathBuf>,
}

fn now_s() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn unavailable(what: impl std::fmt::Display) -> UpstreamError {
    UpstreamError::Unavailable {
        reason: format!("embedded engine: {what}"),
    }
}

/// A librqbit failure, described by stage and error kind only. The engine's own messages can
/// carry tracker URLs (with passkeys), magnet links or peer addresses; none leaves here.
fn engine_failure(stage: &str, e: &anyhow::Error) -> UpstreamError {
    unavailable(format_args!("{stage} failed ({})", failure_kind(e)))
}

fn failure_kind(e: &anyhow::Error) -> String {
    match e.chain().find_map(|c| c.downcast_ref::<std::io::Error>()) {
        Some(io) => format!("i/o: {:?}", io.kind()),
        None => "engine error".into(),
    }
}

fn io_failure(stage: &str, e: &std::io::Error) -> UpstreamError {
    unavailable(format_args!("{stage} failed (i/o: {:?})", e.kind()))
}

fn db_failure(e: anyhow::Error) -> UpstreamError {
    engine_failure("the state database", &e)
}

fn invalid(what: impl std::fmt::Display) -> UpstreamError {
    UpstreamError::BadResponse {
        reason: format!("invalid torrent for the embedded engine: {what}"),
    }
}

fn id20(hash: &str) -> Option<Id20> {
    crate::torrent::is_info_hash(hash)
        .then(|| Id20::from_str(hash).ok())
        .flatten()
}

/// Parses and validates torrent bytes with the engine's own parser (so the names checked
/// are the names it writes): safe relative paths, a safe directory name for multi-file
/// torrents, and no symlink entries.
fn inspect(bytes: &[u8]) -> Result<Inspected, String> {
    let t = librqbit::torrent_from_bytes(bytes)
        .map_err(|_| "it is not a parseable .torrent (bencode)".to_string())?;
    let hash = t.info_hash.as_string();
    let info = t
        .info
        .data
        .validate()
        .map_err(|_| "the engine's metadata validation rejected it".to_string())?;
    let multi = info.info().files.is_some();
    let name = info.name().map(|n| n.into_owned()).unwrap_or_default();
    if multi && (paths::safe_rel(&name).is_none() || name.contains('/')) {
        return Err(format!("unsafe torrent name {name:?}"));
    }
    let mut files = vec![];
    for f in info.iter_file_details() {
        if f.attrs().symlink {
            return Err("the torrent contains a symlink entry".into());
        }
        let rel = f.filename.to_pathbuf();
        if paths::safe_rel(&rel.to_string_lossy()).is_none() {
            return Err(format!("unsafe file name {:?}", rel.to_string_lossy()));
        }
        files.push(rel);
    }
    Ok(Inspected {
        hash,
        name,
        multi,
        files,
    })
}

/// The download directory and every ancestor that exists must be real directories, so that
/// confinement below it means what it says.
fn real_directories(path: &Path) -> Result<(), UpstreamError> {
    let mut p = PathBuf::new();
    for c in path.components() {
        p.push(c);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(unavailable(format_args!(
                    "{} is a symbolic link; embedded.download_dir must be a real path without symlinks",
                    p.display()
                )));
            }
            Ok(m) if !m.is_dir() => {
                return Err(unavailable(format_args!(
                    "{} is not a directory",
                    p.display()
                )));
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// Takes the exclusive engine lock of a state's session directory (non-blocking).
fn lock_session(dir: &Path) -> Result<std::fs::File, UpstreamError> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let f = o
        .open(dir.join("engine.lock"))
        .map_err(|e| io_failure("opening the engine lock", &e))?;
    // flock(LOCK_EX | LOCK_NB) on Unix, LockFileEx on Windows.
    if f.try_lock().is_err() {
        return Err(unavailable(
            "another engine already runs on this state directory; one state directory has one embedded engine",
        ));
    }
    Ok(f)
}

/// Issues and binds the token of a new full piece check: only a completed, unverified,
/// active record is bound. Removal, re-adding and a new engine session clear it.
async fn bind_check(store: &Store, hash: &str) -> Option<String> {
    let token = fsutil::random_hex(12).ok()?;
    let (h, t) = (hash.to_string(), token.clone());
    let bound = store
        .with(move |c| {
            Ok(c.execute(
                "UPDATE embedded_torrents SET verify_token = ?2 WHERE info_hash = ?1 AND completed_at IS NOT NULL AND verified_at IS NULL AND verify_error IS NULL AND removed_at IS NULL",
                params![h, t],
            )?)
        })
        .await
        .ok()?;
    (bound == 1).then_some(token)
}

/// Records a check's result only while its token is still the record's. Returns whether it
/// was recorded.
async fn finish_check(store: &Store, hash: &str, token: &str, result: Result<(), String>) -> bool {
    let (h, t) = (hash.to_string(), token.to_string());
    store
        .with(move |c| {
            let n = match result {
                Ok(()) => c.execute(
                    "UPDATE embedded_torrents SET verified_at = ?3, verify_error = NULL, updated_at = ?3 WHERE info_hash = ?1 AND verify_token = ?2 AND removed_at IS NULL",
                    params![h, t, now_s()],
                )?,
                Err(e) => c.execute(
                    "UPDATE embedded_torrents SET verify_error = ?3, updated_at = ?4 WHERE info_hash = ?1 AND verify_token = ?2 AND removed_at IS NULL",
                    params![h, t, e, now_s()],
                )?,
            };
            Ok(n == 1)
        })
        .await
        .unwrap_or(false)
}

/// The effective limit: `>= 0` own, `-2` the configured global one, `-1` none.
fn effective<T: Copy + PartialOrd + Default>(own: T, global: Option<T>, minus_two: T) -> Option<T> {
    if own >= T::default() {
        Some(own)
    } else if own == minus_two {
        global
    } else {
        None
    }
}

impl Embedded {
    pub fn new(cfg: EmbeddedConfig, store: Store) -> Self {
        Embedded {
            cfg,
            store,
            session: OnceCell::new(),
            ops: tokio::sync::Mutex::new(()),
            seen: Mutex::new(HashMap::new()),
            verifying: Arc::new(Mutex::new(HashSet::new())),
            checks: Mutex::new(vec![]),
            #[cfg(test)]
            check_gate: Arc::default(),
            lock: Mutex::new(None),
        }
    }

    pub fn save_path(&self) -> &str {
        &self.cfg.save_path
    }

    pub fn started(&self) -> bool {
        self.session.initialized()
    }

    /// Starts the session (once): listeners, persistence and fast-resume under the state
    /// directory, and every persisted torrent resumed after the checks below. DHT, local
    /// discovery and UPnP only as configured (UPnP never).
    pub async fn start(&self) -> Result<Arc<Session>, UpstreamError> {
        self.session.get_or_try_init(|| self.open()).await.cloned()
    }

    async fn open(&self) -> Result<Arc<Session>, UpstreamError> {
        let c = &self.cfg;
        fsutil::ensure_private_dir(&c.session_dir)
            .map_err(|e| engine_failure("preparing the engine state", &e))?;
        // One engine per state directory, held until this engine has stopped.
        let lock = lock_session(&c.session_dir)?;
        self.bind_download_dir().await?;
        let persistence = c.session_dir.join("session");
        fsutil::ensure_private_dir(&persistence)
            .map_err(|e| engine_failure("preparing the engine state", &e))?;
        real_directories(&c.download_dir)?;
        std::fs::create_dir_all(&c.download_dir)
            .map_err(|e| io_failure("creating the download directory", &e))?;
        self.check_persisted(&persistence).await?;
        self.reset_pending_verification().await?;
        let dht = c.dht.then(|| DhtSessionConfig {
            bootstrap_addrs: None,
            port: None,
            persistence: Some(DhtPersistenceConfig {
                dump_interval: None,
                config_filename: Some(c.session_dir.join("dht.json")),
            }),
        });
        let opts = SessionOptions {
            dht,
            disable_trackers: !c.trackers,
            fastresume: true,
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(persistence),
            }),
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpOnly,
                listen_addr: c.listen,
                enable_upnp_port_forwarding: false,
                ipv4_only: c.listen.is_ipv4(),
                ..Default::default()
            }),
            ratelimits: LimitsConfig {
                upload_bps: c.upload_bps,
                download_bps: c.download_bps,
            },
            peer_limit: c.peer_limit,
            disable_local_service_discovery: !c.local_discovery,
            ipv4_only: c.listen.is_ipv4(),
            ..Default::default()
        };
        let session = Session::new_with_opts(c.download_dir.clone(), opts)
            .await
            .map_err(|e| engine_failure("starting the engine session", &e))?;
        if let Err(e) = self.after_restore(&session).await {
            // Nothing is left running when startup fails.
            session.stop().await;
            return Err(e);
        }
        *self.lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(lock);
        Ok(session)
    }

    /// A torrent the engine restored from its own persistence was created there without
    /// overwriting anything (or resumed by an admitted record), so its record is admitted.
    async fn after_restore(&self, session: &Arc<Session>) -> Result<(), UpstreamError> {
        let restored: Vec<String> = session.with_torrents(|it| {
            it.map(|(_, t)| t.info_hash().as_string())
                .collect::<Vec<_>>()
        });
        self.store
            .with(move |c| {
                let now = now_s();
                for h in &restored {
                    c.execute(
                        "UPDATE embedded_torrents SET admitted_at = COALESCE(admitted_at, ?2) WHERE info_hash = ?1 AND removed_at IS NULL",
                        params![h, now],
                    )?;
                }
                Ok(())
            })
            .await
            .map_err(db_failure)?;
        self.reattach_initial_peers(session).await
    }

    /// A completed download not yet imported is checked again in every engine session: its
    /// files could have changed while the engine was stopped, and the engine's fast-resume
    /// samples only a few pieces. Imported downloads keep their check (the library has the
    /// data), and transfer counters are untouched.
    async fn reset_pending_verification(&self) -> Result<(), UpstreamError> {
        self.store
            .with(|c| {
                c.execute(
                    "UPDATE embedded_torrents SET verified_at = NULL, verify_token = NULL, updated_at = ?1
                     WHERE removed_at IS NULL AND (verified_at IS NOT NULL OR verify_token IS NOT NULL)
                       AND info_hash NOT IN (SELECT info_hash FROM grabs WHERE state = 'imported')",
                    [now_s()],
                )?;
                Ok(())
            })
            .await
            .map_err(db_failure)
    }

    /// The engine's persisted torrents resume at the paths they were added with. A state is
    /// therefore bound to its first download directory; a different one is refused before
    /// anything resumes.
    async fn bind_download_dir(&self) -> Result<(), UpstreamError> {
        let dir = self.cfg.save_path.clone();
        let bound = self
            .store
            .with(move |c| {
                let bound: Option<String> = c
                    .query_row(
                        "SELECT value FROM meta WHERE key = 'embedded_download_dir'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                if bound.is_none() {
                    c.execute(
                        "INSERT INTO meta (key, value) VALUES ('embedded_download_dir', ?1)",
                        [&dir],
                    )?;
                }
                Ok(bound)
            })
            .await
            .map_err(db_failure)?;
        match bound {
            Some(b) if b != self.cfg.save_path => Err(unavailable(format_args!(
                "embedded.download_dir changed from {b} to {}; the engine's torrents would resume at the old paths. Restore the original directory or use a fresh state directory",
                self.cfg.save_path
            ))),
            _ => Ok(()),
        }
    }

    /// The engine resumes its persisted torrents at their recorded folders as soon as the
    /// session starts. Each one must be a torrent this app recorded, not removed, at exactly
    /// the folder it recorded, below the download directory and reached without symlinks;
    /// otherwise nothing starts.
    async fn check_persisted(&self, persistence: &Path) -> Result<(), UpstreamError> {
        let file = persistence.join("session.json");
        if std::fs::symlink_metadata(&file).is_err() {
            return Ok(());
        }
        let refuse = |why: &str| {
            unavailable(format_args!(
                "{why}; nothing is restored (each persisted torrent must be one this app recorded, at its recorded folder below the download directory, without symlinks)"
            ))
        };
        let bytes = fsutil::read_bounded(&file, 64 * 1024 * 1024)
            .map_err(|_| refuse("the engine's session.json cannot be read"))?;
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|_| refuse("the engine's session.json is not valid JSON"))?;
        let Some(torrents) = v.get("torrents").and_then(Value::as_object) else {
            return Err(refuse("the engine's session.json has no torrent table"));
        };
        let rows: HashMap<String, (String, bool)> = self
            .store
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT info_hash, output_folder, removed_at IS NOT NULL FROM embedded_torrents",
                )?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
                    .collect::<rusqlite::Result<HashMap<_, _>>>()?;
                Ok(rows)
            })
            .await
            .map_err(db_failure)?;
        let root = &self.cfg.download_dir;
        for t in torrents.values() {
            let hash = t["info_hash"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let out = t["output_folder"].as_str().unwrap_or_default();
            let recorded = matches!(rows.get(&hash), Some((o, false)) if o == out);
            let confined = Path::new(out).starts_with(root)
                && paths::check_no_symlinks(root, Path::new(out)).is_ok();
            if !recorded || !confined {
                return Err(refuse(
                    "a persisted engine torrent is not where this app recorded it",
                ));
            }
        }
        Ok(())
    }

    /// Torrents restored from the engine's persistence keep no `initial_peers` (librqbit
    /// 9.0.1 does not persist them, nor can peers be added to a running torrent). With
    /// explicit peers configured, every incomplete torrent this app recorded is re-added at
    /// the same place with them; the engine then re-checks its existing data piece by piece.
    async fn reattach_initial_peers(&self, session: &Arc<Session>) -> Result<(), UpstreamError> {
        if self.cfg.initial_peers.is_empty() {
            return Ok(());
        }
        let incomplete: HashSet<String> = self
            .store
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT info_hash FROM embedded_torrents WHERE completed_at IS NULL AND removed_at IS NULL",
                )?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<HashSet<_>>>()?;
                Ok(rows)
            })
            .await
            .map_err(db_failure)?;
        let restored: Vec<Arc<ManagedTorrent>> = session.with_torrents(|it| {
            it.map(|(_, t)| t.clone())
                .filter(|t| incomplete.contains(&t.info_hash().as_string()))
                .collect()
        });
        let stage = "re-adding a restored torrent with its explicit peers";
        for t in restored {
            let bytes = t
                .with_metadata(|m| m.torrent_bytes.clone())
                .map_err(|e| engine_failure(stage, &e))?;
            let output = t.output_folder().to_string_lossy().to_string();
            let paused = t.is_paused();
            session
                .delete(TorrentIdOrHash::Hash(t.info_hash()), false)
                .await
                .map_err(|e| engine_failure(stage, &e))?;
            session
                .add_torrent(
                    AddTorrent::from_bytes(bytes),
                    Some(AddTorrentOptions {
                        paused,
                        output_folder: Some(output),
                        overwrite: true,
                        initial_peers: Some(self.cfg.initial_peers.clone()),
                        ..Default::default()
                    }),
                )
                .await
                .map_err(|e| engine_failure(stage, &e))?;
        }
        Ok(())
    }

    /// The running session, once started.
    pub fn session(&self) -> Option<&Arc<Session>> {
        self.session.get()
    }

    /// The bound BitTorrent listen address once started (the actual port when configured 0).
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.session.get().and_then(|s| s.listen_addr())
    }

    /// Records the counters of every torrent, stops the session (persistence flushed;
    /// torrents resume on the next start), lets running piece checks finish, and only then
    /// releases the state directory's engine lock.
    pub async fn shutdown(&self) {
        let Some(session) = self.session.get() else {
            return;
        };
        {
            let _ops = self.ops.lock().await;
            let handles: Vec<(String, Arc<ManagedTorrent>)> = session.with_torrents(|it| {
                it.map(|(_, t)| (t.info_hash().as_string(), t.clone()))
                    .collect()
            });
            for (hash, h) in handles {
                let _ = self.observe(session, &hash, &h).await;
            }
            session.stop().await;
        }
        let checks = std::mem::take(&mut *self.checks.lock().unwrap_or_else(|e| e.into_inner()));
        for c in checks {
            let _ = c.await;
        }
        self.lock.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Engine status for diagnostics: configuration and aggregates of the engine's own
    /// state. Read-only (no counter is recorded); no paths, hashes or peer addresses.
    pub fn status(&self) -> Value {
        let c = &self.cfg;
        let mut v = json!({
            "backend": "embedded",
            "engine": ENGINE,
            "transport": "tcp",
            "trackers": c.trackers,
            "dht": c.dht,
            "local_discovery": c.local_discovery,
            "upnp": false,
            "pex": "on for public torrents (not configurable in librqbit 9.0.1); never for private torrents",
            "initial_peers": c.initial_peers.len(),
            "max_active_downloads": c.max_active_downloads,
            "share_limits_without_indexer_goal": { "ratio": c.ratio_limit, "seeding_time_minutes": c.seeding_time_limit_minutes },
        });
        match self.session.get() {
            None => v["status"] = json!("not_started"),
            Some(s) => {
                v["status"] = json!("running");
                v["listen"] = json!(s.listen_addr().map(|a| a.to_string()));
                let mut n = [0u64; 6];
                let (mut peers, mut down, mut up) = (0u64, 0u64, 0u64);
                let all: Vec<Arc<ManagedTorrent>> =
                    s.with_torrents(|it| it.map(|(_, t)| t.clone()).collect());
                {
                    for t in &all {
                        let st = t.stats();
                        let slot = match st.state {
                            TorrentStatsState::Initializing { .. } => 0,
                            TorrentStatsState::Live if st.finished => 2,
                            TorrentStatsState::Live => 1,
                            TorrentStatsState::Paused => 3,
                            TorrentStatsState::Error => 4,
                        };
                        n[slot] += 1;
                        n[5] += u64::from(st.finished);
                        if let Some(l) = &st.live {
                            peers += u64::from(l.snapshot.peer_stats.live);
                            down += l.download_speed.as_bytes();
                            up += l.upload_speed.as_bytes();
                        }
                    }
                }
                v["torrents"] = json!(n[..5].iter().sum::<u64>());
                v["engine_states"] = json!({
                    "checking": n[0], "downloading": n[1], "seeding": n[2], "paused": n[3], "error": n[4], "complete": n[5],
                });
                v["live_peers"] = json!(peers);
                v["download_bytes_per_sec"] = json!(down);
                v["upload_bytes_per_sec"] = json!(up);
            }
        }
        v
    }

    async fn row(&self, hash: &str) -> Result<Option<Row>, UpstreamError> {
        let h = hash.to_string();
        self.store
            .with(move |c| {
                Ok(c.query_row(
                    "SELECT name, category, tags, save_path, output_folder, multi_file, ratio_limit, seeding_time_limit, uploaded, downloaded, seeding_secs, counters_valid, removed_at IS NOT NULL, verified_at IS NOT NULL, verify_error, admitted_at IS NOT NULL FROM embedded_torrents WHERE info_hash = ?1",
                    [&h],
                    |r| {
                        Ok(Row {
                            name: r.get(0)?,
                            category: r.get(1)?,
                            tags: r.get(2)?,
                            save_path: r.get(3)?,
                            output_folder: r.get(4)?,
                            multi_file: r.get(5)?,
                            ratio_limit: r.get(6)?,
                            seeding_time_limit: r.get(7)?,
                            uploaded: r.get(8)?,
                            downloaded: r.get(9)?,
                            seeding_secs: r.get(10)?,
                            counters_valid: r.get(11)?,
                            removed: r.get(12)?,
                            verified: r.get(13)?,
                            verify_error: r.get(14)?,
                            admitted: r.get(15)?,
                        })
                    },
                )
                .optional()?)
            })
            .await
            .map_err(db_failure)
    }

    fn handle(&self, session: &Session, hash: &str) -> Option<Arc<ManagedTorrent>> {
        session.get(TorrentIdOrHash::Hash(id20(hash)?))
    }

    /// Adds a torrent: `.torrent` bytes, or a magnet whose metadata is first fetched from
    /// peers without writing anything. Returns `Ok(true)` once the engine manages it.
    pub async fn add(&self, req: &AddRequest) -> Result<bool, UpstreamError> {
        let session = self.start().await?;
        let _ops = self.ops.lock().await;
        let peers = (!self.cfg.initial_peers.is_empty()).then(|| self.cfg.initial_peers.clone());
        let bytes: Vec<u8> = match &req.payload {
            Payload::Torrent(b) => b.clone(),
            Payload::Magnet(m) => {
                if crate::torrent::magnet_hash(m).is_err() {
                    return Err(invalid("malformed magnet link"));
                }
                let opts = AddTorrentOptions {
                    list_only: true,
                    initial_peers: peers.clone(),
                    ..Default::default()
                };
                let fetch = session.add_torrent(AddTorrent::from_url(m.clone()), Some(opts));
                match tokio::time::timeout(self.cfg.metadata_timeout, fetch).await {
                    Err(_) => {
                        return Err(unavailable(format_args!(
                            "magnet metadata was not received within {} s",
                            self.cfg.metadata_timeout.as_secs()
                        )));
                    }
                    Ok(Err(e)) => return Err(engine_failure("fetching magnet metadata", &e)),
                    Ok(Ok(AddTorrentResponse::ListOnly(l))) => l.torrent_bytes.to_vec(),
                    // Already managed by hash: reconciliation decides whether it is ours.
                    Ok(Ok(_)) => return Ok(true),
                }
            }
        };
        let t = inspect(&bytes).map_err(invalid)?;
        if let Payload::Magnet(m) = &req.payload
            && crate::torrent::magnet_hash(m).ok().as_deref() != Some(t.hash.as_str())
        {
            return Err(invalid(
                "the metadata does not match the magnet's info-hash",
            ));
        }
        if req.save_path.trim_end_matches('/') != self.cfg.save_path {
            return Err(invalid(
                "the grab's save path is not the embedded download directory",
            ));
        }
        // Already managed by the engine (a replayed or retried add): its record keeps its
        // counters and verification and the observation of its live state stays, so the
        // replay counts nothing again; it is only confirmed admitted, as at a restore.
        // Reconciliation by hash decides whether it is ours.
        if self.handle(&session, &t.hash).is_some() {
            let hash = t.hash.clone();
            self.store
                .with(move |c| {
                    c.execute(
                        "UPDATE embedded_torrents SET admitted_at = COALESCE(admitted_at, ?2) WHERE info_hash = ?1 AND removed_at IS NULL",
                        params![hash, now_s()],
                    )?;
                    Ok(())
                })
                .await
                .map_err(db_failure)?;
            return Ok(true);
        }
        let output = if t.multi {
            self.cfg.download_dir.join(&t.name)
        } else {
            self.cfg.download_dir.clone()
        };
        let output_s = output.to_string_lossy().to_string();
        // Nothing is written through a symlink: every existing component from the download
        // directory to the output folder and to each file must be a real directory or file.
        for f in std::iter::once(output.clone()).chain(t.files.iter().map(|f| output.join(f))) {
            paths::check_no_symlinks(&self.cfg.download_dir, &f)
                .map_err(|e| invalid(format_args!("{e}; nothing is written through a symlink")))?;
        }
        // Admission. Existing files may be resumed only by an admitted, active record of this
        // torrent at the same place: one whose files the engine itself created. A record that
        // never passed admission (for example left by a crash before the engine opened its
        // files) proves nothing, so any existing file at the destination is refused.
        let row = self.row(&t.hash).await?;
        let trusted = row
            .as_ref()
            .is_some_and(|r| r.admitted && !r.removed && r.output_folder == output_s);
        if !trusted
            && let Some(f) = t
                .files
                .iter()
                .find(|f| std::fs::symlink_metadata(output.join(f)).is_ok())
        {
            return Err(invalid(format_args!(
                "{} already exists at the destination and was not created by this app's engine; existing files are never overwritten",
                f.display()
            )));
        }
        // The record, before the engine sees the torrent. The same active transfer keeps its
        // counters; a record released earlier starts a new lifecycle (counters, completion,
        // share-limit stop and admission reset).
        let (hash, name, category, tags) = (
            t.hash.clone(),
            t.name.clone(),
            req.category.clone(),
            req.tags.clone(),
        );
        let (save, out, multi) = (self.cfg.save_path.clone(), output_s.clone(), t.multi);
        let ratio = req.ratio_limit.unwrap_or(-2.0);
        let minutes = req.seeding_time_limit_minutes.unwrap_or(-2);
        self.store
            .with(move |c| {
                c.execute(
                    "INSERT INTO embedded_torrents (info_hash, name, category, tags, save_path, output_folder, multi_file, ratio_limit, seeding_time_limit, added_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                     ON CONFLICT (info_hash) DO UPDATE SET name = excluded.name, category = excluded.category, tags = excluded.tags,
                       save_path = excluded.save_path, output_folder = excluded.output_folder, multi_file = excluded.multi_file,
                       ratio_limit = excluded.ratio_limit, seeding_time_limit = excluded.seeding_time_limit,
                       verified_at = NULL, verify_error = NULL, verify_token = NULL,
                       uploaded = CASE WHEN removed_at IS NULL THEN uploaded ELSE 0 END,
                       downloaded = CASE WHEN removed_at IS NULL THEN downloaded ELSE 0 END,
                       seeding_secs = CASE WHEN removed_at IS NULL THEN seeding_secs ELSE 0 END,
                       counters_valid = CASE WHEN removed_at IS NULL THEN counters_valid ELSE 1 END,
                       completed_at = CASE WHEN removed_at IS NULL THEN completed_at ELSE NULL END,
                       stopped_at_limit = CASE WHEN removed_at IS NULL THEN stopped_at_limit ELSE NULL END,
                       admitted_at = CASE WHEN removed_at IS NULL AND output_folder = excluded.output_folder THEN admitted_at ELSE NULL END,
                       added_at = CASE WHEN removed_at IS NULL THEN added_at ELSE excluded.added_at END,
                       removed_at = NULL, updated_at = excluded.updated_at",
                    params![hash, name, category, tags, save, out, multi, ratio, minutes, now_s()],
                )?;
                Ok(())
            })
            .await
            .map_err(db_failure)?;
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&t.hash);
        let opts = AddTorrentOptions {
            paused: req.paused,
            output_folder: Some(output_s),
            overwrite: trusted,
            initial_peers: peers,
            ..Default::default()
        };
        let answer = session
            .add_torrent(AddTorrent::from_bytes(bytes), Some(opts))
            .await;
        let hash = t.hash.clone();
        match answer {
            Ok(AddTorrentResponse::Added(..) | AddTorrentResponse::AlreadyManaged(..)) => {
                self.store
                    .with(move |c| {
                        c.execute(
                            "UPDATE embedded_torrents SET admitted_at = COALESCE(admitted_at, ?2) WHERE info_hash = ?1",
                            params![hash, now_s()],
                        )?;
                        Ok(())
                    })
                    .await
                    .map_err(db_failure)?;
                Ok(true)
            }
            Ok(AddTorrentResponse::ListOnly(_)) => Err(unavailable("unexpected list-only answer")),
            Err(e) => {
                if !trusted {
                    let _ = self
                        .store
                        .with(move |c| {
                            c.execute(
                                "DELETE FROM embedded_torrents WHERE info_hash = ?1",
                                [hash],
                            )?;
                            Ok(())
                        })
                        .await;
                }
                Err(invalid(format_args!(
                    "the engine refused it ({}); existing files at the destination are never overwritten",
                    failure_kind(&e)
                )))
            }
        }
    }

    /// Engine state and this app's records for the given hashes (absent hashes are omitted).
    pub async fn info(&self, hashes: &[String]) -> Result<Vec<TorrentInfo>, UpstreamError> {
        let session = self.start().await?;
        let _ops = self.ops.lock().await;
        let mut out = vec![];
        for hash in hashes {
            let hash = hash.to_ascii_lowercase();
            let Some(h) = self.handle(&session, &hash) else {
                continue;
            };
            out.push(self.observe(&session, &hash, &h).await?);
        }
        Ok(out)
    }

    /// One observation (callers hold `ops`): counters recorded from one reading of one live
    /// state, the share limit enforced, the client view built.
    async fn observe(
        &self,
        session: &Arc<Session>,
        hash: &str,
        h: &Arc<ManagedTorrent>,
    ) -> Result<TorrentInfo, UpstreamError> {
        let stats = h.stats();
        // The counters come from this owned live state only, never paired with another one.
        let live = h.live();
        let now = Instant::now();
        let d = {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            match &live {
                Some(l) => {
                    let s = l.stats_snapshot();
                    let cur = Sample {
                        uploaded: s.uploaded_bytes,
                        fetched: s.fetched_bytes,
                        finished: l.get_hns().is_some_and(|x| x.finished()),
                        at: now,
                    };
                    let any: Arc<dyn std::any::Any + Send + Sync> = l.clone();
                    let id = Arc::downgrade(&any);
                    let prev = seen
                        .get(hash)
                        .filter(|p| p.live.ptr_eq(&id))
                        .map(|p| (&p.sample, p.carry_ms));
                    let d = delta(prev, &cur);
                    seen.insert(
                        hash.to_string(),
                        Seen {
                            live: id,
                            sample: cur,
                            carry_ms: d.carry_ms,
                        },
                    );
                    d
                }
                None => {
                    seen.remove(hash);
                    delta(
                        None,
                        &Sample {
                            uploaded: 0,
                            fetched: 0,
                            finished: false,
                            at: now,
                        },
                    )
                }
            }
        };
        let row = self
            .record_counters(
                hash,
                d.uploaded,
                d.fetched,
                d.seeding_secs,
                d.consistent,
                stats.finished,
            )
            .await?;
        if let Some(r) = &row
            && stats.finished
            && !r.verified
            && r.verify_error.is_none()
        {
            self.verify_later(hash, h, &r.output_folder).await;
        }
        let paused_by_limit = match &row {
            Some(r) if self.limit_reached(r, &stats) => {
                session
                    .pause(h)
                    .await
                    .map_err(|e| engine_failure("pausing at the share limit", &e))?;
                let hs = hash.to_string();
                let _ = self
                    .store
                    .with(move |c| {
                        c.execute(
                            "UPDATE embedded_torrents SET stopped_at_limit = ?2, updated_at = ?2 WHERE info_hash = ?1",
                            params![hs, now_s()],
                        )?;
                        Ok(())
                    })
                    .await;
                self.seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(hash);
                true
            }
            _ => false,
        };
        Ok(self.client_view(hash, h, &stats, row.as_ref(), paused_by_limit))
    }

    /// Starts the one full piece check of a completed torrent (one at a time per torrent;
    /// callers hold `ops`). Its token is issued and bound to the record here, before anything
    /// runs, so a check never binds itself later to a different lifecycle; the spawned task
    /// only reads the pieces and records its result if its token is still the record's.
    async fn verify_later(&self, hash: &str, h: &ManagedTorrent, output_folder: &str) {
        let Ok(layout) = h.with_metadata(|m| Layout::of(&m.info)) else {
            return;
        };
        if self
            .verifying
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(hash)
        {
            return;
        }
        let Some(token) = bind_check(&self.store, hash).await else {
            return;
        };
        self.verifying
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(hash.to_string());
        let (store, set, hash, root) = (
            self.store.clone(),
            self.verifying.clone(),
            hash.to_string(),
            PathBuf::from(output_folder),
        );
        #[cfg(test)]
        let gate = self.check_gate.clone();
        let handle = tokio::spawn(async move {
            #[cfg(test)]
            let _gate = gate.read().await;
            let result = tokio::task::spawn_blocking(move || verify_pieces(&root, &layout))
                .await
                .unwrap_or_else(|_| Err("the check did not finish".into()));
            finish_check(&store, &hash, &token, result).await;
            set.lock().unwrap_or_else(|e| e.into_inner()).remove(&hash);
        });
        let mut checks = self.checks.lock().unwrap_or_else(|e| e.into_inner());
        checks.retain(|c| !c.is_finished());
        checks.push(handle);
    }

    async fn record_counters(
        &self,
        hash: &str,
        up: u64,
        down: u64,
        secs: u64,
        consistent: bool,
        finished: bool,
    ) -> Result<Option<Row>, UpstreamError> {
        let h = hash.to_string();
        let (up, down, secs) = (
            i64::try_from(up).unwrap_or(i64::MAX),
            i64::try_from(down).unwrap_or(i64::MAX),
            i64::try_from(secs).unwrap_or(i64::MAX),
        );
        self.store
            .with(move |c| {
                let now = now_s();
                c.execute(
                    "UPDATE embedded_torrents SET uploaded = uploaded + ?2, downloaded = downloaded + ?3, seeding_secs = seeding_secs + ?4,
                       counters_valid = counters_valid AND ?5, completed_at = CASE WHEN ?6 THEN COALESCE(completed_at, ?7) ELSE completed_at END,
                       updated_at = ?7 WHERE info_hash = ?1 AND (?2 > 0 OR ?3 > 0 OR ?4 > 0 OR NOT ?5 OR (?6 AND completed_at IS NULL))",
                    params![h, up, down, secs, consistent, finished, now],
                )?;
                Ok(())
            })
            .await
            .map_err(db_failure)?;
        self.row(hash).await
    }

    fn ratio(r: &Row, stats: &TorrentStats) -> Option<f64> {
        // Never above a full download's ratio: the denominator is at least the content size.
        let denom = (r.downloaded.max(0) as u64).max(stats.total_bytes);
        (r.counters_valid && denom > 0).then(|| r.uploaded.max(0) as f64 / denom as f64)
    }

    fn limit_reached(&self, r: &Row, stats: &TorrentStats) -> bool {
        if !stats.finished || !matches!(stats.state, TorrentStatsState::Live) || !r.counters_valid {
            return false;
        }
        let ratio = effective(r.ratio_limit, self.cfg.ratio_limit, -2.0);
        let minutes = effective(
            r.seeding_time_limit,
            self.cfg.seeding_time_limit_minutes,
            -2,
        );
        let ratio_met = ratio
            .zip(Self::ratio(r, stats))
            .is_some_and(|(l, v)| v >= l);
        let time_met = minutes.is_some_and(|m| r.seeding_secs >= m.saturating_mul(60));
        ratio_met || time_met
    }

    fn client_view(
        &self,
        hash: &str,
        h: &ManagedTorrent,
        stats: &TorrentStats,
        row: Option<&Row>,
        paused_by_limit: bool,
    ) -> TorrentInfo {
        let total = stats.total_bytes;
        let have = stats.progress_bytes.min(total);
        let finished = stats.finished;
        let live = stats.live.as_ref();
        let dl = live.map(|l| l.download_speed.as_bytes()).unwrap_or(0);
        let peers = live.map(|l| l.snapshot.peer_stats.live).unwrap_or(0);
        // The qBittorrent state vocabulary, from the engine's real state.
        let unverified = finished && row.is_some_and(|r| !r.verified);
        let state = match stats.state {
            TorrentStatsState::Initializing { .. } => "checkingDL",
            TorrentStatsState::Error => "error",
            _ if row.is_some_and(|r| r.verify_error.is_some()) => "error",
            _ if unverified => "checkingUP",
            _ if paused_by_limit => "pausedUP",
            TorrentStatsState::Paused if finished => "pausedUP",
            TorrentStatsState::Paused => "pausedDL",
            TorrentStatsState::Live if finished => "uploading",
            TorrentStatsState::Live if peers == 0 && dl == 0 => "stalledDL",
            TorrentStatsState::Live => "downloading",
        };
        let progress = if total > 0 {
            have as f64 / total as f64
        } else {
            0.0
        };
        let left = total - have;
        let name = row
            .map(|r| r.name.clone())
            .or_else(|| h.name())
            .unwrap_or_default();
        let save_path = row
            .map(|r| r.save_path.clone())
            .unwrap_or_else(|| self.cfg.save_path.clone());
        let content_path = row.map(|r| {
            if r.multi_file {
                r.output_folder.clone()
            } else {
                format!("{}/{}", r.save_path, r.name)
            }
        });
        let valid = row.filter(|r| r.counters_valid && !r.removed);
        let ratio_limit = row.map(|r| r.ratio_limit);
        let time_limit = row.map(|r| r.seeding_time_limit);
        TorrentInfo {
            hash: hash.to_string(),
            name,
            category: row.map(|r| r.category.clone()).unwrap_or_default(),
            tags: row.map(|r| r.tags.clone()).unwrap_or_default(),
            state: state.into(),
            progress,
            size: total as i64,
            amount_left: left as i64,
            save_path,
            dlspeed: dl as i64,
            eta: if finished {
                0
            } else if dl > 0 {
                (left / dl) as i64
            } else {
                8_640_000
            },
            content_path,
            ratio: valid.and_then(|r| Self::ratio(r, stats)),
            ratio_limit,
            max_ratio: ratio_limit
                .map(|l| effective(l, self.cfg.ratio_limit, -2.0).unwrap_or(-1.0)),
            seeding_time: valid.map(|r| r.seeding_secs),
            seeding_time_limit: time_limit,
            max_seeding_time: time_limit
                .map(|l| effective(l, self.cfg.seeding_time_limit_minutes, -2).unwrap_or(-1)),
            downloaded: valid.map(|r| r.downloaded),
            uploaded: valid.map(|r| r.uploaded),
        }
    }

    /// The torrent's files, named relative to the save path as qBittorrent names them
    /// (`<name>/<path>` for multi-file torrents). Padding entries are not content.
    pub async fn files(&self, hash: &str) -> Result<Option<Vec<TorrentFile>>, UpstreamError> {
        let session = self.start().await?;
        let hash = hash.to_ascii_lowercase();
        let Some(h) = self.handle(&session, &hash) else {
            return Ok(None);
        };
        let Some(row) = self.row(&hash).await? else {
            return Ok(None);
        };
        let stats = h.stats();
        let only = h.only_files();
        let infos = h
            .with_metadata(|m| m.file_infos.clone())
            .map_err(|e| engine_failure("reading the torrent's metadata", &e))?;
        let mut out = vec![];
        for (i, f) in infos.iter().enumerate() {
            if f.attrs.padding {
                continue;
            }
            let rel = f.relative_filename.to_string_lossy().to_string();
            let name = if row.multi_file {
                format!("{}/{rel}", row.name)
            } else {
                rel
            };
            let have = stats.file_progress.get(i).copied().unwrap_or(0);
            out.push(TorrentFile {
                index: Some(i as i64),
                name,
                size: f.len as i64,
                progress: if f.len == 0 {
                    1.0
                } else {
                    have.min(f.len) as f64 / f.len as f64
                },
                priority: i64::from(only.as_ref().is_none_or(|o| o.contains(&i))),
            });
        }
        Ok(Some(out))
    }

    async fn our_handle(
        &self,
        hash: &str,
    ) -> Result<(Arc<Session>, Arc<ManagedTorrent>), UpstreamError> {
        let session = self.start().await?;
        let h = self
            .handle(&session, &hash.to_ascii_lowercase())
            .ok_or_else(|| unavailable("no such torrent"))?;
        Ok((session, h))
    }

    pub async fn pause(&self, hash: &str) -> Result<(), UpstreamError> {
        let (session, h) = self.our_handle(hash).await?;
        let _ops = self.ops.lock().await;
        // Record what the live state counted before pausing ends it.
        self.observe(&session, &hash.to_ascii_lowercase(), &h)
            .await?;
        if h.is_paused() {
            return Ok(());
        }
        session
            .pause(&h)
            .await
            .map_err(|e| engine_failure("pausing", &e))
    }

    pub async fn resume(&self, hash: &str) -> Result<(), UpstreamError> {
        let (session, h) = self.our_handle(hash).await?;
        let _ops = self.ops.lock().await;
        if !h.is_paused() {
            return Ok(());
        }
        session
            .unpause(&h)
            .await
            .map_err(|e| engine_failure("resuming", &e))
    }

    /// Removes the torrent from the engine. Data is never deleted here, whatever
    /// `delete_files` says: the app's journaled removal deletes exactly the tracked files.
    pub async fn delete(&self, hash: &str, _delete_files: bool) -> Result<(), UpstreamError> {
        let hash = hash.to_ascii_lowercase();
        let session = self.start().await?;
        let _ops = self.ops.lock().await;
        let Some(h) = self.handle(&session, &hash) else {
            return Ok(());
        };
        self.observe(&session, &hash, &h).await?;
        session
            .delete(TorrentIdOrHash::Hash(h.info_hash()), false)
            .await
            .map_err(|e| engine_failure("removing", &e))?;
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&hash);
        let hs = hash.clone();
        self.store
            .with(move |c| {
                c.execute(
                    "UPDATE embedded_torrents SET removed_at = ?2, verify_token = NULL, updated_at = ?2 WHERE info_hash = ?1",
                    params![hs, now_s()],
                )?;
                Ok(())
            })
            .await
            .map_err(db_failure)
    }

    pub async fn set_category(&self, hash: &str, category: &str) -> Result<(), UpstreamError> {
        let (h, cat) = (hash.to_ascii_lowercase(), category.to_string());
        self.store
            .with(move |c| {
                c.execute(
                    "UPDATE embedded_torrents SET category = ?2, updated_at = ?3 WHERE info_hash = ?1",
                    params![h, cat, now_s()],
                )?;
                Ok(())
            })
            .await
            .map_err(db_failure)
    }

    /// The configured share limits, in qBittorrent's global-preference shape.
    pub fn preferences(&self) -> Preferences {
        Preferences {
            max_ratio_enabled: Some(self.cfg.ratio_limit.is_some()),
            max_ratio: self.cfg.ratio_limit,
            max_seeding_time_enabled: Some(self.cfg.seeding_time_limit_minutes.is_some()),
            max_seeding_time: self.cfg.seeding_time_limit_minutes,
        }
    }

    pub async fn properties(&self, hash: &str) -> Result<Option<Properties>, UpstreamError> {
        let t = self.info(&[hash.to_string()]).await?;
        Ok(t.into_iter().next().map(|t| Properties {
            share_ratio: t.ratio,
            seeding_time: t.seeding_time,
            total_downloaded: t.downloaded,
            total_uploaded: t.uploaded,
            save_path: Some(t.save_path),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(unix)]
    fn layout(torrent: &[u8]) -> Layout {
        let t = librqbit::torrent_from_bytes(torrent).unwrap();
        Layout::of(&t.info.data.validate().unwrap())
    }

    #[cfg(unix)] // makes symlinks
    #[test]
    fn the_piece_check_accepts_exact_data_and_rejects_any_difference() {
        // Files spanning several pieces and a piece boundary between files.
        let a: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let b: Vec<u8> = (0..9_000u32).map(|i| (i % 13) as u8).collect();
        let files = vec![
            ("Show S01/a.mkv".to_string(), a),
            ("Show S01/b.mkv".to_string(), b),
        ];
        let t = crate::fixture::build_torrent("Show S01", &files);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Show S01");
        std::fs::create_dir_all(root.join("Show S01")).unwrap();
        let l = layout(&t.torrent);
        let write = |name: &str, bytes: &[u8]| std::fs::write(root.join(name), bytes).unwrap();
        // The engine writes multi-file content below output_folder without the torrent name.
        for (rel, _, _) in &l.files {
            let src = &files
                .iter()
                .find(|(p, _)| p.ends_with(&*rel.to_string_lossy()))
                .unwrap()
                .1;
            write(&rel.to_string_lossy(), src);
        }
        verify_pieces(&root, &l).unwrap();
        // One flipped byte.
        let mut bad = files[0].1.clone();
        bad[20_000] ^= 1;
        write(&l.files[0].0.to_string_lossy(), &bad);
        assert!(
            verify_pieces(&root, &l)
                .unwrap_err()
                .contains("does not match")
        );
        // A longer file, even with an intact prefix.
        let mut longer = files[0].1.clone();
        longer.push(0);
        write(&l.files[0].0.to_string_lossy(), &longer);
        assert!(verify_pieces(&root, &l).unwrap_err().contains("bytes"));
        // A missing file.
        std::fs::remove_file(root.join(&l.files[0].0)).unwrap();
        assert!(verify_pieces(&root, &l).is_err());
        // A symlink in place of the file, even to the right bytes.
        let elsewhere = tmp.path().join("copy.mkv");
        std::fs::write(&elsewhere, &files[0].1).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(&l.files[0].0)).unwrap();
        assert!(verify_pieces(&root, &l).is_err());
    }

    #[test]
    fn readings_add_only_what_one_live_state_counted() {
        let t0 = Instant::now();
        let s = |uploaded, fetched, finished, ms| Sample {
            uploaded,
            fetched,
            finished,
            at: t0 + std::time::Duration::from_millis(ms),
        };
        // A live state not read before: everything it counted is new.
        assert_eq!(
            delta(None, &s(100, 50, true, 0)),
            Delta {
                uploaded: 100,
                fetched: 50,
                seeding_secs: 0,
                carry_ms: 0,
                consistent: true
            }
        );
        // The same live state: only its growth; seeding time adds up across sub-second
        // readings through the carry.
        let (a, b, c) = (
            s(100, 50, true, 0),
            s(130, 50, true, 600),
            s(130, 50, true, 1200),
        );
        let d1 = delta(Some((&a, 0)), &b);
        assert_eq!((d1.uploaded, d1.seeding_secs, d1.carry_ms), (30, 0, 600));
        let d2 = delta(Some((&b, d1.carry_ms)), &c);
        assert_eq!((d2.uploaded, d2.seeding_secs, d2.carry_ms), (0, 1, 200));
        // No seeding time unless both readings saw it complete.
        assert_eq!(
            delta(Some((&s(0, 0, false, 0), 0)), &s(0, 10, true, 5000)).seeding_secs,
            0
        );
        // A counter that went backwards is not trusted, and adds nothing.
        let d = delta(Some((&b, 0)), &s(10, 50, true, 2000));
        assert!(!d.consistent && d.uploaded == 0);
    }

    #[test]
    fn engine_failures_keep_only_stage_and_kind() {
        let canary = "CANARY-PASSKEY-41";
        let e = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
            .context(format!(
                "error announcing to http://tracker.example/{canary}/announce"
            ));
        let text = engine_failure("announcing", &e).describe();
        assert!(
            text.contains("announcing failed") && text.contains("ConnectionRefused"),
            "{text}"
        );
        assert!(
            !text.contains(canary) && !text.contains("tracker.example"),
            "{text}"
        );
        let opaque = anyhow::anyhow!("magnet:?xt=urn:btih:{canary}");
        assert!(
            !engine_failure("adding", &opaque)
                .describe()
                .contains(canary)
        );
    }

    #[test]
    fn unsafe_or_symlinked_torrents_are_refused_before_anything_is_written() {
        let ok = crate::fixture::build_torrent(
            "Film.2025.mkv",
            &[("Film.2025.mkv".into(), vec![1; 100])],
        );
        let i = inspect(&ok.torrent).unwrap();
        assert_eq!((i.hash.as_str(), i.multi), (ok.info_hash.as_str(), false));
        // Path traversal inside a multi-file torrent, and an unsafe directory name.
        let traversal = crate::fixture::build_torrent(
            "Pack",
            &[
                ("Pack/../../etc/x".into(), vec![1; 10]),
                ("Pack/b".into(), vec![2; 10]),
            ],
        );
        assert!(inspect(&traversal.torrent).is_err());
        let bad_name = crate::fixture::build_torrent(
            "..",
            &[("../a".into(), vec![1; 10]), ("../b".into(), vec![2; 10])],
        );
        assert!(inspect(&bad_name.torrent).is_err());
        // A BEP-47 symlink entry.
        let at = ok
            .torrent
            .windows(13)
            .position(|w| w == b"6:lengthi100e")
            .unwrap();
        let mut link = ok.torrent[..at].to_vec();
        link.extend_from_slice(b"4:attr1:l");
        link.extend_from_slice(&ok.torrent[at..]);
        assert!(inspect(&link).unwrap_err().contains("symlink"));
        assert!(inspect(b"not a torrent").is_err());
    }

    /// An engine over a fresh state directory, reached by nobody (no peers, trackers, DHT or
    /// local discovery).
    fn quiet_engine(root: &Path) -> (Embedded, Store, EmbeddedConfig) {
        let downloads = root.join("downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        let store = Store::open(&root.join("state.sqlite")).unwrap();
        let cfg = EmbeddedConfig {
            save_path: downloads.to_string_lossy().into(),
            download_dir: downloads,
            session_dir: root.join("embedded"),
            listen: "127.0.0.1:0".parse().unwrap(),
            trackers: false,
            dht: false,
            local_discovery: false,
            initial_peers: vec![],
            peer_limit: None,
            upload_bps: None,
            download_bps: None,
            max_active_downloads: 3,
            ratio_limit: None,
            seeding_time_limit_minutes: None,
            metadata_timeout: Duration::from_secs(5),
            movie_category: None,
            tv_category: None,
        };
        (Embedded::new(cfg.clone(), store.clone()), store, cfg)
    }

    /// `(verify_token, verified, verify_error)` of a record.
    fn check_of(store: &Store, hash: &str) -> (Option<String>, bool, Option<String>) {
        let h = hash.to_string();
        store
            .blocking(|c| {
                Ok(c.query_row(
                    "SELECT verify_token, verified_at IS NOT NULL, verify_error FROM embedded_torrents WHERE info_hash = ?1",
                    [h],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?)
            })
            .unwrap()
    }

    #[tokio::test]
    async fn a_check_is_bound_before_it_runs_and_a_superseded_one_records_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (e, store, cfg) = quiet_engine(tmp.path());
        let name = "Bound.Check.2025.1080p.WEB-DL.x264-UM.mkv";
        let data: Vec<u8> = (0..60_000u32).map(|i| (i % 241) as u8).collect();
        let t = crate::fixture::build_torrent(name, &[(name.into(), data.clone())]);
        let hash = t.info_hash.clone();
        // An admitted, active record of this torrent with its complete file in place, as the
        // engine itself left them: the engine finds every piece and the transfer completes.
        let (h, out) = (hash.clone(), cfg.save_path.clone());
        store
            .blocking(|c| {
                c.execute(
                    "INSERT INTO embedded_torrents (info_hash, name, category, tags, save_path, output_folder, multi_file, ratio_limit, seeding_time_limit, admitted_at, added_at, updated_at)
                     VALUES (?1, ?2, 'um-movies', 'unified-media', ?3, ?3, 0, -2.0, -2, 'x', 'x', 'x')",
                    params![h, name, out],
                )?;
                Ok(())
            })
            .unwrap();
        std::fs::write(cfg.download_dir.join(name), &data).unwrap();
        let mut req = AddRequest {
            payload: Payload::Torrent(t.torrent.clone()),
            save_path: cfg.save_path.clone(),
            category: "um-movies".into(),
            tags: "unified-media".into(),
            paused: false,
            ratio_limit: None,
            seeding_time_limit_minutes: None,
            sequential: false,
            first_last_piece: false,
            content_layout: None,
        };
        // Spawned checks wait for this gate, so nothing below can race them.
        let gate = e.check_gate.clone().write_owned().await;
        assert!(e.add(&req).await.unwrap());
        // Observations before the engine reports the transfer finished bind nothing, even when
        // its initial check has already counted every byte (`checkingDL` at progress 1.0). The
        // observation that first sees it finished (`checkingUP`: finished, unverified) has
        // bound its check by the time it returns, and the check's task has not run.
        let started = Instant::now();
        loop {
            let i = e.info(std::slice::from_ref(&hash)).await.unwrap();
            if i[0].state == "checkingUP" {
                assert!(i[0].progress >= 1.0, "{:?}", i[0]);
                break;
            }
            assert_eq!(check_of(&store, &hash), (None, false, None), "{:?}", i[0]);
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "never completed: {:?}",
                i[0]
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (first, verified, error) = check_of(&store, &hash);
        let first = first.expect("bound before its task ran");
        assert!(!verified && error.is_none());
        let pending: Vec<_> = std::mem::take(&mut *e.checks.lock().unwrap());
        assert_eq!(pending.len(), 1, "one check, started once");
        assert!(e.verifying.lock().unwrap().contains(&hash));
        // Observed again while it waits: still one check, the same token, nothing recorded.
        e.info(std::slice::from_ref(&hash)).await.unwrap();
        assert!(e.checks.lock().unwrap().is_empty(), "no second check");
        assert_eq!(check_of(&store, &hash), (Some(first.clone()), false, None));
        // Released, the check reads the pieces and records its result under its own token.
        drop(gate);
        for h in pending {
            h.await.unwrap();
        }
        assert_eq!(check_of(&store, &hash), (Some(first.clone()), true, None));
        assert!(!e.verifying.lock().unwrap().contains(&hash));
        // Removal invalidates the check: a result arriving afterwards records nothing.
        e.delete(&hash, false).await.unwrap();
        assert_eq!(check_of(&store, &hash).0, None);
        assert!(!finish_check(&store, &hash, &first, Ok(())).await);
        // Acquired again (its data went with the journaled removal): a new lifecycle, not
        // complete. The old check can neither bless nor fail it, and nothing binds a new
        // check before this transfer completes.
        std::fs::remove_file(cfg.download_dir.join(name)).unwrap();
        req.paused = true;
        assert!(e.add(&req).await.unwrap());
        e.info(std::slice::from_ref(&hash)).await.unwrap();
        assert!(!finish_check(&store, &hash, &first, Ok(())).await);
        assert!(!finish_check(&store, &hash, &first, Err("stale".into())).await);
        assert_eq!(check_of(&store, &hash), (None, false, None));
        assert!(bind_check(&store, &hash).await.is_none());
        e.shutdown().await;
    }
}
