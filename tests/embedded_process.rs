//! Actual child-process SIGKILL recovery, with authored bytes and loopback peers only.
#![recursion_limit = "256"]
mod common;

use arr::{fixture, fsutil};
use common::{PROWLARR_KEY, TOKEN};
use librqbit::{
    AddTorrent, AddTorrentOptions, ListenerMode, ListenerOptions, Session, SessionOptions,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn launch(state: &Path, label: &str) -> Process {
    let log = std::fs::File::create(state.join(format!("{label}.stderr"))).unwrap();
    Process(
        Command::new(env!("CARGO_BIN_EXE_arr"))
            .args(["serve", "--config"])
            .arg(state.join("config.toml"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    )
}

async fn ready(child: &mut Process, http: &reqwest::Client, base: &str) {
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "scratch server exited; stderr retained in scratch state"
        );
        if let Ok(r) = http
            .get(format!("{base}/api/v1/readiness"))
            .bearer_auth(TOKEN)
            .send()
            .await
            && r.status().is_success()
        {
            return;
        }
        assert!(
            Instant::now() < until,
            "scratch server did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn counters(state: &Path, hash: &str) -> Option<(i64, i64)> {
    let c =
        Connection::open_with_flags(state.join("state.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    c.busy_timeout(Duration::from_secs(2)).unwrap();
    c.query_row(
        "SELECT uploaded, downloaded FROM embedded_torrents WHERE info_hash=?1",
        [hash],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .unwrap()
}

async fn peer(path: &Path) -> std::sync::Arc<Session> {
    std::fs::create_dir_all(path).unwrap();
    Session::new_with_opts(
        path.to_path_buf(),
        SessionOptions {
            dht: None,
            disable_trackers: true,
            disable_local_service_discovery: true,
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpOnly,
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                ipv4_only: true,
                ..Default::default()
            }),
            ipv4_only: true,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigkill_recovers_partial_download_then_real_upload_accounting() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let downloads = root.join("downloads");
    let movies = root.join("movies");
    std::fs::create_dir(&movies).unwrap();
    let (mock_addr, mock) = common::start_mock().await;
    let seed = peer(&root.join("seed")).await;
    let title = "Process.Proof.2026.1080p.WEB-DL.x264-UM";
    let filename = format!("{title}.mkv");
    // A non-media companion makes the transfer long enough to kill it mid-download.
    let video = common::mkv(3);
    let files = vec![
        (filename.clone(), video.clone()),
        ("companion.bin".into(), vec![42; 1024 * 1024]),
    ];
    let torrent = fixture::write_release(&root.join("seed-content"), title, &files).unwrap();
    let seed_handle = seed
        .add_torrent(
            AddTorrent::from_bytes(torrent.torrent.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(
                    root.join("seed-content")
                        .join(title)
                        .to_string_lossy()
                        .into_owned(),
                ),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), seed_handle.wait_until_completed())
        .await
        .unwrap()
        .unwrap();
    let mut release = common::release(title, 4.0, 50);
    release["guid"] = json!(format!("process-proof-{}", torrent.info_hash));
    release["infoHash"] = json!(torrent.info_hash);
    release["magnetUrl"] = Value::Null;
    release["downloadUrl"] = json!(format!(
        "http://{mock_addr}/prowlarr/1/download?apikey={PROWLARR_KEY}&link={}",
        torrent.info_hash
    ));
    {
        let mut m = mock.lock().unwrap();
        m.torrents
            .insert(torrent.info_hash.clone(), torrent.torrent.clone());
        m.releases
            .insert("Process Proof 2026".into(), vec![release]);
    }
    let api_addr = address();
    let engine_addr = address();
    let extra = format!(
        "[execution]\nenabled=true\n[embedded]\ndownload_dir='{}'\nlisten=\"{engine_addr}\"\ntrackers=false\ndht=false\nlocal_discovery=false\ninitial_peers=[\"{}\"]\ndownload_limit_bytes_per_sec=131072\npeer_limit=8\n[migration]\nmode=\"coexistence\"\n[library]\nmovies='{}'\nsample_detection=false\n[automation]\npoll_seconds=1\nrss_interval_minutes=0\nmetadata_refresh_hours=720\nrescan_hours=720\n",
        downloads.display(),
        seed.listen_addr().unwrap(),
        movies.display()
    );
    common::write_managed_config(&state, mock_addr, false, &extra);
    common::load_local(&state.join("config.toml")); // the child process must not reach the real sport source
    // The ratio-two seed goal (the setup default is ratio one).
    let settings = root.join("setup.toml");
    std::fs::write(
        &settings,
        "[[indexers]]\nid = 1\nseed_ratio = 2.0\nseed_time_minutes = -1\n",
    )
    .unwrap();
    let setup = std::process::Command::new(env!("CARGO_BIN_EXE_arr"))
        .args(["setup", "--config"])
        .arg(state.join("config.toml"))
        .arg("--settings")
        .arg(&settings)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let config_path = state.join("config.toml");
    let config_text = std::fs::read_to_string(&config_path).unwrap().replacen(
        "[server]\n",
        &format!("[server]\nlisten=\"{api_addr}\"\n"),
        1,
    );
    std::fs::remove_file(&config_path).unwrap();
    fsutil::write_new_private_file(&config_path, config_text.as_bytes()).unwrap();
    let base = format!("http://{api_addr}");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let mut child = launch(&state, "first");
    ready(&mut child, &http, &base).await;
    let result = http.post(format!("{base}/api/v1/library")).bearer_auth(TOKEN).json(&json!({
        "media_id":"movie:tmdb:900007", "metadata": {"title":"Process Proof", "year":2026, "runtime_minutes":90, "original_language":"english", "digital_release":"2026-01-01"}
    })).send().await.unwrap();
    let status = result.status();
    let body: Value = result.json().await.unwrap();
    assert_eq!(status.as_u16(), 201, "{body}");
    let grab = body["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("automatic add did not queue a grab: {body}"))
        .to_owned();
    let until = Instant::now() + Duration::from_secs(20);
    let before = loop {
        if let Some(v) = counters(&state, &torrent.info_hash)
            && v.1 > 0
            && (v.1 as u64) < torrent.size
        {
            break v;
        }
        assert!(
            Instant::now() < until,
            "no persisted partial network transfer observed"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    // Child::kill is SIGKILL on Unix: no application shutdown, counter flush or task cancellation.
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    drop(child);
    assert_eq!(counters(&state, &torrent.info_hash), Some(before));
    let mut child = launch(&state, "after-partial-kill");
    ready(&mut child, &http, &base).await;
    let after = counters(&state, &torrent.info_hash).unwrap();
    assert!(
        after.1 >= before.1,
        "download counter reset across actual process death"
    );
    let until = Instant::now() + Duration::from_secs(45);
    loop {
        let c = Connection::open_with_flags(
            state.join("state.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        c.busy_timeout(Duration::from_secs(2)).unwrap();
        let s: String = c
            .query_row("SELECT state FROM grabs WHERE id=?1", [&grab], |r| r.get(0))
            .unwrap();
        if s == "imported" {
            break;
        }
        assert!(
            Instant::now() < until,
            "partial transfer did not resume/import: {s}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let dest = movies.join("Process Proof (2026)").join(&filename);
    assert_eq!(std::fs::read(&dest).unwrap(), video);
    assert!(common::playable(&dest).await);
    assert!(seed_handle.stats().uploaded_bytes >= torrent.size);
    seed.delete(librqbit::api::TorrentIdOrHash::Id(seed_handle.id()), false)
        .await
        .unwrap();
    // A real receiver now gets all its bytes from the child process, not the original seeder.
    let leech = peer(&root.join("leecher")).await;
    let received = leech
        .add_torrent(
            AddTorrent::from_bytes(torrent.torrent.clone()),
            Some(AddTorrentOptions {
                initial_peers: Some(vec![engine_addr]),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), received.wait_until_completed())
        .await
        .unwrap()
        .unwrap();
    leech.stop().await;
    let until = Instant::now() + Duration::from_secs(5);
    let uploaded = loop {
        let v = counters(&state, &torrent.info_hash).unwrap();
        if v.0 as u64 >= torrent.size {
            break v;
        }
        assert!(
            Instant::now() < until,
            "real upload bytes were not persisted"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    drop(child);
    let mut child = launch(&state, "after-upload-kill");
    ready(&mut child, &http, &base).await;
    assert_eq!(
        counters(&state, &torrent.info_hash),
        Some(uploaded),
        "restart must neither reset nor invent observed bytes"
    );
    let c =
        Connection::open_with_flags(state.join("state.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let (cleanup, count): (String, i64) = c
        .query_row(
            "SELECT cleanup,(SELECT COUNT(*) FROM grabs) FROM grabs WHERE id=?1",
            [&grab],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        cleanup, "seeding",
        "one receiver has not met the normal ratio-two goal"
    );
    assert_eq!(count, 1, "no duplicate acquisition across process kills");
    assert_eq!(std::fs::read(dest).unwrap(), video);
    drop(child);
    seed.stop().await;
}
