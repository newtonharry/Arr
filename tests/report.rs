//! Error reports (docs/PRIVACY.md) end to end: the real `arr report` against a mock Sentry
//! that records every envelope it's sent.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_arr");

type Received = Arc<Mutex<Vec<String>>>;

async fn start_sentry() -> (SocketAddr, Received) {
    let got: Received = Arc::default();
    let app = Router::new()
        .fallback(|State(got): State<Received>, body: Bytes| async move {
            got.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).into_owned());
            "{}"
        })
        .with_state(got.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, got)
}

async fn report(config: &Path, sentry: SocketAddr, message: &str) {
    let status = tokio::process::Command::new(BIN)
        .args(["report", "--component", "installer", "--message", message])
        .arg("--config")
        .arg(config)
        .env("ARR_SENTRY_DSN", format!("http://key@{sentry}/1"))
        .status()
        .await
        .unwrap();
    // Reporting never fails the thing that's already failing.
    assert!(status.success());
}

const MESSAGE: &str = "couldn't import \"The Matrix (1999)\" from /home/someone/Movies \
    for someone@example.com at 192.168.1.20:32400: token=abcdef0123456789xyz";

#[tokio::test]
async fn a_report_is_sent_scrubbed_only_when_the_config_allows_it() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.toml");
    let (sentry, got) = start_sentry().await;

    // No config yet, then a config without the section, then one that says no.
    report(&config, sentry, MESSAGE).await;
    std::fs::write(&config, "[server]\nlisten = \"127.0.0.1:1\"\n").unwrap();
    report(&config, sentry, MESSAGE).await;
    std::fs::write(&config, "[reporting]\nenabled = false\n").unwrap();
    report(&config, sentry, MESSAGE).await;
    assert!(got.lock().unwrap().is_empty(), "sent without consent");

    std::fs::write(&config, "[reporting]\nenabled = true\n").unwrap();
    report(&config, sentry, MESSAGE).await;
    // `arr report` waits for the send before exiting, so it's already here.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let got = got.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{got:?}");
    let envelope = &got[0];
    for gone in [
        "The Matrix",
        "/home/someone",
        "someone@example.com",
        "192.168.1.20",
        "abcdef0123456789xyz",
    ] {
        assert!(!envelope.contains(gone), "{gone:?} was sent: {envelope}");
    }
    assert!(envelope.contains("couldn't import"), "{envelope}");
    assert!(
        envelope.contains(r#""component":"installer""#),
        "{envelope}"
    );
    assert!(
        envelope.contains(concat!("arr@", env!("CARGO_PKG_VERSION"))),
        "{envelope}"
    );
    let host = std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| h.len() > 3);
    if let Some(host) = host {
        assert!(!envelope.contains(&host), "the computer's name was sent");
    }
}
