//! Real-client sandbox harness (ignored by default). Each scenario drives a full flow against
//! an isolated qBittorrent 4.3.9+ configured through environment variables, with an
//! in-process mock Prowlarr and generated fixtures. See docs/EXECUTION-SANDBOX.md section 10.
//! A scenario only touches its own categories, tag, info-hashes and directories; no native
//! app is configured or contacted.
#![recursion_limit = "256"]

mod common;

use arr::{api, config, fsutil};
use common::*;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is required (see docs/EXECUTION-SANDBOX.md)"))
}

/// One isolated server over private state, the real client and per-run library roots.
struct Sandbox {
    dir: PathBuf,
    state: Arc<api::AppState>,
    base: String,
    client: reqwest::Client,
    mock: Mock,
    mock_addr: SocketAddr,
    downloads: PathBuf,
    movies: PathBuf,
}

impl Sandbox {
    /// `dir` must outlive the sandbox (the caller owns the temporary directory).
    async fn start(dir: &Path, mode: &str, categories: (&str, &str)) -> Self {
        let qb_url = env("UM_SANDBOX_QB_URL");
        let user = std::fs::read_to_string(env("UM_SANDBOX_QB_USER_FILE")).unwrap();
        let pass = std::fs::read_to_string(env("UM_SANDBOX_QB_PASS_FILE")).unwrap();
        let save_path = env("UM_SANDBOX_SAVE_PATH");
        let downloads = PathBuf::from(env("UM_SANDBOX_LOCAL_DOWNLOADS"));
        let library = PathBuf::from(env("UM_SANDBOX_LIBRARY"));
        let movies = library.join(format!("movies-{}", fsutil::random_hex(4).unwrap()));
        let tv = library.join(format!("tv-{}", fsutil::random_hex(4).unwrap()));
        std::fs::create_dir_all(&movies).unwrap();
        std::fs::create_dir_all(&tv).unwrap();
        let (mock_addr, mock) = start_mock().await;
        fsutil::ensure_private_dir(dir).unwrap();
        fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
        fsutil::write_new_private_file(&dir.join("secrets/qb_user"), user.trim().as_bytes())
            .unwrap();
        fsutil::write_new_private_file(&dir.join("secrets/qb_pass"), pass.trim().as_bytes())
            .unwrap();
        let extra = format!(
            "[execution]\nenabled = true\n[qbittorrent]\nbase_url = '{qb_url}'\nusername_file = \"secrets/qb_user\"\npassword_file = \"secrets/qb_pass\"\nsave_path = '{save_path}'\nlocal_path = '{}'\nmovie_category = '{}'\ntv_category = '{}'\n[library]\nmovies = '{}'\ntv = '{}'\nsample_detection = false\n[automation]\npoll_seconds = 3600\n[migration]\nmode = '{mode}'\n",
            downloads.display(),
            categories.0,
            categories.1,
            movies.display(),
            tv.display()
        );
        write_state(dir, &snapshot(), &extra, Some(mock_addr), false);
        let (state, base) = serve(dir).await;
        Sandbox {
            dir: dir.to_path_buf(),
            state,
            base,
            client: reqwest::Client::new(),
            mock,
            mock_addr,
            downloads,
            movies,
        }
    }

    /// A process restart: a new state and server over the same private directory, with
    /// startup recovery.
    async fn restart(&mut self) {
        let (state, base) = serve(&self.dir).await;
        self.state = state;
        self.base = base;
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(TOKEN);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let resp = rb.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Value {
        self.call(reqwest::Method::GET, path, None).await.1
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    async fn tick_at(&self, now: time::OffsetDateTime) {
        arr::worker::tick(&self.state, now).await.unwrap();
    }

    async fn tick(&self) {
        self.tick_at(time::OffsetDateTime::now_utc()).await;
    }

    /// Ticks the worker until the grab is seeding or imported (the real client verifying the
    /// pre-placed data), failing on any terminal or blocked state.
    async fn wait_imported(&self, id: &str) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
        loop {
            self.tick().await;
            let g = self.get(&format!("/api/v1/queue/{id}")).await;
            let status = g["grab"]["status"].as_str().unwrap_or("").to_string();
            eprintln!("status: {status} ({})", g["grab"]["client_state"]);
            if status == "seeding" || status == "imported" {
                return g;
            }
            assert!(
                !matches!(status.as_str(), "failed" | "import_blocked" | "released"),
                "{g}"
            );
            assert!(std::time::Instant::now() < deadline, "timed out: {g}");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    /// Explicit removal of our torrent and its data; the library file must stay playable.
    async fn remove_and_check(&self, id: &str, source: &Path, dest: &Path) {
        let (s, v) = self
            .post(
                &format!("/api/v1/queue/{id}/cancel"),
                json!({ "remove": "torrent_and_data", "ignore_seed_criteria": true }),
            )
            .await;
        assert_eq!(s, 200, "{v}");
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            self.tick().await;
            if !source.exists() {
                break;
            }
        }
        assert!(!source.exists(), "download data removed");
        assert!(
            dest.is_file() && playable(dest).await,
            "library file intact"
        );
    }

    /// Our torrents currently in the real client, among `hashes`.
    async fn in_client(&self, hashes: &[String]) -> Vec<arr::qbit::TorrentInfo> {
        self.state
            .client
            .as_ref()
            .unwrap()
            .info(hashes)
            .await
            .unwrap()
    }
}

async fn serve(dir: &Path) -> (Arc<api::AppState>, String) {
    let cfg = config::load(&dir.join("config.toml")).unwrap();
    let state = Arc::new(api::AppState::new(cfg).unwrap());
    arr::worker::recover(&state, time::OffsetDateTime::now_utc()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let s2 = state.clone();
    tokio::spawn(async move { axum::serve(listener, api::router(s2)).await.unwrap() });
    (state, base)
}

/// Deletes exactly the given (run-unique) hashes from the real client, with their data, so a
/// failed scenario leaves nothing behind. Hashes that are absent are ignored.
async fn delete_leftovers(dir: &Path, hashes: &[String]) {
    let cfg = config::load(&dir.join("config.toml")).unwrap();
    let qb = arr::qbit::Qbit::new(cfg.qbittorrent.unwrap());
    for h in hashes {
        if let Ok(list) = qb.info(std::slice::from_ref(h)).await
            && !list.is_empty()
        {
            eprintln!("cleanup: deleting leftover {h}");
            let _ = qb.delete(h, true).await;
        }
    }
}

/// Runs a scenario as its own task so that cleanup happens even when it panics, then
/// re-raises the panic.
async fn guarded<F>(dir: PathBuf, hashes: Vec<String>, scenario: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let outcome = tokio::spawn(scenario).await;
    delete_leftovers(&dir, &hashes).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test]
#[ignore = "needs an isolated real qBittorrent (UM_SANDBOX_* environment variables)"]
async fn real_qbittorrent_movie_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let sb = Sandbox::start(&dir, "legacy", ("um-sandbox-movies", "um-sandbox-tv")).await;
    let diag = sb.get("/api/v1/diagnostics").await;
    eprintln!("qbittorrent: {}", diag["qbittorrent"]);
    assert_eq!(diag["qbittorrent"]["status"], "ok");

    // Fixture: unique name per run, data placed where the client will look for it.
    let name = format!(
        "The.Matrix.1999.1080p.WEB-DL.x264-SANDBOX{}",
        fsutil::random_hex(3).unwrap().to_uppercase()
    );
    let files = vec![(format!("{name}.mkv"), mkv(5))];
    let t = arr::fixture::write_release(&sb.downloads, &name, &files).unwrap();
    sb.mock
        .lock()
        .unwrap()
        .torrents
        .insert(t.info_hash.clone(), t.torrent.clone());
    let mut r = release(&name, 8.0, 50);
    r["guid"] = json!(format!("sandbox-{}", t.info_hash));
    r["infoHash"] = json!(t.info_hash);
    r["downloadUrl"] = json!(format!(
        "http://{}/prowlarr/1/download?link={}",
        sb.mock_addr, t.info_hash
    ));
    r["magnetUrl"] = Value::Null;
    sb.mock
        .lock()
        .unwrap()
        .releases
        .insert("The Matrix 1999".into(), vec![r]);
    let hash = t.info_hash.clone();
    guarded(dir.clone(), vec![hash.clone()], async move {
        let (_, plan) = sb
            .post(
                "/api/v1/plans",
                json!({ "media_id": "movie:tmdb:603", "execute": true }),
            )
            .await;
        let id = plan["plan"]["execution"]["grabs"][0]
            .as_str()
            .unwrap_or_else(|| panic!("not executed: {plan}"))
            .to_string();
        sb.wait_imported(&id).await;
        let dest = sb.movies.join(format!("The Matrix (1999)/{name}.mkv"));
        let source = sb.downloads.join(format!("{name}.mkv"));
        assert!(dest.is_file() && source.is_file());
        assert!(playable(&dest).await);
        eprintln!("hardlinked: {}", ino(&dest) == ino(&source));
        // Seeding without peers: the share limit is never met, so the source is kept.
        sb.tick().await;
        assert!(source.is_file());
        sb.remove_and_check(&id, &source, &dest).await;
        eprintln!("sandbox flow passed for {hash}");
    })
    .await;
}

/// Coexistence mode against the real client: an imported (observed) title is refused and
/// not picked up by RSS before adoption; after an acknowledged adoption without search, the
/// *scheduled* RSS pass grabs it, the real client verifies the generated payload, and it is
/// imported. An unadopted decoy in the same feed is never fetched or submitted. Replays and
/// a restart create no duplicate. Only the mock Prowlarr is contacted; no native app is
/// configured.
#[tokio::test]
#[ignore = "needs an isolated real qBittorrent (UM_SANDBOX_* environment variables)"]
async fn real_qbittorrent_coexistence_rss_adoption() {
    let run = fsutil::random_hex(4).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let categories = (format!("um-coex-{run}-movies"), format!("um-coex-{run}-tv"));
    let mut sb = Sandbox::start(&dir, "coexistence", (&categories.0, &categories.1)).await;
    let diag = sb.get("/api/v1/diagnostics").await;
    eprintln!("qbittorrent: {}", diag["qbittorrent"]);
    assert_eq!(diag["qbittorrent"]["status"], "ok");

    // The target's data is placed where the client looks; the decoy has none (if it were
    // ever submitted, the client would show it, and cleanup removes it).
    let tag = run.to_uppercase();
    let name = format!("The.Matrix.1999.1080p.WEB-DL.x264-COEX{tag}");
    let files = vec![(format!("{name}.mkv"), mkv(5))];
    let target = arr::fixture::write_release(&sb.downloads, &name, &files).unwrap();
    let decoy_name = format!("Slow.Horses.S01E02.1080p.WEB-DL.x264-DECOY{tag}");
    let decoy = arr::fixture::build_torrent(&decoy_name, &[(format!("{decoy_name}.mkv"), mkv(1))]);
    let item = |title: &str, hash: &str, gib: f64, cat: u32| FeedItem {
        title: title.into(),
        guid: format!("coex-{hash}"),
        url: format!(
            "http://{}/prowlarr/1/download?apikey={PROWLARR_KEY}&link={hash}",
            sb.mock_addr
        ),
        size: (gib * 1024.0 * 1024.0 * 1024.0) as i64,
        seeders: 50,
        info_hash: Some(hash.into()),
        categories: vec![cat],
        pub_date: "Mon, 26 Oct 2026 08:00:00 +0000".into(),
    };
    {
        let mut m = sb.mock.lock().unwrap();
        m.torrents
            .insert(target.info_hash.clone(), target.torrent.clone());
        m.torrents
            .insert(decoy.info_hash.clone(), decoy.torrent.clone());
        let feed = feed_xml(&[
            item(&name, &target.info_hash, 8.0, 2040),
            item(&decoy_name, &decoy.info_hash, 1.5, 5040),
        ]);
        m.feeds.insert("1".into(), feed);
    }
    let hashes = vec![target.info_hash.clone(), decoy.info_hash.clone()];
    let (th, dh) = (target.info_hash.clone(), decoy.info_hash.clone());
    guarded(dir.clone(), hashes.clone(), async move {
        let rss_status = |v: &Value, title: &str| -> (String, String) {
            let i = v["recent"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["title"] == title)
                .unwrap_or_else(|| panic!("{title} not in rss state: {v}"))
                .clone();
            (
                i["status"].as_str().unwrap_or("").into(),
                i["detail"].as_str().unwrap_or("").into(),
            )
        };
        let t0 = time::OffsetDateTime::now_utc();

        // 1. Before adoption: observed, refused, and the scheduled RSS pass ignores both.
        let o = sb.get("/api/v1/ownership?state=observed").await;
        assert!(o["counts"]["movies_observed"].as_u64().unwrap_or(0) >= 1, "{o}");
        let (s, v) = sb
            .post(
                "/api/v1/plans",
                json!({ "media_id": "movie:tmdb:603", "execute": true }),
            )
            .await;
        assert_eq!((s, v["error"]["code"].as_str()), (409, Some("not_adopted")), "{v}");
        sb.tick_at(t0).await;
        let rss = sb.get("/api/v1/rss").await;
        for title in [name.as_str(), decoy_name.as_str()] {
            let (status, detail) = rss_status(&rss, title);
            assert_eq!(status, "ignored", "{title}: {detail}");
        }
        assert!(sb.in_client(&hashes).await.is_empty(), "nothing submitted before adoption");

        // 2. Acknowledged scoped adoption without search: nothing is grabbed yet.
        let (s, v) = sb
            .post(
                "/api/v1/library",
                json!({ "media_id": "movie:tmdb:603", "native_handoff_confirmed": true, "search": false }),
            )
            .await;
        assert_eq!(s, 201, "{v}");
        assert_eq!(v["adoption"]["acquisition"], "adopted", "{v}");
        assert!(v.get("plan").is_none(), "search: false plans nothing: {v}");
        let q = sb.get("/api/v1/queue?all=true").await;
        assert_eq!(q["items"], json!([]), "{q}");

        // 3. The next scheduled RSS pass (its interval elapsed) picks the adopted title up.
        let t1 = t0 + Duration::from_secs(31 * 60);
        sb.tick_at(t1).await;
        let rss = sb.get("/api/v1/rss").await;
        assert_eq!(rss_status(&rss, &name).0, "grabbed", "{rss}");
        assert_eq!(rss_status(&rss, &decoy_name).0, "ignored", "{rss}");
        let q = sb.get("/api/v1/queue?all=true").await;
        let items = q["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 1, "{q}");
        assert_eq!(items[0]["origin"], "automatic");
        let id = items[0]["id"].as_str().unwrap().to_string();
        let g = sb.wait_imported(&id).await;
        assert_eq!(g["grab"]["category"], categories.0.as_str());
        let dest = sb.movies.join(format!("The Matrix (1999)/{name}.mkv"));
        let source = sb.downloads.join(format!("{name}.mkv"));
        assert!(dest.is_file() && source.is_file());
        assert!(playable(&dest).await);
        eprintln!("hardlinked: {}", ino(&dest) == ino(&source));
        let st = sb.get("/api/v1/status/movie:tmdb:603").await;
        assert_eq!(st["state"], "owned", "{st}");

        // In the real client: exactly the target, in our category with our tag.
        let present = sb.in_client(&hashes).await;
        assert_eq!(present.len(), 1, "only the target was submitted");
        assert_eq!(present[0].hash.to_ascii_lowercase(), th);
        assert_eq!(present[0].category, categories.0);
        assert!(present[0].has_tag(arr::downloads::TAG));

        // 4. Replays (another scheduled pass, the adoption call) and a restart: no duplicate.
        sb.tick_at(t1 + Duration::from_secs(31 * 60)).await;
        let (s, _) = sb
            .post(
                "/api/v1/library",
                json!({ "media_id": "movie:tmdb:603", "native_handoff_confirmed": true, "search": false }),
            )
            .await;
        assert_eq!(s, 201);
        sb.restart().await;
        sb.tick_at(t1 + Duration::from_secs(62 * 60)).await;
        let q = sb.get("/api/v1/queue?all=true").await;
        let ids: Vec<&str> = q["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [id.as_str()], "one grab, the same one: {q}");
        assert_eq!(sb.in_client(&hashes).await.len(), 1);

        // 5. Upstream traffic: feeds and the target's download only. No search API call, no
        //    decoy fetch; nothing native is configured.
        {
            let m = sb.mock.lock().unwrap();
            assert!(m.log.iter().all(|q| !q.contains("type=search")), "{:?}", m.log);
            assert_eq!(m.downloads, vec![th.clone()], "only the target was fetched");
            assert!(!m.downloads.contains(&dh));
            assert!(m.feed_log.len() >= 3, "{:?}", m.feed_log);
        }

        sb.remove_and_check(&id, &source, &dest).await;
        eprintln!("coexistence RSS adoption flow passed for {th} (decoy {dh} never submitted)");
    })
    .await;
}
