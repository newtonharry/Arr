//! Independent recovery and confinement regressions. Scratch paths and loopback only.
use arr::{
    config, embedded::Embedded, fixture, fsutil, prowlarr::Payload, qbit::AddRequest, store::Store,
};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn setup(root: &Path, api: SocketAddr, engine: SocketAddr) -> config::RuntimeConfig {
    let state = root.join("state");
    fsutil::ensure_private_dir(&state).unwrap();
    fsutil::ensure_private_dir(&state.join("secrets")).unwrap();
    fsutil::write_new_private_file(
        &state.join("secrets/api_token"),
        b"independent-embedded-review-test-token",
    )
    .unwrap();
    std::fs::create_dir(root.join("downloads")).unwrap();
    let text = format!(
        "[server]\nlisten=\"{api}\"\napi_token_file=\"secrets/api_token\"\n[state]\ndir=\".\"\n[execution]\nenabled=false\n[embedded]\ndownload_dir=\"{}\"\nlisten=\"{engine}\"\ntrackers=false\ndht=false\nlocal_discovery=false\n",
        root.join("downloads").display()
    );
    fsutil::write_new_private_file(&state.join("config.toml"), text.as_bytes()).unwrap();
    config::load(&state.join("config.toml")).unwrap()
}

struct OwnChild(Child);
impl Drop for OwnChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn disabled_execution_does_not_start_embedded_network_listener() {
    let root = tempfile::tempdir().unwrap();
    let api = free_address();
    let engine = free_address();
    let _cfg = setup(root.path(), api, engine);
    let stderr = std::fs::File::create(root.path().join("server.stderr")).unwrap();
    let mut child = OwnChild(
        Command::new(env!("CARGO_BIN_EXE_arr"))
            .args(["serve", "--config"])
            .arg(root.path().join("state/config.toml"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect_timeout(&api, Duration::from_millis(100)).is_err() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "scratch server exited during startup"
        );
        assert!(Instant::now() < deadline, "scratch API did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        TcpStream::connect_timeout(&engine, Duration::from_millis(100)).is_err(),
        "execution=false must not start the embedded engine or resume saved transfers"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_parent_symlink_is_refused_before_engine_writes() {
    let root = tempfile::tempdir().unwrap();
    let cfg = setup(root.path(), free_address(), free_address());
    let outside = root.path().join("outside-downloads");
    std::fs::create_dir(&outside).unwrap();
    let torrent = fixture::build_torrent(
        "Confined Fixture",
        &[
            ("one.mkv".into(), vec![1; 32768]),
            ("two.mkv".into(), vec![2; 32768]),
        ],
    );
    std::os::unix::fs::symlink(&outside, root.path().join("downloads").join(&torrent.name))
        .unwrap();
    let store = Store::open(&cfg.state_dir.join("state.sqlite")).unwrap();
    let engine = Embedded::new(cfg.embedded.unwrap(), store);
    let answer = engine
        .add(&AddRequest {
            payload: Payload::Torrent(torrent.torrent),
            save_path: root.path().join("downloads").to_string_lossy().into_owned(),
            category: "review".into(),
            tags: "unified-media".into(),
            paused: true,
            ratio_limit: Some(2.0),
            seeding_time_limit_minutes: None,
            sequential: false,
            first_last_piece: false,
            content_layout: Some("Original"),
        })
        .await;
    engine.shutdown().await;
    assert!(
        answer.is_err(),
        "an existing output-directory symlink must be refused before add"
    );
    assert_eq!(
        std::fs::read_dir(outside).unwrap().count(),
        0,
        "engine wrote outside downloads through a symlink"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_runtime_cannot_share_the_same_engine_persistence() {
    let root = tempfile::tempdir().unwrap();
    let cfg = setup(root.path(), free_address(), free_address());
    let store = Store::open(&cfg.state_dir.join("state.sqlite")).unwrap();
    let first_config = cfg.embedded.unwrap();
    let first = Embedded::new(first_config.clone(), store.clone());
    first.start().await.unwrap();
    let mut second_config = first_config;
    // Different ports: socket collision must not accidentally stand in for a state lock.
    second_config.listen = free_address();
    let second = Embedded::new(second_config, store);
    let answer = second.start().await;
    second.shutdown().await;
    first.shutdown().await;
    assert!(
        answer.is_err(),
        "one state directory must have only one embedded runtime owner"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_download_mapping_cannot_resume_persisted_torrents_at_old_paths() {
    let root = tempfile::tempdir().unwrap();
    let cfg = setup(root.path(), free_address(), free_address());
    let store = Store::open(&cfg.state_dir.join("state.sqlite")).unwrap();
    let first_config = cfg.embedded.unwrap();
    let first = Embedded::new(first_config.clone(), store.clone());
    let torrent =
        fixture::build_torrent("Mapping Fixture", &[("mapping.mkv".into(), vec![7; 32768])]);
    first
        .add(&AddRequest {
            payload: Payload::Torrent(torrent.torrent),
            save_path: first_config.save_path.clone(),
            category: "review".into(),
            tags: "unified-media".into(),
            paused: true,
            ratio_limit: Some(2.0),
            seeding_time_limit_minutes: None,
            sequential: false,
            first_last_piece: false,
            content_layout: Some("Original"),
        })
        .await
        .unwrap();
    first.shutdown().await;
    drop(first);
    let mut changed = first_config;
    changed.download_dir = root.path().join("different-downloads");
    changed.save_path = changed.download_dir.to_string_lossy().into_owned();
    changed.listen = free_address();
    std::fs::create_dir(&changed.download_dir).unwrap();
    let second = Embedded::new(changed, store);
    let answer = second.start().await;
    second.shutdown().await;
    assert!(
        answer.is_err(),
        "changed download mapping must fail before persisted torrents resume"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tampered_persisted_output_folder_is_refused_before_restoration_writes() {
    let root = tempfile::tempdir().unwrap();
    let cfg = setup(root.path(), free_address(), free_address());
    let store = Store::open(&cfg.state_dir.join("state.sqlite")).unwrap();
    let first_config = cfg.embedded.unwrap();
    let first = Embedded::new(first_config.clone(), store.clone());
    let torrent = fixture::build_torrent(
        "Persistence Fixture",
        &[("persisted.mkv".into(), vec![7; 32768])],
    );
    first
        .add(&AddRequest {
            payload: Payload::Torrent(torrent.torrent),
            save_path: first_config.save_path.clone(),
            category: "review".into(),
            tags: "unified-media".into(),
            paused: true,
            ratio_limit: Some(2.0),
            seeding_time_limit_minutes: None,
            sequential: false,
            first_last_piece: false,
            content_layout: Some("Original"),
        })
        .await
        .unwrap();
    first.shutdown().await;
    drop(first);
    let outside = root.path().join("outside-persisted-downloads");
    std::fs::create_dir(&outside).unwrap();
    let path = first_config.session_dir.join("session/session.json");
    let mut data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let torrents = data["torrents"].as_object_mut().unwrap();
    assert_eq!(torrents.len(), 1);
    for row in torrents.values_mut() {
        row["output_folder"] = serde_json::json!(outside);
    }
    std::fs::write(path, serde_json::to_vec(&data).unwrap()).unwrap();
    let mut second_config = first_config;
    second_config.listen = free_address();
    let second = Embedded::new(second_config, store);
    let answer = second.start().await;
    second.shutdown().await;
    assert!(
        answer.is_err(),
        "persisted folders must be checked against app ownership before the engine restores them"
    );
    assert_eq!(
        std::fs::read_dir(outside).unwrap().count(),
        0,
        "restoration wrote outside the configured download root"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_download_root_is_refused_before_engine_start() {
    let root = tempfile::tempdir().unwrap();
    let cfg = setup(root.path(), free_address(), free_address());
    let outside = root.path().join("outside-root");
    std::fs::create_dir(&outside).unwrap();
    std::fs::remove_dir(root.path().join("downloads")).unwrap();
    std::os::unix::fs::symlink(&outside, root.path().join("downloads")).unwrap();
    let store = Store::open(&cfg.state_dir.join("state.sqlite")).unwrap();
    let engine = Embedded::new(cfg.embedded.unwrap(), store);
    let answer = engine.start().await;
    engine.shutdown().await;
    assert!(
        answer.is_err(),
        "download root and its ancestors need the same symlink confinement as files below it"
    );
    assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
}
