//! `arr set`: changes one setting of an existing install. The config is edited line by line,
//! so comments and layout stay. The changed config must then load, and a changed key or
//! address must work, or every file is put back exactly as it was.

use crate::{config, fsutil, metadata::Tmdb, plex::Plex, tmdbkey, upstream::UpstreamError};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub const NAMES: &str = "tmdb-key, plex-token, plex-url, downloading, upload-limit-kib, torrent-port, error-reports, default-indexers, api-token, mcp-token";

/// Remembers that Knaben was offered, so removing it later sticks (a file beside config.toml).
pub const INDEXER_MARKER: &str = "default-indexers";

/// What a secret setting is called when asking for it, or `None` when the value (if any)
/// comes from the command line. Unknown names are an error.
pub fn secret(name: &str) -> Result<Option<&'static str>> {
    Ok(match name {
        "tmdb-key" => Some("TMDB key"),
        "plex-token" => Some("Plex token"),
        "plex-url" | "downloading" | "upload-limit-kib" | "torrent-port" | "error-reports"
        | "default-indexers" | "api-token" | "mcp-token" => None,
        _ => bail!("there's no setting called {name:?}; settings: {NAMES}"),
    })
}

/// Keys of a section and their new TOML values (`None` removes the key).
type Keys = Vec<(&'static str, Option<String>)>;

/// One change: keys of a config section to set (to a TOML value) or remove, a file to
/// write, how to check the result, and what to tell the user.
struct Plan {
    edit: Option<(&'static str, Keys)>,
    file: Option<(PathBuf, Vec<u8>)>,
    check: Check,
    done: String,
}

enum Check {
    /// The config loads.
    Load,
    Tmdb,
    /// Plex lists its libraries with the new token.
    PlexToken,
    /// Plex answers at the new address (it may refuse the saved token).
    PlexUrl,
}

/// Changes `name` to `value` in the install configured by `config_path` and returns the line
/// to show. Anything that fails leaves every file as it was.
pub async fn run(config_path: &Path, name: &str, value: Option<&str>) -> Result<String> {
    // Absolute, so a bare `config.toml` has a folder to write beside.
    let config_path = &std::path::absolute(config_path)?;
    let old = std::fs::read_to_string(config_path)
        .with_context(|| format!("can't read {}", config_path.display()))?;
    let dir = config_path.parent().context("the config has no folder")?;
    let table: toml::Table = toml::from_str(&old)
        .with_context(|| format!("{} isn't a valid config", config_path.display()))?;
    if name == "default-indexers" {
        if value.is_some_and(|v| !v.trim().is_empty()) {
            bail!("`arr set default-indexers` takes no value");
        }
        return default_indexers(config_path, &old, &table, dir).await;
    }
    let plan = plan(&table, dir, name, value)?;
    // The line editor can't handle every way TOML can say a key (dotted or quoted keys,
    // values over several lines), so the result must say exactly what was meant.
    let edited = match &plan.edit {
        Some((section, keys)) => {
            let text = edit(&old, section, keys);
            if let Some(key) = mismatch(&text, section, keys) {
                bail!(
                    "couldn't change {name} automatically; edit {key} under [{section}] in {} by hand",
                    config_path.display()
                );
            }
            Some(text)
        }
        None => None,
    };
    let old_file = match &plan.file {
        Some((path, _)) => {
            // Under sudo a folder made here would belong to root.
            let parent = path.parent().context("the file has no folder")?;
            if !parent.is_dir() {
                bail!("the secrets folder {} is missing", parent.display());
            }
            match std::fs::read(path) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e).with_context(|| format!("can't read {}", path.display())),
            }
        }
        None => None,
    };

    let applied = async {
        if let Some((path, bytes)) = &plan.file {
            fsutil::atomic_replace_private(path, bytes)?;
        }
        if let Some(text) = &edited {
            fsutil::atomic_replace_private(config_path, text.as_bytes())?;
        }
        let cfg = config::load(config_path).context("Arr wouldn't start with that change")?;
        check(&plan.check, &cfg, config_path).await
    }
    .await;
    match applied {
        Ok(None) => Ok(plan.done),
        Ok(Some(note)) => Ok(format!("{} {note}", plan.done)),
        Err(e) => {
            let mut e = format!("{e:#}");
            if edited.is_some()
                && let Err(r) = fsutil::atomic_replace_private(config_path, old.as_bytes())
            {
                e += &format!(", and couldn't put back {}: {r:#}", config_path.display());
            }
            if let Some((path, _)) = &plan.file {
                let restored = match &old_file {
                    Some(b) => fsutil::atomic_replace_private(path, b),
                    None => std::fs::remove_file(path).map_err(Into::into),
                };
                if let Err(r) = restored {
                    e += &format!(", and couldn't put back {}: {r:#}", path.display());
                }
            }
            Err(anyhow::anyhow!(e))
        }
    }
}

/// The Knaben entry added to an install that searches The Pirate Bay (its id is chosen).
fn knaben_entry(id: u32) -> String {
    format!(
        "[[indexer]]\nid = {id}\nname = \"Knaben\"\nkind = \"knaben\"\nbase_url = \"https://api.knaben.org/\"\n"
    )
}

/// Adds Knaben to an install that still has only the default public source. Nothing existing
/// is touched: the entry is appended with the lowest free id from 2. It is added once: when
/// Knaben is configured, was offered before (the marker file) or the install doesn't use The
/// Pirate Bay (Prowlarr, or its own indexers), the config is left as it is.
async fn default_indexers(
    config_path: &Path,
    old: &str,
    table: &toml::Table,
    dir: &Path,
) -> Result<String> {
    let marker = dir.join(INDEXER_MARKER);
    let remember = || fsutil::atomic_replace_private(&marker, b"knaben\n");
    let entries: &[toml::Value] = table
        .get("indexer")
        .and_then(toml::Value::as_array)
        .map_or(&[], Vec::as_slice);
    let kinds: Vec<&str> = entries
        .iter()
        .filter_map(|e| e.get("kind").and_then(toml::Value::as_str))
        .collect();
    if table.contains_key("prowlarr") {
        return Ok("This install searches through Prowlarr; no search source was added.".into());
    }
    if kinds.contains(&"knaben") {
        if !marker.exists() {
            remember()?;
        }
        return Ok("Knaben is already a search source here.".into());
    }
    if marker.exists() {
        return Ok("Knaben was offered before and isn't configured; left as it is. Add it to config.toml to use it (config/standalone.example.toml).".into());
    }
    if !kinds.contains(&"thepiratebay") {
        return Ok(
            "No search source was added: this install doesn't use the default public source."
                .into(),
        );
    }
    let used: Vec<i64> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(toml::Value::as_integer))
        .collect();
    let id = (2..).find(|i| !used.contains(&i64::from(*i))).unwrap_or(2);
    let nl = if old.contains("\r\n") { "\r\n" } else { "\n" };
    let mut text = old.trim_end().to_string();
    text.push_str(nl);
    text.push_str(nl);
    text.push_str(&knaben_entry(id).replace('\n', nl));
    // Added to a config that loads, and the result must load too, or the old text goes back.
    fsutil::atomic_replace_private(config_path, text.as_bytes())?;
    if let Err(e) = config::load(config_path).context("Arr wouldn't start with that change") {
        let mut e = format!("{e:#}");
        if let Err(r) = fsutil::atomic_replace_private(config_path, old.as_bytes()) {
            e += &format!(", and couldn't put back {}: {r:#}", config_path.display());
        }
        bail!(e);
    }
    remember()?;
    Ok(format!(
        "Added Knaben as search source {id}, next to The Pirate Bay. Run `arr setup` and restart Arr to use it."
    ))
}

/// The first of `keys` that `text` doesn't set to the intended value under `[section]` (or
/// still has, for a key to remove). Text that isn't TOML any more fails on the first key.
fn mismatch<'k>(text: &str, section: &str, keys: &[(&'k str, Option<String>)]) -> Option<&'k str> {
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return keys.first().map(|(k, _)| *k);
    };
    let got = table.get(section).and_then(toml::Value::as_table);
    keys.iter()
        .find(|(k, want)| {
            let want = want.as_ref().map(|v| {
                toml::from_str::<toml::Table>(&format!("v = {v}"))
                    .ok()
                    .and_then(|mut t| t.remove("v"))
            });
            let have = got.and_then(|t| t.get(*k));
            match want {
                None => have.is_some(),
                Some(want) => want.is_none() || have != want.as_ref(),
            }
        })
        .map(|(k, _)| *k)
}

fn plan(table: &toml::Table, dir: &Path, name: &str, value: Option<&str>) -> Result<Plan> {
    let secret = secret(name)?;
    let value = match (secret, value) {
        (_, Some(v)) => v.trim(),
        (Some(what), None) => bail!("no {what} was given"),
        (None, None) if matches!(name, "api-token" | "mcp-token") => "",
        (None, None) => bail!("`arr set {name}` needs a value ({})", usage(name)),
    };
    if !value.is_empty() && matches!(name, "api-token" | "mcp-token") {
        bail!("`arr set {name}` makes a new random token; it takes no value");
    }
    let section = |s: &str| table.get(s).and_then(toml::Value::as_table);
    let file = |s: &str, key: &str| {
        section(s)
            .and_then(|t| t.get(key))
            .and_then(toml::Value::as_str)
            .map(|f| dir.join(f))
    };
    let restart = "Restart Arr to use it.";
    let simple = |section: &'static str, key: &'static str, v: String, done: String| Plan {
        edit: Some((section, vec![(key, Some(v))])),
        file: None,
        check: Check::Load,
        done,
    };
    let embedded = || {
        section("embedded").context("this install doesn't use Arr's own downloader ([embedded])")
    };
    Ok(match name {
        "tmdb-key" => Plan {
            edit: Some((
                "tmdb",
                vec![
                    ("enabled", Some("true".into())),
                    (
                        "credential_file",
                        Some(config::quote(tmdbkey::CREDENTIAL_FILE)),
                    ),
                ],
            )),
            file: Some((
                dir.join(tmdbkey::CREDENTIAL_FILE),
                tmdbkey::shape(value)?.as_bytes().to_vec(),
            )),
            check: Check::Tmdb,
            done: format!("Saved the TMDB key. {restart}"),
        },
        "plex-token" => {
            if value.len() > 1024 || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
                bail!("that doesn't look like a Plex token");
            }
            Plan {
                edit: None,
                file: Some((
                    file("plex", "token_file")
                        .context("this install isn't connected to Plex ([plex] token_file)")?,
                    value.as_bytes().to_vec(),
                )),
                check: Check::PlexToken,
                done: format!("Saved the Plex token. {restart}"),
            }
        }
        "plex-url" => {
            section("plex").context("this install isn't connected to Plex (no [plex] section)")?;
            let url = config::parse_base_url(value, "plex-url")?;
            Plan {
                check: Check::PlexUrl,
                ..simple(
                    "plex",
                    "base_url",
                    config::quote(url.as_str()),
                    format!("Changed the Plex address to {url}. {restart}"),
                )
            }
        }
        "downloading" => {
            let on = switch(value)?;
            // `download_submission` is the old name of `enabled`; both would be a duplicate.
            Plan {
                edit: Some((
                    "execution",
                    vec![
                        ("enabled", Some(on.to_string())),
                        ("download_submission", None),
                    ],
                )),
                file: None,
                check: Check::Load,
                done: format!("Turned downloading {value}. {restart}"),
            }
        }
        "error-reports" => {
            let on = switch(value)?;
            simple(
                "reporting",
                "enabled",
                on.to_string(),
                format!("Turned error reports {value}. {restart}"),
            )
        }
        "upload-limit-kib" => {
            embedded()?;
            let kib: u32 = value
                .parse()
                .ok()
                .filter(|k| *k <= u32::MAX / 1024)
                .with_context(|| {
                    format!(
                        "the upload limit is a whole number of KiB/s from 0 (no limit) to {}",
                        u32::MAX / 1024
                    )
                })?;
            let (v, done) = match kib {
                0 => (None, "Removed the upload limit.".to_string()),
                k => (
                    Some((k * 1024).to_string()),
                    format!("Changed the upload limit to {k} KiB/s."),
                ),
            };
            Plan {
                edit: Some(("embedded", vec![("upload_limit_bytes_per_sec", v)])),
                file: None,
                check: Check::Load,
                done: format!("{done} {restart}"),
            }
        }
        "torrent-port" => {
            let port: u16 = value
                .parse()
                .ok()
                .filter(|p| *p != 0)
                .context("the torrent port is a number from 1 to 65535")?;
            let listen = embedded()?
                .get("listen")
                .and_then(toml::Value::as_str)
                .context("[embedded] has no listen address")?;
            let mut addr: std::net::SocketAddr = listen.trim().parse().with_context(|| {
                format!("[embedded] listen isn't an IP:port address ({listen:?})")
            })?;
            addr.set_port(port);
            simple(
                "embedded",
                "listen",
                config::quote(&addr.to_string()),
                format!("Changed the torrent port to {port}. {restart}"),
            )
        }
        "api-token" | "mcp-token" => {
            let (path, what) = if name == "api-token" {
                (file("server", "api_token_file"), "API token")
            } else {
                (file("mcp", "token_file"), "MCP token")
            };
            let path = path.with_context(|| format!("this install has no {what} file"))?;
            Plan {
                edit: None,
                done: format!(
                    "Made a new {what} in {}. {restart} Then update anything that uses it.",
                    path.display()
                ),
                file: Some((path, fsutil::random_hex(32)?.into_bytes())),
                check: Check::Load,
            }
        }
        _ => unreachable!("secret() knows every name"),
    })
}

fn usage(name: &str) -> &'static str {
    match name {
        "plex-url" => "Plex's address, like http://127.0.0.1:32400",
        "downloading" | "error-reports" => "on or off",
        "default-indexers" => "no value",
        "upload-limit-kib" => "KiB/s, 0 for no limit",
        _ => "a port from 1 to 65535",
    }
}

fn switch(value: &str) -> Result<bool> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => bail!("use on or off (got {value:?})"),
    }
}

/// Tries the change for real. `Some` is a note to add to the success line.
async fn check(
    check: &Check,
    cfg: &config::RuntimeConfig,
    config_path: &Path,
) -> Result<Option<String>> {
    let plex = || cfg.plex.clone().context("the config has no [plex] section");
    match check {
        Check::Load => Ok(None),
        Check::Tmdb => {
            let tmdb = cfg
                .tmdb
                .as_ref()
                .and_then(Tmdb::new)
                .context("the config didn't turn TMDB on")?;
            tmdbkey::ask(&tmdb).await.map(|()| None)
        }
        Check::PlexToken | Check::PlexUrl => {
            let plex = plex()?;
            let url = plex.base_url.clone();
            match Plex::new(plex).sections().await {
                Ok(_) => Ok(None),
                Err(UpstreamError::Auth { .. }) if matches!(check, Check::PlexToken) => {
                    bail!("Plex didn't accept that token")
                }
                // Plex is there; the sign-in is a separate fix (the installer applies the
                // address first, then a new token or sign-in). Only a real Plex counts: a
                // wrong port or a protected proxy refuses too.
                Err(UpstreamError::Auth { .. }) => {
                    let http = crate::upstream::http_client(
                        crate::plexsignin::PLEX_TIMEOUT,
                        reqwest::redirect::Policy::none(),
                    );
                    match crate::plexsignin::identity(&http, &url).await {
                        Ok(id) if !id.machine_identifier.is_empty() => Ok(Some(format!(
                            "Plex refused the saved sign-in there; run `arr plex-signin --config {}` or set a new token.",
                            config_path.display()
                        ))),
                        _ => bail!("{url} didn't answer like a Plex server"),
                    }
                }
                Err(e) => bail!("couldn't reach Plex at {url} ({})", e.describe()),
            }
        }
    }
}

/// `text` with `keys` of `[section]` set to the given TOML values (or removed when `None`),
/// everything else as it was, comments included. New keys go just under the section's
/// header; a missing section is added at the end when there's something to set.
pub fn edit(text: &str, section: &str, keys: &[(&str, Option<String>)]) -> String {
    let header = format!("[{section}]");
    let mut out: Vec<String> = Vec::new();
    let mut at = None;
    let mut inside = false;
    let mut seen = vec![false; keys.len()];
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t.split('#').next().unwrap_or("").trim() == header;
            if inside && at.is_none() {
                at = Some(out.len() + 1);
            }
        } else if inside {
            let key = t.split('=').next().unwrap_or("").trim();
            if let Some(i) = keys.iter().position(|(k, _)| *k == key) {
                seen[i] = true;
                if let Some(v) = &keys[i].1 {
                    out.push(format!("{key} = {v}"));
                }
                continue;
            }
        }
        out.push(line.to_string());
    }
    let missing: Vec<String> = keys
        .iter()
        .zip(seen)
        .filter_map(|((k, v), seen)| v.as_ref().filter(|_| !seen).map(|v| format!("{k} = {v}")))
        .collect();
    match at {
        Some(i) => {
            out.splice(i..i, missing);
        }
        None if !missing.is_empty() => {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push(header);
            out.extend(missing);
        }
        None => {}
    }
    // Keep Windows line ends if the file has them.
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut s = out.join(nl);
    s.push_str(nl);
    s
}

#[cfg(test)]
mod tests {
    use super::edit;

    fn tmdb(text: &str) -> String {
        edit(
            text,
            "tmdb",
            &[
                ("enabled", Some("true".into())),
                (
                    "credential_file",
                    Some("\"secrets/tmdb_credential\"".into()),
                ),
            ],
        )
    }

    #[test]
    fn turns_on_the_section_the_installer_wrote() {
        let before = "# Arr\n[tvmaze]\nenabled = true\n\n[tmdb]\nenabled = false\n\n[execution]\nenabled = true\n";
        assert_eq!(
            tmdb(before),
            "# Arr\n[tvmaze]\nenabled = true\n\n[tmdb]\ncredential_file = \"secrets/tmdb_credential\"\nenabled = true\n\n[execution]\nenabled = true\n"
        );
    }

    #[test]
    fn replaces_keys_and_keeps_comments_and_other_keys() {
        let before = "[tmdb] # movies\n# a note\nenabled = true\ncredential_file = \"old\"\ntimeout_secs = 30\n";
        assert_eq!(
            tmdb(before),
            "[tmdb] # movies\n# a note\nenabled = true\ncredential_file = \"secrets/tmdb_credential\"\ntimeout_secs = 30\n"
        );
    }

    #[test]
    fn adds_a_section_when_there_is_none() {
        assert_eq!(
            tmdb("[server]\nlisten = \"127.0.0.1:1\""),
            "[server]\nlisten = \"127.0.0.1:1\"\n\n[tmdb]\nenabled = true\ncredential_file = \"secrets/tmdb_credential\"\n"
        );
    }

    #[test]
    fn removes_a_key_and_leaves_other_sections_alone() {
        let before = "[embedded]\nlisten = \"0.0.0.0:6881\"\nupload_limit_bytes_per_sec = 512000\n\n[other]\nupload_limit_bytes_per_sec = 1\n";
        let keys = [("upload_limit_bytes_per_sec", None)];
        assert_eq!(
            edit(before, "embedded", &keys),
            "[embedded]\nlisten = \"0.0.0.0:6881\"\n\n[other]\nupload_limit_bytes_per_sec = 1\n"
        );
        // Removing from a missing section adds nothing.
        assert_eq!(edit("[server]\n", "embedded", &keys), "[server]\n");
    }

    #[test]
    fn subtables_and_commented_keys_are_not_the_section() {
        let before = "[mcp]\n# enabled = true\n[mcp.oauth]\nenabled = false\n";
        assert_eq!(
            edit(before, "mcp", &[("enabled", Some("true".into()))]),
            "[mcp]\nenabled = true\n# enabled = true\n[mcp.oauth]\nenabled = false\n"
        );
    }

    #[test]
    fn keeps_windows_line_ends() {
        assert_eq!(
            edit(
                "[reporting]\r\nenabled = false\r\n",
                "reporting",
                &[("enabled", Some("true".into()))]
            ),
            "[reporting]\r\nenabled = true\r\n"
        );
    }
}
