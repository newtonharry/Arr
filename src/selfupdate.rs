//! "Update now" in the web app.
//!
//! Arr never updates itself. The updater `install.sh` / `install.ps1` set up (the same one the
//! daily job runs, which checks the SHA-256 and rolls back if the new version doesn't start) does
//! the work, and Arr only asks for it:
//!
//! - Linux: Arr creates `update-requested` in its state folder, and a systemd `arr-update.path`
//!   unit starts the updater. macOS: Arr creates `update-queue/requested`, which the launchd
//!   job's `QueueDirectories` watches. Either way the updater deletes the file first thing, so a
//!   failed run can't loop.
//! - Windows: Arr starts the "Arr update" scheduled task.
//!
//! The updater writes `update-status.json` (state, from, to, time) for Arr to show. Nothing the
//! browser sends decides what runs: no version, no address.
//!
//! An install made before this existed has neither the watcher nor an updater that knows the
//! files, so Arr reports `can_update: false` until the installer runs again.

use crate::upstream;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const TRIGGER_FILE: &str = "update-requested";
/// launchd's QueueDirectories starts the job while this folder has a file in it, and (unlike
/// WatchPaths, which also fires when the file is deleted) not again once the updater has
/// emptied it.
pub const TRIGGER_QUEUE: &str = "update-queue/requested";
pub const STATUS_FILE: &str = "update-status.json";
const REPO: &str = "newtonharry/Arr";
const LATEST_URL: &str = "https://api.github.com/repos/newtonharry/Arr/releases/latest";
/// The updater scripts carry this word when they know the trigger and status files.
const MARKER: &str = "arr-update-trigger";
/// How long a release answer is kept; a failed check is tried again sooner.
const KEEP: Duration = Duration::from_secs(6 * 3600);
const KEEP_FAILURE: Duration = Duration::from_secs(15 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// No second request while one was made this recently.
const PENDING: Duration = Duration::from_secs(120);
/// A status of "checking" or "updating" older than this is a run that died.
const STALE_SECS: i64 = 30 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Latest {
    /// Without the leading `v`.
    pub version: String,
}

impl Latest {
    pub fn release_url(&self) -> String {
        format!("https://github.com/{REPO}/releases/tag/v{}", self.version)
    }
}

/// `(major, minor, patch)` of `1.2.3` or `v1.2.3`; anything else (a pre-release, a word) is None.
fn triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().strip_prefix('v').unwrap_or(v.trim()).split('.');
    let part = |s: Option<&str>| {
        let s = s?;
        (!s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    let t = (part(it.next())?, part(it.next())?, part(it.next())?);
    it.next().is_none().then_some(t)
}

/// Whether version `a` is newer than `b`.
pub fn newer(a: &str, b: &str) -> bool {
    matches!((triple(a), triple(b)), (Some(a), Some(b)) if a > b)
}

/// The release GitHub calls latest, from its API's answer. Only a plain `vX.Y.Z` tag counts.
pub fn parse_release(body: &str) -> Option<Latest> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = v.get("tag_name")?.as_str()?;
    triple(tag)?;
    Some(Latest {
        version: tag.trim_start_matches('v').to_string(),
    })
}

/// What the updater last wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    /// `idle`, `requested`, `checking`, `up_to_date`, `updating`, `failed` or `done`.
    pub state: String,
    pub from: Option<String>,
    pub to: Option<String>,
    /// RFC 3339.
    pub at: Option<String>,
}

const STATES: [&str; 5] = ["checking", "up_to_date", "updating", "failed", "done"];

fn version_text(v: &serde_json::Value) -> Option<String> {
    let s = v.as_str()?;
    triple(s).map(|_| s.trim_start_matches('v').to_string())
}

/// Reads `update-status.json`. A missing, unreadable or odd file is `None`, never an error.
pub fn read_status(state_dir: &Path, now: i64) -> Option<Status> {
    let text = std::fs::read_to_string(state_dir.join(STATUS_FILE)).ok()?;
    parse_status(&text, now)
}

pub fn parse_status(text: &str, now: i64) -> Option<Status> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let state = v.get("state")?.as_str()?;
    if !STATES.contains(&state) {
        return None;
    }
    let at = v.get("at").and_then(|a| a.as_str());
    let when = at.and_then(|a| {
        time::OffsetDateTime::parse(a, &time::format_description::well_known::Rfc3339).ok()
    });
    // A run that never finished is not running for ever.
    if matches!(state, "checking" | "updating")
        && when.is_none_or(|w| now - w.unix_timestamp() > STALE_SECS)
    {
        return Some(Status {
            state: "failed".into(),
            from: version_text(&v["from"]),
            to: version_text(&v["to"]),
            at: when.map(|_| at.unwrap_or_default().to_string()),
        });
    }
    Some(Status {
        state: state.into(),
        from: version_text(&v["from"]),
        to: version_text(&v["to"]),
        at: when.map(|_| at.unwrap_or_default().to_string()),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum RequestError {
    /// Auto-updates are off, or the install can't be told to update from here.
    Unavailable,
    /// Already up to date, as far as Arr knows.
    UpToDate,
    /// One was requested or is running.
    Busy,
    /// The service manager wouldn't start it.
    Failed(String),
}

pub struct Updates {
    state_dir: PathBuf,
    /// Where `arr-update` (or `arr-update.ps1`) lives (next to the arr program), and where the
    /// watcher's unit or plist would be.
    paths: Mutex<(PathBuf, Vec<PathBuf>)>,
    api_url: Mutex<String>,
    cache: tokio::sync::Mutex<Option<(Instant, Option<Latest>)>>,
    last_request: Mutex<Option<(Instant, i64)>>,
    client: reqwest::Client,
}

impl Updates {
    pub fn new(state_dir: &Path) -> Self {
        let bin_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        Self::with_paths(state_dir, &bin_dir, default_service_dirs())
    }

    pub fn with_paths(state_dir: &Path, bin_dir: &Path, service_dirs: Vec<PathBuf>) -> Self {
        Updates {
            state_dir: state_dir.to_path_buf(),
            paths: Mutex::new((bin_dir.to_path_buf(), service_dirs)),
            // The variable is for testing the page against a stand-in; it is the operator's, not
            // the browser's, and the updater it starts still reads GitHub itself.
            api_url: Mutex::new(
                std::env::var("ARR_RELEASES_API_URL")
                    .ok()
                    .filter(|u| u.starts_with("http"))
                    .unwrap_or_else(|| LATEST_URL.into()),
            ),
            cache: tokio::sync::Mutex::new(None),
            last_request: Mutex::new(None),
            client: upstream::http_client(CHECK_TIMEOUT, reqwest::redirect::Policy::none()),
        }
    }

    /// Where the updater and its watcher are looked for (tests point them at a temporary folder).
    pub fn set_paths(&self, bin_dir: &Path, service_dirs: Vec<PathBuf>) {
        *self.paths.lock().unwrap_or_else(|e| e.into_inner()) =
            (bin_dir.to_path_buf(), service_dirs);
    }

    /// Where the release check looks (tests point it at a mock).
    pub fn set_api_url(&self, url: &str) {
        *self.api_url.lock().unwrap_or_else(|e| e.into_inner()) = url.into();
    }

    fn trigger(&self) -> PathBuf {
        self.state_dir.join(if cfg!(target_os = "macos") {
            TRIGGER_QUEUE
        } else {
            TRIGGER_FILE
        })
    }

    fn updater(&self) -> PathBuf {
        let bin_dir = self
            .paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone();
        bin_dir.join(if cfg!(windows) {
            "arr-update.ps1"
        } else {
            "arr-update"
        })
    }

    /// Whether automatic updates are installed (their updater is there).
    pub fn auto_updates(&self) -> bool {
        self.updater().is_file()
    }

    /// Whether Arr can start the updater: auto-updates are on, the updater knows the trigger
    /// and status files, and the service manager is set to watch for the trigger.
    pub async fn can_update(&self) -> bool {
        if !self.updater_knows_the_trigger() {
            return false;
        }
        if cfg!(windows) {
            return task_exists().await;
        }
        self.watcher_installed()
    }

    fn updater_knows_the_trigger(&self) -> bool {
        std::fs::read_to_string(self.updater()).is_ok_and(|t| t.contains(MARKER))
    }

    fn watcher_installed(&self) -> bool {
        let dirs = self
            .paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .clone();
        dirs.iter().any(|d| {
            if cfg!(target_os = "macos") {
                std::fs::read_to_string(d.join("io.github.newtonharry.arr-update.plist"))
                    .is_ok_and(|t| t.contains("<key>QueueDirectories</key>"))
            } else {
                d.join("arr-update.path").is_file()
            }
        })
    }

    /// The latest release, from the cache or GitHub. `None` when it can't be told (offline, no
    /// answer in five seconds, an odd reply) or when auto-updates are off.
    pub async fn latest(&self) -> Option<Latest> {
        if !self.auto_updates() {
            return None;
        }
        let mut cache = self.cache.lock().await;
        if let Some((at, answer)) = &*cache
            && at.elapsed() < if answer.is_some() { KEEP } else { KEEP_FAILURE }
        {
            return answer.clone();
        }
        let url = self
            .api_url
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let answer = async {
            let r = self
                .client
                .get(url)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?;
            // A release answer is a few KB; never read more than this.
            if r.content_length().is_some_and(|n| n > 1 << 20) {
                return None;
            }
            parse_release(&r.text().await.ok()?)
        }
        .await;
        *cache = Some((Instant::now(), answer.clone()));
        answer
    }

    /// The updater's state, as the page shows it.
    pub fn status(&self, now: i64) -> Status {
        let last = *self.last_request.lock().unwrap_or_else(|e| e.into_inner());
        self.status_after(last, now)
    }

    /// `last` is when a request was last made: (this process's clock, unix seconds).
    fn status_after(&self, last: Option<(Instant, i64)>, now: i64) -> Status {
        let file = read_status(&self.state_dir, now);
        // "requested" from the request until the updater says anything (a status written at or
        // after it), or for two minutes if it never does.
        let waiting = last.is_some_and(|(t, at)| {
            t.elapsed() < PENDING
                && file
                    .as_ref()
                    .and_then(|f| f.at.as_deref())
                    .and_then(|a| {
                        time::OffsetDateTime::parse(
                            a,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .ok()
                    })
                    .is_none_or(|w| w.unix_timestamp() < at)
        });
        if self.trigger().exists() || waiting {
            return Status {
                state: "requested".into(),
                ..Default::default()
            };
        }
        file.unwrap_or_else(|| Status {
            state: "idle".into(),
            ..Default::default()
        })
    }

    /// Asks the service manager to run the updater. The caller has the signed-in check.
    pub async fn request(&self, current: &str, now: i64) -> Result<(), RequestError> {
        if !self.can_update().await {
            return Err(RequestError::Unavailable);
        }
        if let Some(l) = self.latest().await
            && !newer(&l.version, current)
        {
            return Err(RequestError::UpToDate);
        }
        // Held until the request is made, so two taps can't both pass the check.
        let mut last = self.last_request.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(
            self.status_after(*last, now).state.as_str(),
            "requested" | "checking" | "updating"
        ) {
            return Err(RequestError::Busy);
        }
        if cfg!(windows) {
            start_task().map_err(RequestError::Failed)?;
        } else {
            let path = self.trigger();
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = writeln!(f, "requested {now}");
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(RequestError::Busy);
                }
                Err(e) => {
                    return Err(RequestError::Failed(format!(
                        "couldn't write the request ({})",
                        e.kind()
                    )));
                }
            }
        }
        *last = Some((Instant::now(), now));
        Ok(())
    }
}

fn default_service_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![];
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/Library/LaunchDaemons"));
        if let Some(h) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(h).join("Library/LaunchAgents"));
        }
    } else {
        dirs.push(PathBuf::from("/etc/systemd/system"));
        if let Some(c) = std::env::var_os("XDG_CONFIG_HOME").filter(|c| !c.is_empty()) {
            dirs.push(PathBuf::from(c).join("systemd/user"));
        } else if let Some(h) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(h).join(".config/systemd/user"));
        }
    }
    dirs
}

async fn task_exists() -> bool {
    tokio::task::spawn_blocking(|| {
        std::process::Command::new("schtasks")
            .args(["/Query", "/TN", "Arr update"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
    .await
    .unwrap_or(false)
}

fn start_task() -> Result<(), String> {
    let out = std::process::Command::new("schtasks")
        .args(["/Run", "/TN", "Arr update"])
        .output()
        .map_err(|e| format!("couldn't run schtasks ({})", e.kind()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err("Windows wouldn't start the \"Arr update\" task".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rfc(secs: i64) -> String {
        time::OffsetDateTime::from_unix_timestamp(secs)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    #[test]
    fn versions_compare_as_numbers_and_odd_tags_are_ignored() {
        assert!(newer("0.12.1", "0.12.0"));
        assert!(newer("v0.13.0", "0.12.9"));
        assert!(newer("0.12.10", "0.12.9"));
        assert!(!newer("0.12.0", "0.12.0"));
        assert!(!newer("0.11.9", "0.12.0"));
        assert!(!newer("0.13.0-rc1", "0.12.0"));
        assert!(!newer("nightly", "0.12.0"));
    }

    #[test]
    fn the_release_answer_gives_a_plain_version_or_nothing() {
        let l = parse_release(
            r#"{"tag_name":"v0.12.1","html_url":"https://evil.example/x","assets":[]}"#,
        )
        .unwrap();
        assert_eq!(l.version, "0.12.1");
        // The link is built here, never taken from the answer.
        assert_eq!(
            l.release_url(),
            "https://github.com/newtonharry/Arr/releases/tag/v0.12.1"
        );
        for bad in [
            r#"{"tag_name":"nightly"}"#,
            r#"{"tag_name":"v1.2"}"#,
            r#"{"tag_name":"v1.2.3-beta"}"#,
            r#"{"message":"rate limited"}"#,
            "<html>",
            "",
        ] {
            assert_eq!(parse_release(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_status_file_is_read_loosely_and_a_dead_run_is_a_failure() {
        let now = 1_800_000_000;
        let s = parse_status(
            &format!(
                r#"{{"state":"done","from":"0.12.0","to":"v0.12.1","at":"{}","extra":1}}"#,
                rfc(now - 10)
            ),
            now,
        )
        .unwrap();
        assert_eq!(s.state, "done");
        assert_eq!(s.to.as_deref(), Some("0.12.1"));
        // Not one of the states, not JSON, or a version that isn't one.
        assert_eq!(parse_status(r#"{"state":"exploded"}"#, now), None);
        assert_eq!(parse_status("nope", now), None);
        let s = parse_status(r#"{"state":"done","from":"<script>","at":"x"}"#, now).unwrap();
        assert_eq!(s.from, None);
        // "updating" for over half an hour, or with no time, is a run that died.
        let old = format!(
            r#"{{"state":"updating","from":"0.12.0","to":"0.12.1","at":"{}"}}"#,
            rfc(now - 3600)
        );
        assert_eq!(parse_status(&old, now).unwrap().state, "failed");
        assert_eq!(
            parse_status(r#"{"state":"checking"}"#, now).unwrap().state,
            "failed"
        );
        let fresh = format!(r#"{{"state":"updating","at":"{}"}}"#, rfc(now - 60));
        assert_eq!(parse_status(&fresh, now).unwrap().state, "updating");
    }

    fn install(dir: &Path, marker: bool, watcher: bool) -> Updates {
        let bin = dir.join("bin");
        let svc = dir.join("svc");
        let state = dir.join("state");
        for d in [&bin, &svc, &state] {
            std::fs::create_dir_all(d).unwrap();
        }
        let script = if marker {
            format!("#!/bin/sh\n# {MARKER}\n")
        } else {
            "#!/bin/sh\n".into()
        };
        std::fs::write(bin.join("arr-update"), script).unwrap();
        if watcher {
            std::fs::write(svc.join("arr-update.path"), "[Path]\n").unwrap();
        }
        Updates::with_paths(&state, &bin, vec![svc])
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn arr_offers_the_update_only_when_the_whole_mechanism_is_installed() {
        let d = tempfile::tempdir().unwrap();
        // An install from before this existed: an updater that doesn't know the files.
        assert!(!install(d.path(), false, false).can_update().await);
        assert!(!install(d.path(), true, false).can_update().await);
        assert!(!install(d.path(), false, true).can_update().await);
        let u = install(d.path(), true, true);
        assert!(u.can_update().await);
        assert!(u.auto_updates());
        // No updater, no auto-updates.
        std::fs::remove_file(d.path().join("bin/arr-update")).unwrap();
        assert!(!u.auto_updates());
        assert!(!u.can_update().await);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_request_writes_the_trigger_once_and_a_second_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let u = install(d.path(), true, true);
        // GitHub can't be reached in a test; an unknown latest version doesn't block a request.
        u.set_api_url("http://127.0.0.1:9/");
        let now = 1_800_000_000;
        assert_eq!(u.status(now).state, "idle");
        u.request("0.12.0", now).await.unwrap();
        assert!(d.path().join("state").join(TRIGGER_FILE).is_file());
        assert_eq!(u.status(now).state, "requested");
        assert_eq!(u.request("0.12.0", now).await, Err(RequestError::Busy));
        // The updater deletes the trigger and reports; the in-memory guard still holds for a
        // while, so a double tap can't start a second run.
        std::fs::remove_file(d.path().join("state").join(TRIGGER_FILE)).unwrap();
        assert_eq!(u.request("0.12.0", now).await, Err(RequestError::Busy));
        std::fs::write(
            d.path().join("state").join(STATUS_FILE),
            format!(r#"{{"state":"updating","at":"{}"}}"#, rfc(now)),
        )
        .unwrap();
        assert_eq!(u.status(now).state, "updating");
        assert_eq!(u.request("0.12.0", now).await, Err(RequestError::Busy));
        // Once the run has finished, the next request goes through.
        std::fs::write(
            d.path().join("state").join(STATUS_FILE),
            format!(r#"{{"state":"failed","at":"{}"}}"#, rfc(now + 30)),
        )
        .unwrap();
        assert_eq!(u.status(now + 31).state, "failed");
        u.request("0.12.0", now + 31).await.unwrap();
        assert_eq!(u.status(now + 31).state, "requested");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn nothing_is_requested_when_it_cannot_be_done() {
        let d = tempfile::tempdir().unwrap();
        let u = install(d.path(), true, false);
        assert_eq!(u.request("0.12.0", 0).await, Err(RequestError::Unavailable));
        assert!(!d.path().join("state").join(TRIGGER_FILE).exists());
    }

    #[tokio::test]
    async fn the_release_check_is_cached_and_a_failure_means_unknown() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new().route(
            "/latest",
            axum::routing::get(move || {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    r#"{"tag_name":"v0.12.1"}"#
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let d = tempfile::tempdir().unwrap();
        let u = install(d.path(), true, true);
        u.set_api_url(&format!("http://{addr}/latest"));
        for _ in 0..3 {
            assert_eq!(u.latest().await.unwrap().version, "0.12.1");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "answered from the cache");
        // Nothing is asked when auto-updates are off.
        std::fs::remove_file(d.path().join("bin/arr-update")).unwrap();
        assert_eq!(u.latest().await, None);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // An unreachable GitHub is unknown, and remembered for a short while only.
        let u = install(d.path(), true, true);
        u.set_api_url("http://127.0.0.1:9/");
        assert_eq!(u.latest().await, None);
    }
}
