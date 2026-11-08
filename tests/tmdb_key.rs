//! `arr tmdb-key`: adding a TMDB key to an install that's already set up, against a mock TMDB.

use axum::Router;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::routing::get;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_arr");

async fn start_tmdb() -> std::net::SocketAddr {
    let app = Router::new().route(
        "/3/movie/603",
        get(|Query(q): Query<HashMap<String, String>>| async move {
            if q.get("api_key").map(String::as_str) == Some("good-key") {
                (StatusCode::OK, r#"{"id":603,"title":"The Matrix"}"#)
            } else {
                (StatusCode::UNAUTHORIZED, r#"{"status_code":7}"#)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn tmdb_key(config: &Path, key: &str) -> std::process::Output {
    let mut child = Command::new(BIN)
        .args(["tmdb-key", "--config"])
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // As PowerShell pipes it: with a line end.
    write!(child.stdin.take().unwrap(), "{key}\r\n").unwrap();
    child.wait_with_output().unwrap()
}

#[tokio::test(flavor = "multi_thread")] // the mock answers while this thread waits on arr
async fn a_key_added_after_setup_is_checked_saved_and_turned_on() {
    let tmdb = start_tmdb().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let ok = Command::new(BIN)
        .args(["init", "--state-dir"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(ok.status.success());
    let config = state.join("config.toml");
    let text = std::fs::read_to_string(&config)
        .unwrap()
        .replace("https://api.themoviedb.org", &format!("http://{tmdb}"));
    std::fs::write(&config, &text).unwrap();
    let secret = state.join("secrets/tmdb_credential");

    // A key TMDB refuses changes nothing.
    let out = tmdb_key(&config, "wrong-key");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("TMDB didn't accept that key"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    assert!(!secret.exists());

    let out = tmdb_key(&config, "good-key");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "good-key");
    let parsed: toml::Table = toml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(parsed["tmdb"]["enabled"].as_bool(), Some(true));
    assert_eq!(
        parsed["tmdb"]["credential_file"].as_str(),
        Some("secrets/tmdb_credential")
    );
    let check = Command::new(BIN)
        .args(["check", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );

    // A later wrong key keeps the working one.
    assert!(!tmdb_key(&config, "wrong-again").status.success());
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "good-key");
    assert_eq!(
        toml::from_str::<toml::Table>(&std::fs::read_to_string(&config).unwrap()).unwrap()["tmdb"]
            ["enabled"]
            .as_bool(),
        Some(true)
    );
}
