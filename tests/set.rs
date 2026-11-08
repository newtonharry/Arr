//! `arr set`: changing one setting of an install that's already set up, against mock Plex and
//! TMDB servers. A change that doesn't work leaves every file exactly as it was.

use axum::Router;
use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_arr");

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A Plex that accepts tokens starting with `accept`, with empty libraries.
async fn start_plex(accept: &'static str) -> SocketAddr {
    let ok = move |h: &HeaderMap| {
        let token = h.get("x-plex-token").and_then(|v| v.to_str().ok());
        token.is_some_and(|t| t.starts_with(accept))
    };
    serve(
        Router::new()
            .route(
                "/identity",
                get(|| async { r#"{"MediaContainer":{"machineIdentifier":"machine-1"}}"# }),
            )
            .route(
                "/library/sections",
                get(move |h: HeaderMap| async move {
                    if ok(&h) {
                        (StatusCode::OK, r#"{"MediaContainer":{"Directory":[]}}"#)
                    } else {
                        (StatusCode::UNAUTHORIZED, "")
                    }
                }),
            )
            .route(
                "/library/sections/{id}/all",
                get(move |h: HeaderMap| async move {
                    if ok(&h) {
                        (
                            StatusCode::OK,
                            r#"{"MediaContainer":{"size":0,"totalSize":0,"offset":0,"Metadata":[]}}"#,
                        )
                    } else {
                        (StatusCode::UNAUTHORIZED, "")
                    }
                }),
            ),
    )
    .await
}

async fn start_tmdb() -> SocketAddr {
    serve(Router::new().route(
        "/3/movie/603",
        get(|Query(q): Query<HashMap<String, String>>| async move {
            if q.get("api_key").map(String::as_str) == Some("good-key") {
                (StatusCode::OK, r#"{"id":603,"title":"The Matrix"}"#)
            } else {
                (StatusCode::UNAUTHORIZED, r#"{"status_code":7}"#)
            }
        }),
    ))
    .await
}

fn write_private(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// `arr init`, with TMDB at `tmdb` and `extra` added to the config. Returns the config path.
fn install(dir: &Path, tmdb: SocketAddr, extra: &str) -> PathBuf {
    let state = dir.join("state");
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
    write_private(&config, format!("{text}{extra}").as_bytes());
    config
}

/// The full install: Plex, Arr's own downloader, a library and an indexer.
fn full_install(dir: &Path, tmdb: SocketAddr, plex: SocketAddr) -> PathBuf {
    let config = install(
        dir,
        tmdb,
        &format!(
            r#"
[plex]
base_url = "http://{plex}"
token_file = "secrets/plex_token"

[embedded]
download_dir = {dl:?}
listen = "0.0.0.0:6881"

[library]
movies = {movies:?}

[[indexer]]
id = 1
name = "The Pirate Bay"
kind = "thepiratebay"
base_url = "https://apibay.org/"
"#,
            dl = dir.join("dl").display().to_string(),
            movies = dir.join("movies").display().to_string(),
        ),
    );
    write_private(&config.with_file_name("secrets/plex_token"), b"good-token");
    config
}

fn set(config: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(BIN)
        .args(["set", "--config"])
        .arg(config)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(s) = stdin {
        // As PowerShell pipes it: with a line end.
        write!(child.stdin.take().unwrap(), "{s}\r\n").unwrap();
    }
    child.wait_with_output().unwrap()
}

/// Succeeded, saying exactly `line`.
fn says(out: &Output, line: &str) {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{line}\n"));
}

/// Failed with one `Error:` line containing `why`, and printed nothing on stdout.
fn refused(out: &Output, why: &str) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(out.stdout.is_empty());
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(err.starts_with("Error: ") && err.contains(why), "{err}");
}

fn table(config: &Path) -> toml::Table {
    toml::from_str(&std::fs::read_to_string(config).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")] // the mocks answer while this thread waits on arr
async fn plex_token_and_address_are_tried_before_they_are_kept() {
    let (tmdb, plex) = (start_tmdb().await, start_plex("good").await);
    let other_plex = start_plex("other").await;
    let tmp = tempfile::tempdir().unwrap();
    let config = full_install(tmp.path(), tmdb, plex);
    let token = config.with_file_name("secrets/plex_token");
    let before = std::fs::read(&config).unwrap();

    refused(
        &set(&config, &["plex-token"], Some("bad-token")),
        "Plex didn't accept that token",
    );
    assert_eq!(std::fs::read(&config).unwrap(), before);
    assert_eq!(std::fs::read(&token).unwrap(), b"good-token");

    says(
        &set(&config, &["plex-token"], Some("good-token-2")),
        "Saved the Plex token. Restart Arr to use it.",
    );
    assert_eq!(std::fs::read(&token).unwrap(), b"good-token-2");
    assert_eq!(std::fs::read(&config).unwrap(), before);

    // A token on the command line would show in the process list.
    refused(
        &set(&config, &["plex-token", "good-token-3"], None),
        "on stdin",
    );

    // A server that isn't there, or isn't Plex, changes nothing.
    refused(
        &set(&config, &["plex-url", "http://127.0.0.1:1"], None),
        "couldn't reach Plex",
    );
    refused(
        &set(&config, &["plex-url", "ftp://x"], None),
        "http or https",
    );
    // A wrong port or a protected proxy refuses the token too, but isn't Plex.
    let proxy = serve(Router::new().fallback(|| async { StatusCode::UNAUTHORIZED })).await;
    refused(
        &set(&config, &["plex-url", &format!("http://{proxy}/")], None),
        "didn't answer like a Plex server",
    );
    assert_eq!(std::fs::read(&config).unwrap(), before);

    // A Plex that refuses the saved token is still Plex: the address is kept, with a note
    // (the installer then signs in again or sets a new token).
    let other = format!("http://{other_plex}/");
    says(
        &set(&config, &["plex-url", &other], None),
        &format!(
            "Changed the Plex address to {other}. Restart Arr to use it. Plex refused the saved sign-in there; run `arr plex-signin --config {}` or set a new token.",
            config.display()
        ),
    );
    assert_eq!(
        table(&config)["plex"]["base_url"].as_str(),
        Some(other.as_str())
    );

    let url = format!("http://localhost:{}", plex.port());
    says(
        &set(&config, &["plex-url", &url], None),
        &format!("Changed the Plex address to {url}/. Restart Arr to use it."),
    );
    assert_eq!(
        table(&config)["plex"]["base_url"].as_str(),
        Some(format!("{url}/").as_str())
    );
}

#[tokio::test(flavor = "multi_thread")] // the mock answers while this thread waits on arr
async fn a_tmdb_key_is_tried_before_it_is_kept() {
    let tmdb = start_tmdb().await;
    let tmp = tempfile::tempdir().unwrap();
    let config = install(tmp.path(), tmdb, "");
    let before = std::fs::read(&config).unwrap();
    let secret = config.with_file_name("secrets/tmdb_credential");

    refused(
        &set(&config, &["tmdb-key"], Some("wrong-key")),
        "TMDB didn't accept that key",
    );
    assert_eq!(std::fs::read(&config).unwrap(), before);
    assert!(!secret.exists(), "a new secret file is removed again");

    says(
        &set(&config, &["tmdb-key"], Some("good-key")),
        "Saved the TMDB key. Restart Arr to use it.",
    );
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "good-key");
    assert_eq!(table(&config)["tmdb"]["enabled"].as_bool(), Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn simple_settings_must_still_load() {
    let tmdb = start_tmdb().await;
    let tmp = tempfile::tempdir().unwrap();

    // Without a downloader, downloading can't be turned on and there's no upload limit.
    let bare = install(&tmp.path().join("bare"), tmdb, "");
    let before = std::fs::read(&bare).unwrap();
    refused(
        &set(&bare, &["downloading", "on"], None),
        "Arr wouldn't start with that change",
    );
    refused(
        &set(&bare, &["upload-limit-kib", "500"], None),
        "Arr's own downloader",
    );
    refused(&set(&bare, &["mcp-token"], None), "no MCP token file");
    refused(&set(&bare, &["downloading", "maybe"], None), "on or off");
    refused(&set(&bare, &["downloading"], None), "needs a value");
    refused(
        &set(&bare, &["colour", "blue"], None),
        "tmdb-key, plex-token",
    );
    assert_eq!(std::fs::read(&bare).unwrap(), before);

    let plex = start_plex("good").await;
    let config = full_install(&tmp.path().join("full"), tmdb, plex);
    says(
        &set(&config, &["downloading", "on"], None),
        "Turned downloading on. Restart Arr to use it.",
    );
    assert_eq!(table(&config)["execution"]["enabled"].as_bool(), Some(true));
    says(
        &set(&config, &["upload-limit-kib", "500"], None),
        "Changed the upload limit to 500 KiB/s. Restart Arr to use it.",
    );
    assert_eq!(
        table(&config)["embedded"]["upload_limit_bytes_per_sec"].as_integer(),
        Some(512_000)
    );
    refused(
        &set(&config, &["upload-limit-kib", "99999999"], None),
        "whole number",
    );
    says(
        &set(&config, &["upload-limit-kib", "0"], None),
        "Removed the upload limit. Restart Arr to use it.",
    );
    assert!(
        !table(&config)["embedded"]
            .as_table()
            .unwrap()
            .contains_key("upload_limit_bytes_per_sec")
    );
    refused(&set(&config, &["torrent-port", "0"], None), "1 to 65535");
    says(
        &set(&config, &["torrent-port", "51413"], None),
        "Changed the torrent port to 51413. Restart Arr to use it.",
    );
    assert_eq!(
        table(&config)["embedded"]["listen"].as_str(),
        Some("0.0.0.0:51413")
    );
    says(
        &set(&config, &["error-reports", "on"], None),
        "Turned error reports on. Restart Arr to use it.",
    );
    assert_eq!(table(&config)["reporting"]["enabled"].as_bool(), Some(true));

    let token = config.with_file_name("secrets/api_token");
    let old = std::fs::read_to_string(&token).unwrap();
    let out = set(&config, &["api-token"], None);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("update anything that uses it"));
    let new = std::fs::read_to_string(&token).unwrap();
    assert!(new != old && new.len() == 64);
    // Comments the installer wrote are still there.
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("# Native indexers, searched directly")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_reports_and_fails_when_a_run_fails() {
    let (tmdb, plex) = (start_tmdb().await, start_plex("good").await);
    let tmp = tempfile::tempdir().unwrap();
    // No [plex] movie_section, so the movies run can't start.
    let config = full_install(tmp.path(), tmdb, plex);
    let setup = Command::new(BIN)
        .args(["setup", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let out = Command::new(BIN)
        .args(["bootstrap", "--media", "movies", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["runs"][0]["status"], "failed", "{report}");
    let err = String::from_utf8_lossy(&out.stderr);
    let last = err.lines().last().unwrap();
    assert!(
        last.starts_with("Error: ") && last.contains("movies: no Plex section"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_the_line_editor_cant_make_are_refused() {
    let tmdb = start_tmdb().await;
    let tmp = tempfile::tempdir().unwrap();
    let plex = start_plex("good").await;
    let config = full_install(tmp.path(), tmdb, plex);
    let text = std::fs::read_to_string(&config).unwrap();

    // `download_submission` is the old name of `enabled`: it goes, or both would be there.
    let legacy = text.replacen(
        "\nenabled = false\ndownload_cleanup",
        "\ndownload_submission = false\ndownload_cleanup",
        1,
    );
    assert_ne!(legacy, text);
    write_private(&config, legacy.as_bytes());
    says(
        &set(&config, &["downloading", "on"], None),
        "Turned downloading on. Restart Arr to use it.",
    );
    let execution = table(&config)["execution"].as_table().unwrap().clone();
    assert_eq!(execution["enabled"].as_bool(), Some(true));
    assert!(!execution.contains_key("download_submission"));

    // A dotted key: the edit would say it twice, so nothing changes.
    let dotted = format!("reporting.enabled = false\n{text}");
    write_private(&config, dotted.as_bytes());
    refused(
        &set(&config, &["error-reports", "on"], None),
        "couldn't change error-reports automatically; edit enabled under [reporting]",
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), dotted);

    // Under sudo a new secrets folder would belong to root.
    let secrets = config.with_file_name("secrets");
    std::fs::rename(&secrets, tmp.path().join("moved")).unwrap();
    refused(
        &set(&config, &["tmdb-key"], Some("good-key")),
        "the secrets folder",
    );
    assert!(!secrets.exists());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), dotted);
}

#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_reads_the_libraries_the_config_has() {
    let (tmdb, plex) = (start_tmdb().await, start_plex("good").await);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("movies")).unwrap();
    let config = full_install(tmp.path(), tmdb, plex);
    let movies_only = |section: u32| {
        let text = std::fs::read_to_string(&config).unwrap();
        // Not the template's commented-out example.
        let text = match text.find("\nmovie_section = ").map(|i| i + 1) {
            Some(i) => {
                let end = i + text[i..].find('\n').unwrap();
                format!("{}movie_section = {section}{}", &text[..i], &text[end..])
            }
            None => text.replacen(
                "\ntoken_file = \"secrets/plex_token\"\n",
                &format!("\ntoken_file = \"secrets/plex_token\"\nmovie_section = {section}\nmovies_path = \"/data/movies\"\n"),
                1,
            ),
        };
        write_private(&config, text.as_bytes());
    };
    movies_only(1);
    let setup = Command::new(BIN)
        .args(["setup", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let bootstrap = |args: &[&str]| {
        let out = Command::new(BIN)
            .arg("bootstrap")
            .args(args)
            .arg("--config")
            .arg(&config)
            .output()
            .unwrap();
        let report: serde_json::Value =
            serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
        (out, report)
    };
    // No --media: just the movie library, the only one there is.
    let (out, report) = bootstrap(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(report["runs"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(report["runs"][0]["media"], "movie", "{report}");
    assert_eq!(report["runs"][0]["section"], 1, "{report}");
    // Asking for TV, which isn't set up, still fails.
    let (out, _) = bootstrap(&["--media", "tv"]);
    assert_eq!(out.status.code(), Some(1));

    // Setup retried with another library: its run starts over, not the old one's.
    movies_only(3);
    let (out, report) = bootstrap(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(report["runs"][0]["section"], 3, "{report}");
    assert_eq!(report["runs"][0]["status"], "done", "{report}");
}
