//! `arr plex-signin`: repairing a revoked Plex sign-in, against a mock plex.tv and Plex.

use axum::Router;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::json;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

const BIN: &str = env!("CARGO_BIN_EXE_arr");
const ACCOUNT_TOKEN: &str = "account-token-0123456789";
const NEW_TOKEN: &str = "server-token-new-0123456789";
const OLD_TOKEN: &str = "server-token-old-0123456789";
const MACHINE: &str = "machine-0123456789";

#[derive(Default)]
struct World {
    pin_polls: u32,
    /// The machine the configured Plex says it is.
    identity: String,
}

type Mock = Arc<Mutex<World>>;

fn token(h: &HeaderMap) -> &str {
    h.get("x-plex-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

async fn start_mock() -> (SocketAddr, Mock) {
    let m: Mock = Arc::new(Mutex::new(World {
        identity: MACHINE.into(),
        ..Default::default()
    }));
    let app = Router::new()
        .route(
            "/api/v2/pins",
            post(|| async { axum::Json(json!({ "id": 7, "code": "pin-code" })) }),
        )
        .route(
            "/api/v2/pins/{id}",
            get(
                |State(m): State<Mock>, AxPath(id): AxPath<u64>| async move {
                    if id != 7 {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    let mut w = m.lock().unwrap();
                    w.pin_polls += 1;
                    // plex.tv has a bad moment, then the sign-in finishes.
                    if w.pin_polls == 1 {
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    let t = (w.pin_polls > 2).then_some(ACCOUNT_TOKEN);
                    axum::Json(json!({ "id": 7, "code": "pin-code", "authToken": t }))
                        .into_response()
                },
            ),
        )
        .route(
            "/api/v2/resources",
            get(|h: HeaderMap| async move {
                if token(&h) != ACCOUNT_TOKEN {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(json!([
                    { "name": "Other", "provides": "server", "owned": true,
                      "clientIdentifier": "other", "accessToken": "other-token" },
                    { "name": "Living Room", "provides": "server", "owned": true,
                      "clientIdentifier": MACHINE, "accessToken": NEW_TOKEN },
                ]))
                .into_response()
            }),
        )
        .route(
            "/identity",
            get(|State(m): State<Mock>| async move {
                let id = m.lock().unwrap().identity.clone();
                axum::Json(
                    json!({ "MediaContainer": { "machineIdentifier": id, "claimed": true } }),
                )
            }),
        )
        .route(
            "/library/sections",
            get(|h: HeaderMap| async move {
                // The old token was revoked.
                if token(&h) != NEW_TOKEN {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(json!({ "MediaContainer": { "Directory": [] } })).into_response()
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, m)
}

/// An installed Arr's state folder, with its Plex at `plex`; returns the config's path.
fn install(dir: &Path, plex: &str, token: &str) -> PathBuf {
    arr::fsutil::ensure_private_dir(&dir.join("secrets")).unwrap();
    for (f, v) in [
        ("secrets/plex_token", token),
        ("secrets/plex_client_id", "client-0123"),
        ("secrets/api_token", "api-token-0123456789-0123456789"),
    ] {
        arr::fsutil::write_new_private_file(&dir.join(f), v.as_bytes()).unwrap();
    }
    let config = dir.join("config.toml");
    let text = format!(
        "# mine\n[server]\napi_token_file = \"secrets/api_token\"\n\n[plex]\nbase_url = \"{plex}\"\ntoken_file = \"secrets/plex_token\"\nmovie_section = 1\n"
    );
    arr::fsutil::write_new_private_file(&config, text.as_bytes()).unwrap();
    config
}

/// Every file under `dir` with its contents.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(files(&p));
        } else {
            out.push((p.clone(), std::fs::read(&p).unwrap()));
        }
    }
    out.sort();
    out
}

async fn check(config: &Path) -> std::process::Output {
    tokio::process::Command::new(BIN)
        .args(["plex-signin", "--check", "--config"])
        .arg(config)
        .output()
        .await
        .unwrap()
}

/// Runs the sign-in, checks the link it prints, and returns its exit code and errors.
async fn signin(config: &Path, mock: SocketAddr) -> (i32, String) {
    let mut child = tokio::process::Command::new(BIN)
        .args(["plex-signin", "--config"])
        .arg(config)
        .arg(format!("--plex-tv-url=http://{mock}/"))
        .arg(format!("--plex-auth-url=http://{mock}/auth"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = tokio::time::timeout(Duration::from_secs(20), out.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let want = format!(
        "Open this link to sign in to Plex: http://{mock}/auth#?clientID=client-0123&code=pin-code&"
    );
    assert!(first.starts_with(&want), "{first}");
    let rest = tokio::time::timeout(Duration::from_secs(60), async {
        let mut rest = vec![];
        while let Some(l) = out.next_line().await.unwrap() {
            rest.push(l);
        }
        rest
    })
    .await
    .unwrap();
    let done = child.wait_with_output().await.unwrap();
    let err = String::from_utf8_lossy(&done.stderr).into_owned();
    if done.status.success() {
        assert_eq!(
            rest,
            ["Signed in to Plex again. Restart Arr to use it."],
            "{err}"
        );
    } else {
        assert!(rest.is_empty(), "{rest:?}");
        assert_eq!(err.lines().count(), 1, "{err}");
        assert!(err.starts_with("Error: "), "{err}");
    }
    (done.status.code().unwrap(), err)
}

#[tokio::test(flavor = "multi_thread")]
async fn check_says_whether_plex_still_accepts_the_sign_in() {
    let (mock, _) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();

    let ok = install(
        &tmp.path().join("ok"),
        &format!("http://{mock}/"),
        NEW_TOKEN,
    );
    let out = check(&ok).await;
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "Plex accepts Arr's sign-in.\n"
    );

    let revoked = install(
        &tmp.path().join("revoked"),
        &format!("http://{mock}/"),
        OLD_TOKEN,
    );
    let out = check(&revoked).await;
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("Error: Plex no longer accepts Arr's sign-in") && err.lines().count() == 1,
        "{err}"
    );

    // Nothing listens on port 9.
    let down = install(&tmp.path().join("down"), "http://127.0.0.1:9/", NEW_TOKEN);
    let out = check(&down).await;
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("Error: ") && err.lines().count() == 1,
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn signing_in_again_replaces_only_the_plex_token() {
    let (mock, _) = start_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let config = install(tmp.path(), &format!("http://{mock}/"), OLD_TOKEN);
    let before = files(tmp.path());

    let (code, err) = signin(&config, mock).await;
    assert_eq!(code, 0, "{err}");
    let token = tmp.path().join("secrets/plex_token");
    assert_eq!(std::fs::read_to_string(&token).unwrap(), NEW_TOKEN);
    let mut want: Vec<_> = before
        .into_iter()
        .map(|(p, v)| {
            let v = if p == token { NEW_TOKEN.into() } else { v };
            (p, v)
        })
        .collect();
    want.sort();
    assert_eq!(files(tmp.path()), want);
    assert_eq!(check(&config).await.status.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn another_server_changes_nothing() {
    let (mock, world) = start_mock().await;
    // The Plex in the config isn't one of the account's servers.
    world.lock().unwrap().identity = "someone-else".into();
    let tmp = tempfile::tempdir().unwrap();
    let config = install(tmp.path(), &format!("http://{mock}/"), OLD_TOKEN);
    let before = files(tmp.path());

    let (code, err) = signin(&config, mock).await;
    assert_eq!(code, 1);
    assert!(
        err.contains("isn't one of this Plex account's servers"),
        "{err}"
    );
    assert_eq!(files(tmp.path()), before);
}
