//! What the web app's Settings screen uses: "Update now", a few settings anyone in the
//! household may change from a phone, and a plain-words health summary. Nothing here returns a
//! secret, a path or an address, and nothing the browser sends decides what runs.

use super::*;
use crate::selfupdate::{RequestError, newer};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Disk space below which the summary is amber and red.
const LOW_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const VERY_LOW_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// How long a check of another program may take.
const PROBE: Duration = Duration::from_secs(4);

fn unix_now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

// ---------------------------------------------------------------- update

async fn update_view(st: &AppState) -> Value {
    let latest = st.updates.latest().await;
    let available = latest.as_ref().is_some_and(|l| newer(&l.version, VERSION));
    let can_update = st.updates.can_update().await;
    json!({
        "current": VERSION,
        "latest": latest.as_ref().map(|l| l.version.clone()),
        "available": available,
        "can_update": can_update,
        "auto_updates": st.updates.auto_updates(),
        "release_url": latest.as_ref().filter(|_| available).map(|l| l.release_url()),
        "status": st.updates.status(unix_now()),
    })
}

/// `GET /update`: the running version, the latest release (checked at most every six hours,
/// and only when automatic updates are installed) and what the updater last did.
pub(super) async fn update_get(State(st): St) -> ApiResult {
    ok(update_view(&st).await)
}

/// `POST /update`: asks the installed updater to run. It takes no body: the updater decides
/// what to install, from GitHub's latest release.
pub(super) async fn update_post(State(st): St) -> ApiResult {
    match st.updates.request(VERSION, unix_now()).await {
        Ok(()) => Ok((StatusCode::ACCEPTED, Json(update_view(&st).await)).into_response()),
        Err(RequestError::Unavailable) => Err(ApiError::conflict(
            ErrorCode::InvalidRequest,
            "this install can't update from here; run the installer again on the computer",
        )),
        Err(RequestError::UpToDate) => Err(ApiError::conflict(
            ErrorCode::InvalidRequest,
            "Arr is already up to date",
        )),
        Err(RequestError::Busy) => Err(ApiError::conflict(
            ErrorCode::InvalidRequest,
            "an update is already on its way",
        )),
        Err(RequestError::Failed(why)) => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            ErrorCode::UpstreamUnavailable,
            why,
        )),
    }
}

// ---------------------------------------------------------------- settings

fn app_settings_view(st: &AppState) -> Value {
    let e = st.cfg.embedded.as_ref();
    json!({
        "downloading": st.cfg.execution_enabled,
        "embedded": e.is_some(),
        "upload_limit_kib": e.and_then(|e| e.upload_bps).map(|b| b.get() / 1024),
        "torrent_port": e.map(|e| e.listen.port()),
        "error_reports": st.cfg.reporting,
        "error_reports_available": crate::report::available(),
        "tmdb_key_set": st.cfg.tmdb.as_ref().is_some_and(|t| t.credential.is_some()),
        "can_restart": crate::lifecycle::supervised(),
    })
}

/// `GET /settings/app`: the few settings the app can change, never a secret.
pub(super) async fn app_settings_get(State(st): St) -> ApiResult {
    ok(app_settings_view(&st))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AppSettingBody {
    name: String,
    value: String,
}

/// The settings the app may change. Not here, on purpose: the Plex token (signing in owns it),
/// the API and MCP tokens (new ones break connected programs) and the Plex address (a wrong
/// one cuts Arr off from Plex). Those stay with `arr set` on the computer.
const CHANGEABLE: [&str; 5] = [
    "downloading",
    "upload-limit-kib",
    "torrent-port",
    "error-reports",
    "tmdb-key",
];

fn switch(v: &str) -> Option<bool> {
    match v {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// Whether `value` is what the running config already has.
fn unchanged(st: &AppState, name: &str, value: &str) -> bool {
    let e = st.cfg.embedded.as_ref();
    match name {
        "downloading" => switch(value) == Some(st.cfg.execution_enabled),
        "error-reports" => switch(value) == Some(st.cfg.reporting),
        "upload-limit-kib" => {
            value.parse::<u32>().ok()
                == Some(e.and_then(|e| e.upload_bps).map_or(0, |b| b.get() / 1024))
        }
        "torrent-port" => {
            value.parse::<u16>().ok().filter(|p| *p != 0) == e.map(|e| e.listen.port())
        }
        _ => false,
    }
}

fn say(name: &str, value: &str) -> String {
    let on = |v: &str| if v == "on" { "on" } else { "off" };
    match name {
        "downloading" => format!("Downloading is {}.", on(value)),
        "error-reports" => format!("Error reports are {}.", on(value)),
        "upload-limit-kib" if value.trim() == "0" => "The upload limit is gone.".into(),
        "upload-limit-kib" => format!("The upload limit is {} KiB/s.", value.trim()),
        "torrent-port" => format!("The torrent port is {}.", value.trim()),
        _ => "Saved the TMDB key.".into(),
    }
}

/// A new torrent port must not be Arr's own HTTP port and must be free to bind where the
/// engine listens (TCP, the only transport it uses). Otherwise Arr would exit to restart and
/// fail to start again, with nobody able to reach the app to undo it. Bad numbers are left to
/// `arr set`, which says what is wrong with them.
async fn check_port(st: &AppState, value: &str) -> Result<(), ApiError> {
    let (Ok(port), Some(e)) = (value.parse::<u16>(), st.cfg.embedded.as_ref()) else {
        return Ok(());
    };
    if port == 0 || port == e.listen.port() {
        return Ok(());
    }
    let refuse = |why: &str| {
        Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            format!("port {port} {why}; pick another"),
        ))
    };
    if port == st.cfg.listen.port() {
        return refuse("is the port Arr itself uses");
    }
    let mut addr = e.listen;
    addr.set_port(port);
    let free = tokio::task::spawn_blocking(move || std::net::TcpListener::bind(addr).is_ok())
        .await
        .unwrap_or(false);
    if !free {
        return refuse("is already in use or can't be opened");
    }
    Ok(())
}

/// `POST /settings/app`: changes one setting the way `arr set` does (same checks, and every file
/// is put back if anything fails), then restarts Arr so it takes effect. A value that is
/// already in place changes nothing and restarts nothing.
pub(super) async fn app_settings_post(
    State(st): St,
    ApiJson(body): ApiJson<AppSettingBody>,
) -> ApiResult {
    if !CHANGEABLE.contains(&body.name.as_str()) {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            "that isn't a setting the app can change",
        ));
    }
    let value = body.value.trim();
    // A TMDB read-access token is long: its own limit is the one `arr set` applies (the key
    // check, 1024), and the request body is capped well below anything else.
    if body.name != "tmdb-key" && value.len() > 200 {
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            "that value is too long",
        ));
    }
    // The upload limit is blank for no limit.
    let value = if body.name == "upload-limit-kib" && value.is_empty() {
        "0"
    } else {
        value
    };
    if unchanged(&st, &body.name, value) {
        return ok(
            json!({ "changed": false, "restarting": false, "message": "Nothing to change." }),
        );
    }
    // Every change takes a restart, and Arr started by hand would not come back.
    if !crate::lifecycle::supervised() {
        return Err(ApiError::conflict(
            ErrorCode::InvalidRequest,
            "Arr isn't running as a service here, so it can't restart itself; use `arr set` on the computer",
        ));
    }
    if body.name == "torrent-port" {
        check_port(&st, value).await?;
    }
    let _one_at_a_time = st.settings_lock.lock().await;
    let path = crate::lifecycle::config_path(&st.cfg.state_dir);
    if let Err(e) = crate::set::run(&path, &body.name, Some(value)).await {
        // The words are `arr set`'s, with anything that names a folder or address removed.
        return Err(ApiError::bad_request(
            ErrorCode::InvalidRequest,
            crate::report::scrub(&crate::fsutil::one_line(&format!("{e:#}")))
                // The scrubber takes out web addresses; this one is not private.
                .replace("from <url>", "from TMDB's API settings page"),
        ));
    }
    tracing::info!(setting = %body.name, "a setting was changed from the web app; restarting");
    // Let this answer reach the phone first.
    let state = st.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        state.restart.request();
    });
    ok(json!({
        "changed": true,
        "restarting": true,
        "message": say(&body.name, value),
    }))
}

// ---------------------------------------------------------------- health

fn item(id: &str, label: &str, level: &str, text: impl Into<String>) -> Value {
    json!({ "id": id, "label": label, "level": level, "text": text.into() })
}

fn gb(bytes: u64) -> u64 {
    bytes / (1024 * 1024 * 1024)
}

async fn plex_item(st: &AppState) -> Value {
    let Some(plex) = &st.plex else {
        return item("plex", "Plex", "warn", "Arr isn't connected to Plex.");
    };
    match tokio::time::timeout(PROBE, plex.sections()).await {
        Ok(Ok(_)) => item("plex", "Plex", "ok", "Plex is answering."),
        Ok(Err(crate::upstream::UpstreamError::Auth { .. })) => item(
            "plex",
            "Plex",
            "bad",
            "Plex is there, but it no longer accepts Arr's sign-in. Sign in to Plex again on the computer.",
        ),
        Ok(Err(crate::upstream::UpstreamError::Timeout)) | Err(_) => {
            item("plex", "Plex", "bad", "Plex didn't answer in time.")
        }
        Ok(Err(_)) => item("plex", "Plex", "bad", "Arr can't reach Plex."),
    }
}

fn folders_item(st: &AppState) -> Value {
    let Some(l) = &st.cfg.library else {
        return item(
            "folders",
            "Library folders",
            "warn",
            "No library folders are set up.",
        );
    };
    let mut missing = vec![];
    let mut any = false;
    // (the sport folder is made at the first import, so its absence is not a problem)
    for (name, p) in [("Movies", &l.movies), ("TV", &l.tv)] {
        if let Some(p) = p {
            any = true;
            if !p.is_dir() {
                missing.push(name);
            }
        }
    }
    match (any, missing.as_slice()) {
        (false, _) => item(
            "folders",
            "Library folders",
            "warn",
            "No library folders are set up.",
        ),
        (true, []) => item(
            "folders",
            "Library folders",
            "ok",
            "The movie and TV folders are there.",
        ),
        (true, m) => item(
            "folders",
            "Library folders",
            "bad",
            format!(
                "The {} folder can't be found. Is the drive connected?",
                m.join(" and ")
            ),
        ),
    }
}

async fn downloads_item(st: &AppState) -> Value {
    let label = "Downloads";
    if !st.cfg.execution_enabled {
        return item(
            "downloads",
            label,
            "warn",
            "Downloading is off, so Arr won't fetch anything new.",
        );
    }
    if let Some(e) = st.client.as_deref().and_then(Client::embedded) {
        return if e.status()["status"] == "running" {
            item("downloads", label, "ok", "The download engine is running.")
        } else {
            item(
                "downloads",
                label,
                "bad",
                "The download engine isn't running.",
            )
        };
    }
    if let Some(q) = st.client.as_deref().and_then(Client::qbit) {
        return match tokio::time::timeout(PROBE, q.version()).await {
            Ok(Ok(_)) => item("downloads", label, "ok", "qBittorrent is answering."),
            _ => item("downloads", label, "bad", "Arr can't reach qBittorrent."),
        };
    }
    item("downloads", label, "bad", "No download program is set up.")
}

fn search_item(st: &AppState) -> Value {
    let label = "Search sites";
    let s = search_readiness(st);
    match s["status"].as_str().unwrap_or("") {
        "ok" => item("search", label, "ok", "The search sites are working."),
        "configured" => item("search", label, "ok", "Searching through Prowlarr."),
        "degraded" => item(
            "search",
            label,
            "warn",
            "Some search sites aren't answering.",
        ),
        "blocked" => item(
            "search",
            label,
            "bad",
            "None of the search sites can be used right now.",
        ),
        "no_settings" => item("search", label, "bad", "Arr's setup isn't finished."),
        _ => item(
            "search",
            label,
            "bad",
            "No search sites are set up, so Arr can't find downloads.",
        ),
    }
}

async fn disk_item(st: &AppState) -> Option<Value> {
    let l = st.cfg.library.as_ref()?;
    // (the sport root is not made until the first import, so a missing one is left out)
    let roots: Vec<PathBuf> = l.roots().into_iter().filter(|p| p.exists()).collect();
    let free = tokio::task::spawn_blocking(move || {
        roots
            .iter()
            .filter_map(|p| crate::paths::free_bytes(p))
            .min()
    })
    .await
    .ok()
    .flatten()?;
    let (level, text) = if free < VERY_LOW_BYTES {
        (
            "bad",
            format!("Only {} GB is free, so downloads may stop.", gb(free)),
        )
    } else if free < LOW_BYTES {
        (
            "warn",
            format!("Space is getting low: {} GB free.", gb(free)),
        )
    } else {
        ("ok", format!("{} GB free.", gb(free)))
    };
    Some(item("disk", "Disk space", level, text))
}

fn remote_item(st: &AppState) -> Value {
    if st.cfg.mcp.as_ref().is_some_and(|m| m.oauth.is_some()) {
        item(
            "remote",
            "Remote access",
            "ok",
            "Set up, so the app works away from home.",
        )
    } else {
        item(
            "remote",
            "Remote access",
            "ok",
            "Not set up. The app works at home only.",
        )
    }
}

async fn update_item(st: &AppState) -> Value {
    let label = "Updates";
    if !st.updates.auto_updates() {
        return item(
            "updates",
            label,
            "warn",
            "Automatic updates are off. Arr only updates when you run the installer again.",
        );
    }
    let status = st.updates.status(unix_now());
    if status.state == "failed" {
        let on = status.from.unwrap_or_else(|| VERSION.to_string());
        return item(
            "updates",
            label,
            "warn",
            format!("The last update didn't work, so Arr stayed on {on}."),
        );
    }
    match st.updates.latest().await {
        Some(l) if newer(&l.version, VERSION) => item(
            "updates",
            label,
            "warn",
            format!("Arr {} is available.", l.version),
        ),
        Some(_) => item(
            "updates",
            label,
            "ok",
            format!("Arr {VERSION} is up to date."),
        ),
        None => item(
            "updates",
            label,
            "ok",
            format!("Arr {VERSION}. Couldn't check for a newer version just now."),
        ),
    }
}

/// `GET /system/health`: the summary in plain words, and the last warnings and errors Arr
/// logged (kept in memory, scrubbed of names, paths, addresses and tokens).
pub(super) async fn system_health(State(st): St) -> ApiResult {
    let (plex, downloads, disk, updates) = tokio::join!(
        plex_item(&st),
        downloads_item(&st),
        disk_item(&st),
        update_item(&st)
    );
    let mut items = vec![plex, folders_item(&st), downloads, search_item(&st)];
    items.extend(disk);
    items.push(remote_item(&st));
    items.push(updates);
    let problems: Vec<Value> = crate::logbuf::recent()
        .into_iter()
        .map(|p| {
            json!({
                "at": OffsetDateTime::from_unix_timestamp(p.at)
                    .map(crate::import::rfc3339)
                    .unwrap_or_default(),
                "level": p.level,
                "component": p.component,
                "message": p.message,
            })
        })
        .collect();
    ok(json!({ "items": items, "problems": problems }))
}

use std::path::PathBuf;
