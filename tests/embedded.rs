//! The embedded download engine (librqbit, in the app) against real BitTorrent peers on
//! loopback. Authored torrents over real playable media; a controlled seeding peer and
//! controlled leechers, each its own librqbit session in this process, reached only through
//! explicit addresses (no trackers, DHT or local discovery). Real bytes over TCP, hash-checked
//! completion, import, seeding to the normal goal, cleanup and restarts. No qBittorrent
//! configuration, mock or API exists in these tests.
#![recursion_limit = "256"]

mod common;

use arr::fixture::TorrentFixture;
use arr::{api, config, fsutil};
use common::*;
use librqbit::{
    AddTorrent, AddTorrentOptions, ListenerMode, ListenerOptions, ManagedTorrent, Session,
    SessionOptions,
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A controlled peer: a librqbit session on 127.0.0.1 with nothing but explicit peers.
struct Peer {
    session: Arc<Session>,
    dir: PathBuf,
}

impl Peer {
    async fn start(dir: &Path) -> Peer {
        std::fs::create_dir_all(dir).unwrap();
        let session = Session::new_with_opts(
            dir.to_path_buf(),
            SessionOptions {
                dht: None,
                disable_trackers: true,
                listen: Some(ListenerOptions {
                    mode: ListenerMode::TcpOnly,
                    listen_addr: "127.0.0.1:0".parse().unwrap(),
                    ipv4_only: true,
                    ..Default::default()
                }),
                disable_local_service_discovery: true,
                ipv4_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        Peer {
            session,
            dir: dir.to_path_buf(),
        }
    }

    fn addr(&self) -> SocketAddr {
        self.session.listen_addr().unwrap()
    }

    /// Seeds `t` from its files (checked by this peer's engine before it serves them).
    async fn seed(&self, t: &TorrentFixture, files: &[(String, Vec<u8>)]) -> Arc<ManagedTorrent> {
        let out = self.dir.join(&t.info_hash);
        for (rel, bytes) in files {
            let p = out.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
        }
        let h = self
            .session
            .add_torrent(
                AddTorrent::from_bytes(t.torrent.clone()),
                Some(AddTorrentOptions {
                    output_folder: Some(out.to_string_lossy().into()),
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), h.wait_until_completed())
            .await
            .unwrap()
            .unwrap();
        h
    }

    /// Downloads `t` from `from` only, into its own folder.
    async fn leech(&self, t: &TorrentFixture, from: SocketAddr, tag: &str) -> Arc<ManagedTorrent> {
        let out = self.dir.join(format!("leech-{tag}"));
        let h = self
            .session
            .add_torrent(
                AddTorrent::from_bytes(t.torrent.clone()),
                Some(AddTorrentOptions {
                    output_folder: Some(out.to_string_lossy().into()),
                    initial_peers: Some(vec![from]),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .into_handle()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), h.wait_until_completed())
            .await
            .expect("the leecher completed from the app's engine")
            .unwrap();
        h
    }

    async fn forget(&self, h: &ManagedTorrent) {
        self.session
            .delete(librqbit::api::TorrentIdOrHash::Id(h.id()), false)
            .await
            .unwrap();
    }
}

/// A peer that connects to the app's engine, claims every piece of `t` and never unchokes
/// it: connected, and delivering nothing. Raw BitTorrent wire protocol over loopback.
async fn silent_peer(to: SocketAddr, t: &TorrentFixture) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let hash = hex::decode(&t.info_hash).unwrap();
    let mut s = tokio::net::TcpStream::connect(to).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend_from_slice(b"BitTorrent protocol");
    hs.extend_from_slice(&[0; 8]);
    hs.extend_from_slice(&hash);
    hs.extend_from_slice(b"-UM0001-silentpeer01");
    s.write_all(&hs).await.unwrap();
    let mut answer = [0u8; 68];
    s.read_exact(&mut answer).await.unwrap();
    assert_eq!(&answer[28..48], &hash[..], "the engine took the connection");
    // A bitfield with every piece (the fixture's pieces are 16 KiB).
    let pieces = t.size.div_ceil(16 * 1024) as usize;
    let mut bits = vec![0u8; pieces.div_ceil(8)];
    for i in 0..pieces {
        bits[i / 8] |= 0x80 >> (i % 8);
    }
    let mut msg = ((bits.len() + 1) as u32).to_be_bytes().to_vec();
    msg.push(5);
    msg.extend_from_slice(&bits);
    s.write_all(&msg).await.unwrap();
    tokio::spawn(async move {
        let (mut r, mut w) = s.into_split();
        let drain = async {
            let mut buf = [0u8; 4096];
            while matches!(r.read(&mut buf).await, Ok(n) if n > 0) {}
        };
        let keep_alive = async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if w.write_all(&[0, 0, 0, 0]).await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            _ = drain => {}
            _ = keep_alive => {}
        }
    })
}

struct Opts {
    tmdb: bool,
    download_limit: Option<u32>,
    max_active: u32,
    stall_minutes: Option<u64>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            tmdb: false,
            download_limit: None,
            max_active: 3,
            stall_minutes: None,
        }
    }
}

struct Emb {
    base: String,
    client: reqwest::Client,
    state: Arc<api::AppState>,
    server: tokio::task::JoinHandle<()>,
    dir: PathBuf,
    downloads: PathBuf,
    movies: PathBuf,
    tv: PathBuf,
    mock: Mock,
    mock_addr: SocketAddr,
    seeder: Peer,
    _tmp: tempfile::TempDir,
}

async fn serve(dir: &Path) -> (Arc<api::AppState>, String, tokio::task::JoinHandle<()>) {
    let cfg = config::load(&dir.join("config.toml")).unwrap();
    let state = Arc::new(api::AppState::new(cfg).unwrap());
    arr::worker::recover(&state, time::OffsetDateTime::now_utc()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s2 = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, api::router(s2)).await.unwrap();
    });
    (state, format!("http://{addr}"), server)
}

async fn start(o: Opts) -> Emb {
    let (mock_addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let (dir, downloads) = (root.join("state"), root.join("downloads"));
    let (movies, tv) = (root.join("library/movies"), root.join("library/tv"));
    for d in [&movies, &tv] {
        std::fs::create_dir_all(d).unwrap();
    }
    let seeder = Peer::start(&root.join("seeder")).await;
    fsutil::ensure_private_dir(&dir).unwrap();
    fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
    fsutil::write_new_private_file(&dir.join("secrets/tmdb"), TMDB_V3_KEY.as_bytes()).unwrap();
    let tmdb = if o.tmdb {
        format!(
            "[tmdb]\nenabled = true\nbase_url = \"http://{mock_addr}/tmdb\"\ncredential_file = \"secrets/tmdb\"\n"
        )
    } else {
        String::new()
    };

    let limit = o
        .download_limit
        .map(|l| format!("download_limit_bytes_per_sec = {l}\n"))
        .unwrap_or_default();
    let extra = format!(
        "[execution]\nenabled = true\ndownload_cleanup = true\n\
         [embedded]\ndownload_dir = '{dl}'\nlisten = \"127.0.0.1:0\"\ntrackers = false\ndht = false\nlocal_discovery = false\ninitial_peers = [\"{seed}\"]\nmax_active_downloads = {max}\n{limit}\
         [migration]\nmode = \"coexistence\"\n\
         [library]\nmovies = '{movies}'\ntv = '{tv}'\nsample_detection = false\nprobe_timeout_secs = 30\n\
         [automation]\npoll_seconds = 3600\n{stall}{tmdb}",
        dl = downloads.display(),
        seed = seeder.addr(),
        max = o.max_active,
        movies = movies.display(),
        tv = tv.display(),
        stall = o
            .stall_minutes
            .map(|m| format!("stall_timeout_minutes = {m}\n"))
            .unwrap_or_default(),
    );
    write_managed_config(&dir, mock_addr, true, &extra);
    let (state, base, server) = serve(&dir).await;
    let client = reqwest::Client::new();
    setup_managed(&base, &client).await;
    seed_standard_library(&base, &client, &movies, &tv, true).await;
    mock.lock().unwrap().log.clear();
    Emb {
        base,
        client: reqwest::Client::new(),
        state,
        server,
        dir,
        downloads,
        movies,
        tv,
        mock,
        mock_addr,
        seeder,
        _tmp: tmp,
    }
}

impl Emb {
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
    }

    async fn json(&self, rb: reqwest::RequestBuilder) -> (u16, Value, String) {
        let r = rb.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
        )
    }

    fn engine(&self) -> &arr::embedded::Embedded {
        self.state.client.as_ref().unwrap().embedded().unwrap()
    }

    async fn tick(&self) {
        self.tick_at(time::OffsetDateTime::now_utc()).await;
    }

    async fn tick_at(&self, now: time::OffsetDateTime) {
        arr::worker::tick(&self.state, now).await.unwrap();
    }

    fn grab(&self, id: &str) -> arr::downloads::Grab {
        let id = id.to_string();
        self.state
            .store
            .blocking(|c| arr::downloads::get(c, &id))
            .unwrap()
            .unwrap()
    }

    /// Worker passes until `done` holds for the grab (bounded).
    async fn until(&self, id: &str, what: &str, done: impl Fn(&arr::downloads::Grab) -> bool) {
        let started = Instant::now();
        loop {
            self.tick().await;
            let g = self.grab(id);
            if done(&g) {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(90),
                "{what}: state {} cleanup {} client {:?} error {:?} detail {:?}",
                g.state,
                g.cleanup,
                g.client_state,
                g.last_error,
                g.cleanup_detail
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// An authored release: seeded by the controlled peer, its `.torrent` served by the
    /// Prowlarr mock's download proxy (or only a magnet, whose metadata comes from the peer).
    async fn publish(
        &self,
        title: &str,
        files: Vec<(String, Vec<u8>)>,
        gib: f64,
        magnet_only: bool,
    ) -> (TorrentFixture, Value) {
        let t = arr::fixture::build_torrent(title, &files);
        self.seeder.seed(&t, &files).await;
        self.mock
            .lock()
            .unwrap()
            .torrents
            .insert(t.info_hash.clone(), t.torrent.clone());
        let mut r = release(title, gib, 50);
        r["guid"] = json!(format!("fixture-{}", t.info_hash));
        r["infoHash"] = json!(t.info_hash);
        if magnet_only {
            r["downloadUrl"] = Value::Null;
            r["magnetUrl"] = json!(t.magnet);
        } else {
            r["downloadUrl"] = json!(format!(
                "http://{}/prowlarr/1/download?apikey={PROWLARR_KEY}&link={}",
                self.mock_addr, t.info_hash
            ));
            r["magnetUrl"] = Value::Null;
        }
        (t, r)
    }

    fn search_results(&self, query: &str, releases: Vec<Value>) {
        self.mock
            .lock()
            .unwrap()
            .releases
            .insert(query.into(), releases);
    }

    /// The app's lifetime counters for a torrent: (uploaded, downloaded, seeding seconds,
    /// valid, verified).
    fn counters(&self, hash: &str) -> (i64, i64, i64, bool, bool) {
        let h = hash.to_string();
        self.state
            .store
            .blocking(|c| {
                Ok(c.query_row(
                    "SELECT uploaded, downloaded, seeding_secs, counters_valid, verified_at IS NOT NULL FROM embedded_torrents WHERE info_hash = ?1",
                    [h],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )?)
            })
            .unwrap()
    }

    /// Stops the engine (gracefully, or abruptly as a crash would: its tasks cancelled with
    /// nothing flushed or recorded) and starts a new process state over the same directory.
    async fn restart(&mut self, graceful: bool) {
        self.restart_with(graceful, || {}).await;
    }

    /// As `restart`, running `between` while nothing runs. The old process state is let go of
    /// entirely, as a dead process would: its server, the client's pooled connections (each
    /// holds the state) and the state itself, whose engine lock goes with it.
    async fn restart_with(&mut self, graceful: bool, between: impl FnOnce()) {
        if graceful {
            self.engine().shutdown().await;
        } else if let Some(s) = self.engine().session() {
            s.cancellation_token().cancel();
        }
        self.server.abort();
        self.client = reqwest::Client::new();
        tokio::time::sleep(Duration::from_millis(300)).await;
        between();
        let (state, base, server) = serve(&self.dir).await;
        (self.state, self.base, self.server) = (state, base, server);
        let started = Instant::now();
        while let Err(err) = self.engine().start().await {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{}",
                err.describe()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn app_addr(&self) -> SocketAddr {
        self.engine().start().await.unwrap();
        self.engine().listen_addr().unwrap()
    }
}

/// The engine's progress and state for one torrent, as the app sees it.
async fn progress(e: &Emb, hash: &str) -> Option<(f64, String)> {
    e.engine()
        .info(&[hash.to_string()])
        .await
        .unwrap()
        .first()
        .map(|t| (t.progress, t.state.clone()))
}

fn movie_files(stem: &str) -> Vec<(String, Vec<u8>)> {
    vec![
        (format!("{stem}.mkv"), mkv(3)),
        (format!("Sample/{}.sample.mkv", stem.to_lowercase()), mkv(1)),
        (format!("{stem}.nfo"), b"nfo".to_vec()),
    ]
}

#[tokio::test]
async fn a_new_movie_arrives_over_bittorrent_imports_and_seeds_to_ratio_two_across_restarts() {
    let mut e = start(Opts {
        tmdb: true,
        ..Default::default()
    })
    .await;
    let title = "Leviticus.2025.1080p.WEB-DL.x264-UM";
    let (t, r) = e.publish(title, movie_files(title), 4.0, false).await;
    e.search_results("Leviticus 2025", vec![r]);

    // The ordinary one-call add of a title new to the native apps.
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "movie:tmdb:900001" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    let g = e.grab(&id);
    assert_eq!(
        (g.category.as_str(), g.seed_ratio),
        ("um-movies", Some(2.0))
    );
    assert_eq!(g.save_path, arr::paths::client_form(&e.downloads));

    // Real bytes from the controlled peer, every piece checked, then the normal import.
    e.until(&id, "imported", |g| g.state == "imported").await;
    let seeded = e
        .seeder
        .session
        .with_torrents(|it| it.map(|(_, h)| h.stats().uploaded_bytes).sum::<u64>());
    assert!(
        seeded >= t.size,
        "the peer uploaded {seeded} of {} bytes",
        t.size
    );
    let (_, downloaded, _, valid, verified) = e.counters(&t.info_hash);
    assert!(
        valid && verified && downloaded as u64 >= t.size,
        "{downloaded}"
    );
    let dest = e.movies.join(format!("Leviticus (2025)/{title}.mkv"));
    let source = e.downloads.join(format!("{title}/{title}.mkv"));
    assert!(dest.is_file() && playable(&dest).await);
    assert_eq!(
        ino(&dest),
        ino(&source),
        "hardlinked from the engine's download"
    );
    assert!(
        !e.movies
            .join("Leviticus (2025)")
            .join(format!("Sample/{}.sample.mkv", title.to_lowercase()))
            .exists()
    );
    assert_eq!(e.grab(&id).cleanup, "seeding");

    // The backend is reported truthfully; nothing names or needs qBittorrent.
    let (_, d, _) = e.json(e.get("/api/v1/diagnostics")).await;
    assert_eq!(d["download_client"]["backend"], "embedded");
    assert_eq!(d["qbittorrent"]["status"], "not_configured");
    assert_eq!(d["embedded"]["status"], "running", "{d}");
    assert_eq!(
        (
            d["embedded"]["dht"].as_bool(),
            d["embedded"]["upnp"].as_bool()
        ),
        (Some(false), Some(true)),
        "UPnP is on by default (it only maps a non-loopback listener)"
    );
    assert_eq!(d["embedded"]["transport"], "tcp+utp");
    assert_eq!(
        d["embedded"]["ipv6"], false,
        "a loopback listener stays IPv4"
    );
    assert!(d["embedded"]["connections"]["utp"]["v4"]["attempts"].is_u64());
    let (_, caps, _) = e.json(e.get("/api/v1/capabilities")).await;
    let names: Vec<&str> = caps["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"embedded_download_engine")
            && names.contains(&"embedded_download_submission")
    );
    assert!(
        !names.contains(&"download_submission"),
        "qBittorrent evidence never counts here"
    );

    // Only the app's engine serves the leechers from here on.
    let seeding = e
        .seeder
        .session
        .with_torrents(|it| it.map(|(_, h)| h.clone()).collect::<Vec<_>>());
    for h in &seeding {
        e.seeder.forget(h).await;
    }
    let leechers = Peer::start(&e._tmp.path().join("leechers")).await;
    let first = leechers.leech(&t, e.app_addr().await, "1").await;
    leechers.forget(&first).await;
    e.tick().await;
    let (up1, ..) = e.counters(&t.info_hash);
    assert!(up1 as u64 >= t.size, "uploaded {up1}");
    // A graceful restart keeps the lifetime counters; the engine's own restart from zero.
    e.restart(true).await;
    e.tick().await;
    let (up2, ..) = e.counters(&t.info_hash);
    assert_eq!(up2, up1, "persisted, not reset and not invented");
    assert_eq!(
        e.grab(&id).cleanup,
        "seeding",
        "ratio 1 of 2: still seeding"
    );
    // An abrupt stop loses at most the unobserved bytes; never more.
    let second = leechers.leech(&t, e.app_addr().await, "2").await;
    leechers.forget(&second).await;
    e.tick().await;
    let (up3, ..) = e.counters(&t.info_hash);
    assert!(up3 > up2, "{up3}");
    e.restart(false).await;
    e.tick().await;
    assert_eq!(e.counters(&t.info_hash).0, up3);
    // More controlled leechers until the normal goal (ratio 2 of the content) is met.
    let mut n = 3;
    while (e.counters(&t.info_hash).0 as u64) < 2 * t.size.max(e.counters(&t.info_hash).1 as u64) {
        assert!(n <= 5, "the goal was not reached");
        let h = leechers.leech(&t, e.app_addr().await, &n.to_string()).await;
        leechers.forget(&h).await;
        e.tick().await;
        n += 1;
    }
    // The engine stops it at the share limit; the normal cleanup removes the download.
    e.until(&id, "cleaned up", |g| g.cleanup == "cleaned").await;
    assert!(!source.exists(), "download data removed");
    assert!(dest.is_file() && playable(&dest).await, "library file kept");
    assert!(
        e.engine()
            .info(std::slice::from_ref(&t.info_hash))
            .await
            .unwrap()
            .is_empty()
    );

    // Replaying the add downloads nothing again.
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "movie:tmdb:900001" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["plan"]["execution"]["grabs"], json!([]), "{text}");
    let (_, q, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"].as_array().map(Vec::len), Some(1), "{q}");
}

#[tokio::test]
async fn a_season_pack_pauses_resumes_and_survives_an_abrupt_engine_stop() {
    let mut e = start(Opts {
        // Slow enough that the pause and the abrupt stop land mid-download.
        download_limit: Some(24 * 1024),
        ..Default::default()
    })
    .await;
    let title = "Fargo.S05.1080p.WEB-DL.x264-UM";
    let files: Vec<(String, Vec<u8>)> = (1..=10)
        .map(|n| {
            (
                format!("Fargo.S05E{n:02}.1080p.WEB-DL.x264-UM.mkv"),
                mkv_sized(4, 1280, 720),
            )
        })
        .collect();
    let (t, r) = e.publish(title, files, 25.0, false).await;
    e.search_results("Fargo S05", vec![r]);
    // Operator handoff for exactly this season, then the normal add with search.
    let (s, v, text) = e
        .json(e.post("/api/v1/library").json(&json!({
            "media_id": "season:tvdb:269613:5", "native_handoff_confirmed": true
        })))
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["plan"]["selection"], "season_pack", "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    let started = Instant::now();
    while progress(&e, &t.info_hash)
        .await
        .is_none_or(|(p, _)| p <= 0.0)
    {
        assert!(started.elapsed() < Duration::from_secs(30), "no progress");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Paused through the API: the engine stops fetching.
    let (s, _, text) = e.json(e.post(&format!("/api/v1/queue/{id}/pause"))).await;
    assert_eq!(s, 200, "{text}");
    let (p1, state) = progress(&e, &t.info_hash).await.unwrap();
    assert_eq!(state, "pausedDL");
    assert!(p1 < 1.0, "paused mid-download ({p1})");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        progress(&e, &t.info_hash).await.unwrap().0,
        p1,
        "nothing fetched while paused"
    );
    let (s, _, text) = e.json(e.post(&format!("/api/v1/queue/{id}/resume"))).await;
    assert_eq!(s, 200, "{text}");
    while progress(&e, &t.info_hash)
        .await
        .is_none_or(|(p, _)| p <= p1)
    {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "no progress after resume"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // An abrupt stop mid-download; the new engine resumes from its data and the peer.
    let before = progress(&e, &t.info_hash).await.unwrap().0;
    assert!(before < 1.0, "still downloading ({before})");
    e.restart(false).await;
    e.until(&id, "imported", |g| g.state == "imported").await;
    let (_, _, _, valid, verified) = e.counters(&t.info_hash);
    assert!(valid && verified);
    let season = e.tv.join("Fargo/Season 5");
    for n in 1..=10 {
        let f = season.join(format!("Fargo.S05E{n:02}.1080p.WEB-DL.x264-UM.mkv"));
        assert!(f.is_file(), "E{n:02}");
    }
    assert!(playable(&season.join("Fargo.S05E10.1080p.WEB-DL.x264-UM.mkv")).await);
    assert_eq!(e.grab(&id).imported_files, 10);
}

#[tokio::test]
async fn a_magnet_resolves_from_its_peer_and_unsafe_foreign_or_unaccounted_downloads_fail_closed() {
    let e = start(Opts {
        ..Default::default()
    })
    .await;
    // A magnet only: the metadata comes from the controlled peer, then the normal flow.
    let title = "The.Matrix.1999.1080p.WEB-DL.x264-UM";
    let (t, r) = e.publish(title, movie_files(title), 0.05, true).await;
    e.search_results("The Matrix 1999", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.until(&id, "imported", |g| g.state == "imported").await;
    assert!(
        e.movies
            .join(format!("The Matrix (1999)/{title}.mkv"))
            .is_file()
    );

    // Lost counters: the share ratio is unknown, so nothing removes the download.
    let h = t.info_hash.clone();
    e.state
        .store
        .blocking(|c| {
            c.execute(
                "UPDATE embedded_torrents SET counters_valid = 0 WHERE info_hash = ?1",
                [h],
            )?;
            Ok(())
        })
        .unwrap();
    e.tick().await;
    let info = e
        .engine()
        .info(std::slice::from_ref(&t.info_hash))
        .await
        .unwrap();
    assert_eq!(
        (info[0].ratio, info[0].uploaded),
        (None, None),
        "never fabricated"
    );
    let (s, v, text) = e
        .json(
            e.post(&format!("/api/v1/queue/{id}/cancel"))
                .json(&json!({ "remove": "torrent_and_data" })),
        )
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("seed_criteria_unmet")),
        "{text}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ratio is not reported"),
        "{text}"
    );
    assert!(e.downloads.join(format!("{title}/{title}.mkv")).is_file());

    // A foreign file where a new single-file torrent would write: refused, left untouched.
    let name = "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM.mkv";
    std::fs::write(e.downloads.join(name), b"someone else's file").unwrap();
    let (_, r) = e
        .publish(name, vec![(name.into(), mkv(2))], 1.5, false)
        .await;
    e.search_results("Slow Horses S01E02", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let foreign = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    e.until(&foreign, "refused", |g| g.state == "failed").await;
    let g = e.grab(&foreign);
    assert_eq!(
        g.failure.as_deref(),
        Some("invalid_torrent"),
        "{:?}",
        g.last_error
    );
    assert!(
        g.last_error
            .unwrap_or_default()
            .contains("never overwritten")
    );
    assert_eq!(
        std::fs::read(e.downloads.join(name)).unwrap(),
        b"someone else's file"
    );
}

#[tokio::test]
async fn grabs_beyond_the_active_download_limit_wait_in_the_outbox() {
    let e = start(Opts {
        download_limit: Some(16 * 1024),
        max_active: 1,
        ..Default::default()
    })
    .await;
    let (title, name) = (
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM.mkv",
    );
    let (_, r1) = e
        .publish(
            title,
            vec![(format!("{title}.mkv"), mkv_sized(12, 1920, 1080))],
            0.05,
            false,
        )
        .await;
    let (_, r2) = e
        .publish(name, vec![(name.into(), mkv(2))], 1.5, false)
        .await;
    e.search_results("The Matrix 1999", vec![r1]);
    e.search_results("Slow Horses S01E02", vec![r2]);
    let mut ids = vec![];
    for target in ["movie:tmdb:603", "episode:tvdb:372264:1:2"] {
        let (s, v, text) = e
            .json(
                e.post("/api/v1/plans")
                    .json(&json!({ "media_id": target, "execute": true })),
            )
            .await;
        assert_eq!(s, 201, "{text}");
        ids.push(
            v["plan"]["execution"]["grabs"][0]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    e.tick().await;
    let (first, second) = (e.grab(&ids[0]), e.grab(&ids[1]));
    assert_eq!(first.state, "downloading");
    assert_eq!(
        (second.state.as_str(), second.attempts),
        ("queued", 0),
        "waits, no attempt counted"
    );
    assert!(
        e.engine()
            .info(std::slice::from_ref(&second.info_hash))
            .await
            .unwrap()
            .is_empty()
    );
    // Once the first is done, the second is submitted.
    e.until(&ids[1], "second submitted", |g| g.state != "queued")
        .await;
    assert_ne!(e.grab(&ids[0]).state, "downloading");
    e.until(&ids[1], "second imported", |g| g.state == "imported")
        .await;
}

#[tokio::test]
async fn a_state_is_bound_to_its_first_download_backend() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let lib = format!(
        "[library]\nmovies = '{}'\n[migration]\nmode = \"coexistence\"\n[execution]\nenabled = true\n",
        tmp.path().join("movies").display()
    );
    let embedded = format!(
        "{lib}[embedded]\ndownload_dir = '{}'\nlisten = \"127.0.0.1:0\"\n",
        tmp.path().join("dl").display()
    );
    let (addr, _mock) = start_mock().await;
    write_managed_config(&dir, addr, false, &embedded);
    let state = api::AppState::new(config::load(&dir.join("config.toml")).unwrap()).unwrap();
    assert!(
        !state.client.as_ref().unwrap().embedded().unwrap().started(),
        "no engine until used"
    );
    drop(state);
    // The same state with qBittorrent configured instead is refused, never reinterpreted.
    let qbit = format!(
        "{lib}[qbittorrent]\nbase_url = \"http://127.0.0.1:9/\"\nsave_path = \"/downloads\"\nlocal_path = '{}'\n",
        tmp.path().join("qb").display()
    );
    write_managed_config(&dir, addr, false, &qbit);
    let err = api::AppState::new(config::load(&dir.join("config.toml")).unwrap())
        .err()
        .unwrap();
    assert!(
        format!("{err:#}").contains("bound to the embedded download backend"),
        "{err:#}"
    );
}

/// Opt-in: the real `arr serve` process (worker included) with the embedded
/// engine, killed with SIGKILL in the middle of a download, then started again over the same
/// state. Loopback only: in-process Prowlarr/TMDB mocks and a controlled seeding peer.
/// `cargo test --locked --test embedded -- --ignored --nocapture`
#[tokio::test]
#[ignore = "opt-in: runs the real binary and kills it mid-download (about 20 s)"]
async fn the_server_process_killed_mid_download_resumes_and_imports() {
    let (mock_addr, mock) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let (dir, downloads, movies) = (
        root.join("state"),
        root.join("downloads"),
        root.join("movies"),
    );
    std::fs::create_dir_all(&movies).unwrap();
    let seeder = Peer::start(&root.join("seeder")).await;
    fsutil::ensure_private_dir(&dir).unwrap();
    fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
    fsutil::write_new_private_file(&dir.join("secrets/tmdb"), TMDB_V3_KEY.as_bytes()).unwrap();
    let extra = format!(
        "[execution]\nenabled = true\ndownload_cleanup = true\n\
         [embedded]\ndownload_dir = '{}'\nlisten = \"127.0.0.1:0\"\ntrackers = false\ninitial_peers = [\"{}\"]\ndownload_limit_bytes_per_sec = 16384\n\
         [migration]\nmode = \"coexistence\"\n\
         [library]\nmovies = '{}'\ntv = '{}'\nsample_detection = false\nprobe_timeout_secs = 30\n\
         [automation]\npoll_seconds = 1\n\
         [tmdb]\nenabled = true\nbase_url = \"http://{mock_addr}/tmdb\"\ncredential_file = \"secrets/tmdb\"\n",
        downloads.display(),
        seeder.addr(),
        movies.display(),
        root.join("tv").display()
    );
    std::fs::create_dir_all(root.join("tv")).unwrap();
    write_managed_config(&dir, mock_addr, false, &extra);
    {
        let (state, base, server) = serve(&dir).await;
        setup_managed(&base, &reqwest::Client::new()).await;
        server.abort();
        drop(state);
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cfg_path = dir.join("config.toml");
    let cfg = std::fs::read_to_string(&cfg_path).unwrap().replacen(
        "[server]\n",
        &format!("[server]\nlisten = \"127.0.0.1:{port}\"\n"),
        1,
    );
    std::fs::remove_file(&cfg_path).unwrap();
    fsutil::write_new_private_file(&cfg_path, cfg.as_bytes()).unwrap();
    let title = "Leviticus.2025.1080p.WEB-DL.x264-UM";
    let files = vec![(format!("{title}.mkv"), mkv_sized(20, 1920, 1080))];
    let t = arr::fixture::build_torrent(&format!("{title}.mkv"), &files);
    seeder.seed(&t, &files).await;
    let mut r = release(title, 4.0, 50);
    r["guid"] = json!(format!("fixture-{}", t.info_hash));
    r["infoHash"] = json!(t.info_hash);
    r["downloadUrl"] = json!(format!(
        "http://{mock_addr}/prowlarr/1/download?apikey={PROWLARR_KEY}&link={}",
        t.info_hash
    ));
    r["magnetUrl"] = Value::Null;
    {
        let mut m = mock.lock().unwrap();
        m.torrents.insert(t.info_hash.clone(), t.torrent.clone());
        m.releases.insert("Leviticus 2025".into(), vec![r]);
    }
    let spawn = || {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_arr"))
            .args(["serve", "--config"])
            .arg(&cfg_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let wait_up = || async {
        for _ in 0..200 {
            if http
                .get(format!("{base}/api/v1/health"))
                .send()
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the server did not start");
    };
    let grab = |id: String| {
        let (http, base) = (http.clone(), base.clone());
        async move {
            let v: Value = http
                .get(format!("{base}/api/v1/queue/{id}"))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v["grab"].clone()
        }
    };
    let mut child = spawn();
    wait_up().await;
    let v: Value = http
        .post(format!("{base}/api/v1/library"))
        .bearer_auth(TOKEN)
        .json(&json!({ "media_id": "movie:tmdb:900001" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string();
    let started = Instant::now();
    loop {
        let g = grab(id.clone()).await;
        let p = g["progress"].as_f64().unwrap_or(0.0);
        if p > 0.0 && p < 1.0 {
            break;
        }
        assert!(p < 1.0, "finished before the kill: {g}");
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "never started: {g}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // SIGKILL: no shutdown path runs.
    child.kill().await.unwrap();
    let mut child = spawn();
    wait_up().await;
    loop {
        let g = grab(id.clone()).await;
        if g["state"] == "imported" {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "not imported after the restart: {g}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let dest = movies.join(format!("Leviticus (2025)/{title}.mkv"));
    assert!(dest.is_file() && playable(&dest).await);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        files[0].1,
        "byte-identical after the kill"
    );
    child.kill().await.unwrap();
}

// ------------------------------------------------------------------ engine invariants

/// An engine over a fresh state, used directly (no API, no worker).
fn standalone(
    root: &Path,
) -> (
    arr::embedded::Embedded,
    arr::store::Store,
    config::EmbeddedConfig,
) {
    let state = root.join("state");
    fsutil::ensure_private_dir(&state).unwrap();
    fsutil::ensure_private_dir(&state.join("secrets")).unwrap();
    fsutil::write_new_private_file(&state.join("secrets/api_token"), TOKEN.as_bytes()).unwrap();
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    let text = format!(
        "[server]\napi_token_file = \"secrets/api_token\"\n[embedded]\ndownload_dir = '{}'\nlisten = \"127.0.0.1:0\"\ntrackers = false\n",
        root.join("downloads").display()
    );
    fsutil::write_new_private_file(&state.join("config.toml"), text.as_bytes()).unwrap();
    let cfg = config::load(&state.join("config.toml")).unwrap();
    let store = arr::store::Store::open(&state.join("state.sqlite")).unwrap();
    let e = cfg.embedded.unwrap();
    (
        arr::embedded::Embedded::new(e.clone(), store.clone()),
        store,
        e,
    )
}

fn request(payload: arr::prowlarr::Payload, save: &str) -> arr::qbit::AddRequest {
    arr::qbit::AddRequest {
        payload,
        save_path: save.to_string(),
        category: "um-movies".into(),
        tags: "unified-media".into(),
        paused: true,
        ratio_limit: Some(2.0),
        seeding_time_limit_minutes: None,
        sequential: false,
        first_last_piece: false,
        content_layout: None,
        select: None,
    }
}

fn row(store: &arr::store::Store, hash: &str) -> Option<(i64, i64, bool, bool, bool)> {
    let h = hash.to_string();
    store
        .blocking(|c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT uploaded, seeding_secs, completed_at IS NULL AND stopped_at_limit IS NULL, admitted_at IS NOT NULL, removed_at IS NULL FROM embedded_torrents WHERE info_hash = ?1",
                [h],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?)
        })
        .unwrap()
}

#[tokio::test]
async fn a_record_never_admitted_does_not_authorise_overwriting_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, store, cfg) = standalone(tmp.path());
    let name = "Crash.Window.2025.1080p.WEB-DL.x264-UM.mkv";
    let t = arr::fixture::build_torrent(name, &[(name.into(), vec![5; 40_000])]);
    // What a crash between recording and the engine opening its files leaves behind: the
    // record, never admitted. Meanwhile an unrelated file took the destination name.
    let (h, out) = (t.info_hash.clone(), cfg.save_path.clone());
    store
        .blocking(|c| {
            c.execute(
                "INSERT INTO embedded_torrents (info_hash, name, category, tags, save_path, output_folder, multi_file, ratio_limit, seeding_time_limit, added_at, updated_at)
                 VALUES (?1, ?2, 'um-movies', 'unified-media', ?3, ?3, 0, 2.0, -2, 'x', 'x')",
                rusqlite::params![h, name, out],
            )?;
            Ok(())
        })
        .unwrap();
    let foreign = cfg.download_dir.join(name);
    std::fs::write(&foreign, b"someone else's file").unwrap();
    let req = request(
        arr::prowlarr::Payload::Torrent(t.torrent.clone()),
        &cfg.save_path,
    );
    let err = engine.add(&req).await.unwrap_err();
    assert!(
        err.describe().contains("not created by this app's engine"),
        "{}",
        err.describe()
    );
    assert_eq!(std::fs::read(&foreign).unwrap(), b"someone else's file");
    assert!(
        engine
            .info(std::slice::from_ref(&t.info_hash))
            .await
            .unwrap()
            .is_empty()
    );
    // Once the destination is free, the torrent is admitted by the engine creating its file.
    std::fs::remove_file(&foreign).unwrap();
    assert!(engine.add(&req).await.unwrap());
    assert_eq!(
        row(&store, &t.info_hash).map(|r| r.3),
        Some(true),
        "admitted"
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn a_released_record_acquired_again_starts_a_fresh_seeding_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, store, cfg) = standalone(tmp.path());
    let name = "Lifecycle.2025.1080p.WEB-DL.x264-UM.mkv";
    let t = arr::fixture::build_torrent(name, &[(name.into(), vec![6; 40_000])]);
    let req = request(
        arr::prowlarr::Payload::Torrent(t.torrent.clone()),
        &cfg.save_path,
    );
    assert!(engine.add(&req).await.unwrap());
    engine.delete(&t.info_hash, false).await.unwrap();
    // The earlier lifecycle had seeded to its goal and been stopped there.
    let h = t.info_hash.clone();
    store
        .blocking(|c| {
            c.execute(
                "UPDATE embedded_torrents SET uploaded = 999999, seeding_secs = 99999, completed_at = 'x', stopped_at_limit = 'x' WHERE info_hash = ?1",
                [h],
            )?;
            Ok(())
        })
        .unwrap();
    // Its data went with the journaled removal; a new grab of the same release follows.
    std::fs::remove_file(cfg.download_dir.join(name)).unwrap();
    assert!(engine.add(&req).await.unwrap());
    let (uploaded, seeding, fresh, admitted, active) = row(&store, &t.info_hash).unwrap();
    assert_eq!(
        (uploaded, seeding, fresh, admitted, active),
        (0, 0, true, true, true)
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn engine_failures_never_carry_its_own_text_out() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, _store, cfg) = standalone(tmp.path());
    let canary = "CANARY-PASSKEY-7f3a";
    // A malformed magnet is refused here, before the engine sees it.
    let bad =
        format!("magnet:?xt=urn:btih:nothex&tr=http%3A%2F%2Ftracker.example%2F{canary}%2Fannounce");
    let err = engine
        .add(&request(
            arr::prowlarr::Payload::Magnet(bad),
            &cfg.save_path,
        ))
        .await
        .unwrap_err();
    assert!(!err.describe().contains(canary), "{}", err.describe());
    // A valid magnet the engine cannot resolve (no trackers, DHT or peers): its own error is
    // reduced to the stage and kind.
    let good = format!(
        "magnet:?xt=urn:btih:{}&tr=http%3A%2F%2Ftracker.example%2F{canary}%2Fannounce",
        "ab".repeat(20)
    );
    let err = engine
        .add(&request(
            arr::prowlarr::Payload::Magnet(good),
            &cfg.save_path,
        ))
        .await
        .unwrap_err();
    let text = err.describe();
    assert!(text.contains("fetching magnet metadata failed"), "{text}");
    assert!(
        !text.contains(canary) && !text.contains("tracker.example"),
        "{text}"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_engine_lock_is_held_until_its_session_has_actually_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let (first, store, cfg) = standalone(tmp.path());
    first.start().await.unwrap();
    let first = Arc::new(first);
    let stopping = tokio::spawn({
        let f = first.clone();
        async move { f.shutdown().await }
    });
    // While the first session is stopping (librqbit's stop winds tasks down), the state is
    // still owned.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!stopping.is_finished());
    let second = arr::embedded::Embedded::new(cfg.clone(), store.clone());
    let early = second.start().await;
    assert!(
        early.is_err(),
        "the lock was released before the session stopped"
    );
    stopping.await.unwrap();
    // Once stopped, a new engine may take over.
    second.start().await.unwrap();
    second.shutdown().await;
}

#[tokio::test]
async fn concurrent_controls_fabricate_no_bytes_and_seeding_time_accrues_below_a_second() {
    let e = start(Opts {
        ..Default::default()
    })
    .await;
    let title = "The.Matrix.1999.1080p.WEB-DL.x264-UM";
    let (t, r) = e
        .publish(
            title,
            vec![(format!("{title}.mkv"), mkv_sized(12, 1920, 1080))],
            0.05,
            false,
        )
        .await;
    e.search_results("The Matrix 1999", vec![r]);
    let (_, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    e.until(&id, "imported", |g| g.state == "imported").await;
    for h in e
        .seeder
        .session
        .with_torrents(|it| it.map(|(_, h)| h.clone()).collect::<Vec<_>>())
    {
        e.seeder.forget(&h).await;
    }
    // The current live state uploads once, so its own counter is not zero.
    let leechers = Peer::start(&e._tmp.path().join("leechers")).await;
    let l = leechers.leech(&t, e.app_addr().await, "1").await;
    leechers.forget(&l).await;
    e.tick().await;
    let (before, ..) = e.counters(&t.info_hash);
    assert!(before as u64 >= t.size);
    // Concurrent observations and controls with nobody connected: nothing may be added.
    let engine = || e.engine();
    let hash = t.info_hash.clone();
    let reads = async {
        for _ in 0..60 {
            engine().info(std::slice::from_ref(&hash)).await.unwrap();
            tokio::task::yield_now().await;
        }
    };
    let controls = async {
        for i in 0..20 {
            if i % 2 == 0 {
                engine().pause(&hash).await.unwrap();
            } else {
                engine().resume(&hash).await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        engine().resume(&hash).await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(reads, controls)
    })
    .await
    .expect("no deadlock");
    let (after, _, seeding0, valid, _) = e.counters(&t.info_hash);
    assert_eq!(after, before, "no fabricated upload");
    assert!(valid);
    // Polls 300 ms apart while seeding: the time adds up across sub-second intervals.
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(2600) {
        e.engine().info(std::slice::from_ref(&hash)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let (_, _, seeding1, _, _) = e.counters(&t.info_hash);
    assert!(seeding1 - seeding0 >= 2, "{seeding0} -> {seeding1}");
    // Aggregate engine diagnostics, read-only.
    let (_, d, _) = e.json(e.get("/api/v1/diagnostics")).await;
    let agg = &d["embedded"];
    assert_eq!(agg["engine_states"]["seeding"], 1, "{agg}");
    assert_eq!(agg["engine_states"]["complete"], 1, "{agg}");
    assert!(
        agg["live_peers"].is_u64() && agg["upload_bytes_per_sec"].is_u64(),
        "{agg}"
    );
    assert!(!agg.to_string().contains(&t.info_hash), "no hashes");
    assert_eq!(
        e.counters(&t.info_hash).2,
        seeding1,
        "a diagnostics read records nothing"
    );
}

#[tokio::test]
async fn a_replayed_add_of_an_active_torrent_counts_nothing_again_and_keeps_its_check() {
    let e = start(Opts {
        ..Default::default()
    })
    .await;
    let title = "The.Matrix.1999.1080p.WEB-DL.x264-UM";
    let (t, r) = e
        .publish(
            title,
            vec![(format!("{title}.mkv"), mkv_sized(12, 1920, 1080))],
            0.05,
            false,
        )
        .await;
    e.search_results("The Matrix 1999", vec![r]);
    let (_, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    e.until(&id, "imported", |g| g.state == "imported").await;
    for h in e
        .seeder
        .session
        .with_torrents(|it| it.map(|(_, h)| h.clone()).collect::<Vec<_>>())
    {
        e.seeder.forget(&h).await;
    }
    let hash = t.info_hash.clone();
    let engine = e.engine();
    // A new live state, whose own counters start from zero, uploads one full copy and
    // fetches nothing: ratio 1 of the goal 2.
    engine.pause(&hash).await.unwrap();
    engine.resume(&hash).await.unwrap();
    let leechers = Peer::start(&e._tmp.path().join("leechers")).await;
    let l = leechers.leech(&t, e.app_addr().await, "1").await;
    leechers.forget(&l).await;
    engine.info(std::slice::from_ref(&hash)).await.unwrap();
    let (up, down, _, valid, verified) = e.counters(&t.info_hash);
    assert!(up as u64 >= t.size && (up as u64) < 2 * t.size.max(down as u64));
    assert!(valid && verified);
    // The same add replayed (a retry after a worker crash), each time observed again: the
    // live state's whole counters are not taken a second time, the goal is not reached
    // early and the check stands.
    let req = request(
        arr::prowlarr::Payload::Torrent(t.torrent.clone()),
        engine.save_path(),
    );
    for _ in 0..3 {
        assert!(engine.add(&req).await.unwrap());
        let i = engine.info(std::slice::from_ref(&hash)).await.unwrap();
        let (up2, down2, _, valid2, verified2) = e.counters(&t.info_hash);
        assert_eq!((up2, down2), (up, down), "nothing counted again");
        assert!(valid2 && verified2, "the check stands");
        assert_eq!(i[0].state, "uploading", "still seeding toward the goal");
    }
    let h = hash.clone();
    let stopped: bool = e
        .state
        .store
        .blocking(|c| {
            Ok(c.query_row(
                "SELECT stopped_at_limit IS NOT NULL FROM embedded_torrents WHERE info_hash = ?1",
                [h],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(!stopped, "not stopped at the share limit");
}

/// The engine's view of a torrent until it reports `downloading` with a peer connected and
/// nothing arriving (bounded).
async fn connected_without_data(e: &Emb, hash: &str) {
    let started = Instant::now();
    loop {
        let i = e
            .engine()
            .info(&[hash.to_string()])
            .await
            .unwrap()
            .pop()
            .unwrap();
        if i.state == "downloading" {
            assert_eq!((i.progress, i.dlspeed), (0.0, 0));
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "never connected: {}",
            i.state
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_download_with_peers_connected_but_no_data_fails_at_the_stall_timeout() {
    let mut e = start(Opts {
        stall_minutes: Some(30),
        ..Default::default()
    })
    .await;
    let title = "The.Matrix.1999.1080p.WEB-DL.x264-UM";
    let (t, r) = e
        .publish(
            title,
            vec![(format!("{title}.mkv"), mkv_sized(12, 1920, 1080))],
            0.05,
            false,
        )
        .await;
    // The controlled seeder forgets it: the only peer the engine meets is the silent one.
    for h in e
        .seeder
        .session
        .with_torrents(|it| it.map(|(_, h)| h.clone()).collect::<Vec<_>>())
    {
        e.seeder.forget(&h).await;
    }
    e.search_results("The Matrix 1999", vec![r]);
    let (_, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    e.until(&id, "submitted", |g| {
        g.state == "downloading" && g.submitted_at.is_some()
    })
    .await;
    let peer = silent_peer(e.app_addr().await, &t).await;
    connected_without_data(&e, &t.info_hash).await;
    // The worker sees `downloading` (a peer connected), never `stalled`, and no progress.
    let t0 = time::OffsetDateTime::now_utc();
    let at = |m: u64| t0 + Duration::from_secs(m * 60);
    e.tick_at(at(0)).await;
    e.tick_at(at(20)).await;
    let g = e.grab(&id);
    assert_eq!(
        (g.state.as_str(), g.client_state.as_deref()),
        ("downloading", Some("downloading"))
    );
    assert!(g.stalled_since.is_some(), "no progress while downloading");
    // The app stops for 100 min, and the engine with it: that time is not time without
    // progress. 20 min observed before the restart and 9 after are within the timeout.
    peer.abort();
    e.restart(true).await;
    let peer = silent_peer(e.app_addr().await, &t).await;
    connected_without_data(&e, &t.info_hash).await;
    e.tick_at(at(120)).await;
    e.tick_at(at(129)).await;
    let g = e.grab(&id);
    assert_eq!(g.state, "downloading", "{:?}", g.last_error);
    // 31 min observed: failed and blocklisted as any stall is.
    e.tick_at(at(131)).await;
    let g = e.grab(&id);
    assert_eq!(
        (g.state.as_str(), g.failure.as_deref()),
        ("failed", Some("stalled")),
        "{:?}",
        g.last_error
    );
    let msg = g.last_error.unwrap_or_default();
    assert!(
        msg.contains("no download progress for 31 min (downloading)"),
        "{msg}"
    );
    let (_, v, _) = e.json(e.get("/api/v1/blocklist")).await;
    assert_eq!(v["items"][0]["reason"], "stalled", "{v}");
    // The normal cleanup of a failed incomplete download removes it from the engine.
    e.tick_at(at(132)).await;
    e.tick_at(at(133)).await;
    let g = e.grab(&id);
    assert_eq!(g.cleanup, "cleaned", "{:?}", g.cleanup_detail);
    assert!(progress(&e, &t.info_hash).await.is_none());
    peer.abort();
}

#[tokio::test]
async fn a_completed_payload_changed_while_stopped_is_checked_again_before_any_import() {
    let mut e = start(Opts {
        ..Default::default()
    })
    .await;
    let title = "The.Matrix.1999.1080p.WEB-DL.x264-UM";
    let video = mkv_sized(12, 1920, 1080);
    let (t, r) = e
        .publish(
            title,
            vec![(format!("{title}.mkv"), video.clone())],
            0.05,
            false,
        )
        .await;
    e.search_results("The Matrix 1999", vec![r]);
    let (_, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    e.tick().await;
    // Complete and fully verified, but not yet imported (no worker pass since).
    let started = Instant::now();
    while progress(&e, &t.info_hash).await.map(|p| p.1).as_deref() != Some("uploading") {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "never verified"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(e.counters(&t.info_hash).4);
    assert_eq!(e.grab(&id).state, "downloading");
    for h in e
        .seeder
        .session
        .with_torrents(|it| it.map(|(_, h)| h.clone()).collect::<Vec<_>>())
    {
        e.seeder.forget(&h).await;
    }
    // While stopped, one byte in the middle changes; size and modification time do not.
    let file = e.downloads.join(format!("{title}.mkv"));
    e.restart_with(true, || {
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        let mut bytes = std::fs::read(&file).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        std::fs::write(&file, &bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    })
    .await;
    // The earlier verification does not carry over to the new session.
    assert!(
        !e.counters(&t.info_hash).4,
        "verification reset for a pending import"
    );
    for _ in 0..30 {
        e.tick().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let g = e.grab(&id);
    assert_ne!(g.state, "imported", "{:?}", g.client_state);
    assert!(
        !e.movies
            .join(format!("The Matrix (1999)/{title}.mkv"))
            .exists()
    );
    assert_ne!(
        g.client_state.as_deref(),
        Some("seeding"),
        "never reported complete"
    );
}

/// The API socket is taken first: with it occupied, `serve` exits before any engine or
/// worker starts, and leaves the state free for the next start.
#[test]
fn an_occupied_api_port_starts_no_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (_engine, _store, _cfg) = standalone(root);
    let api = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let engine_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let cfg_path = root.join("state/config.toml");
    let text = format!(
        "[server]\nlisten = '{}'\napi_token_file = \"secrets/api_token\"\n[execution]\nenabled = true\n[embedded]\ndownload_dir = '{}'\nlisten = '{engine_port}'\ntrackers = false\n[library]\nmovies = '{}'\n[prowlarr]\nbase_url = \"http://127.0.0.1:9/\"\napi_key_file = \"secrets/api_token\"\n[migration]\nmode = \"coexistence\"\n",
        api.local_addr().unwrap(),
        root.join("downloads").display(),
        root.join("movies").display()
    );
    std::fs::remove_file(&cfg_path).unwrap();
    fsutil::write_new_private_file(&cfg_path, text.as_bytes()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_arr"))
        .args(["serve", "--config"])
        .arg(&cfg_path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "serve must fail with its API port taken"
    );
    assert!(
        std::net::TcpStream::connect_timeout(&engine_port, Duration::from_millis(200)).is_err(),
        "no engine listener"
    );
    assert!(
        !root.join("state/embedded/engine.lock").exists(),
        "no engine was started"
    );
}

#[tokio::test]
async fn a_season_pack_fetches_only_the_files_of_the_episodes_it_was_grabbed_for() {
    // Slow Horses S01: E01 and E03 are in the library already.
    let mut e = start(Opts {
        // Slow enough that the abrupt stop lands mid-download.
        download_limit: Some(24 * 1024),
        ..Default::default()
    })
    .await;
    let title = "Slow.Horses.S01.1080p.WEB-DL.x264-UM";
    let name = |n: u16| format!("Slow.Horses.S01E{n:02}.1080p.WEB-DL.x264-UM.mkv");
    // The owned episodes (and the nfo) are big, so the saving is plain: nothing like their
    // size should be fetched or written.
    let big = |bytes: Vec<u8>| {
        let mut b = bytes;
        b.resize(400 * 1024, 0);
        b
    };
    let files: Vec<(String, Vec<u8>)> = (1..=6)
        .map(|n| {
            let video = mkv_sized(4, 1280, 720);
            (
                name(n),
                if [1, 3].contains(&n) {
                    big(video)
                } else {
                    video
                },
            )
        })
        .chain([("Slow.Horses.S01.nfo".to_string(), big(b"nfo".to_vec()))])
        .collect();
    let total: usize = files.iter().map(|f| f.1.len()).sum();
    let (t, r) = e.publish(title, files, 12.0, false).await;
    e.search_results("Slow Horses S01", vec![r]);
    let (s, v, text) = e
        .json(e.post("/api/v1/plans").json(&json!({
            "media_id": "season:tvdb:372264:1", "execute": true
        })))
        .await;
    assert_eq!(s, 201, "{text}");
    let cand = &v["plan"]["candidates"][0];
    assert_eq!(cand["covers_episodes"], json!([2, 4, 5, 6]), "{text}");
    assert_eq!(cand["skips_owned"], json!([1, 3]), "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    let started = Instant::now();
    while progress(&e, &t.info_hash)
        .await
        .is_none_or(|(p, _)| p <= 0.0)
    {
        assert!(started.elapsed() < Duration::from_secs(30), "no progress");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Only the four episodes are selected: the owned ones and the nfo are not wanted.
    let wanted = |fs: &[arr::qbit::TorrentFile]| -> Vec<(String, i64)> {
        fs.iter()
            .map(|f| (f.name.rsplit('/').next().unwrap().to_string(), f.priority))
            .collect()
    };
    let expect: Vec<(String, i64)> = (1..=6)
        .map(|n| (name(n), i64::from(![1, 3].contains(&n))))
        .chain([("Slow.Horses.S01.nfo".to_string(), 0)])
        .collect();
    let fs = e.engine().files(&t.info_hash).await.unwrap().unwrap();
    assert_eq!(wanted(&fs), expect);
    // The engine's total is what it was asked for, not the whole pack.
    let size = e
        .engine()
        .info(std::slice::from_ref(&t.info_hash))
        .await
        .unwrap()[0]
        .size;
    assert!(size > 0 && (size as usize) < total, "{size} of {total}");

    // The selection survives an abrupt stop.
    let before = progress(&e, &t.info_hash).await.unwrap().0;
    assert!(before < 1.0, "still downloading ({before})");
    e.restart(false).await;
    let fs = e.engine().files(&t.info_hash).await.unwrap().unwrap();
    assert_eq!(wanted(&fs), expect, "the selection came back");

    e.until(&id, "imported", |g| g.state == "imported").await;
    assert_eq!(e.grab(&id).imported_files, 4);
    let season = e.tv.join("Slow Horses/Season 1");
    for n in [2, 4, 5, 6] {
        assert!(season.join(name(n)).is_file(), "E{n:02} from the pack");
    }
    for n in [1, 3] {
        assert!(
            !season.join(name(n)).exists(),
            "E{n:02} stays the file it was"
        );
        assert!(
            season
                .join(format!("Slow.Horses.S01E0{n}.1080p.WEB-DL.x264-OLD.mkv"))
                .is_file()
        );
    }
    // Files that were not selected hold no more than the edges of the pieces they share with
    // a selected one: nothing like their size is on the disk.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        for f in [name(1), name(3), "Slow.Horses.S01.nfo".to_string()] {
            let p = e.downloads.join(title).join(&f);
            let m = std::fs::metadata(&p).unwrap_or_else(|_| panic!("{f} is not created"));
            let used = m.blocks() * 512;
            assert!(
                used <= 64 * 1024 || used < m.len() / 2,
                "{f}: {used} bytes on disk of {}",
                m.len()
            );
        }
    }
    // Every selected piece passed the check, so the grab is not held back by it.
    let (_, _, _, valid, verified) = e.counters(&t.info_hash);
    assert!(valid && verified);
}
