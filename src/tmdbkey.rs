//! `arr tmdb-key`: adds or replaces the TMDB key of an existing install, so Arr can find
//! movies you don't own yet. The installers run it when a config already exists.

use crate::{config, fsutil, metadata::Tmdb, upstream::UpstreamError};
use anyhow::{Context, Result, bail};
use std::path::Path;

const CREDENTIAL_FILE: &str = "secrets/tmdb_credential";
/// Asked for to check a key: The Matrix, which every key can read.
const CHECK_MOVIE: u32 = 603;

/// Saves `key` beside `config`, turns on its `[tmdb]` section and checks the key with TMDB.
/// Anything that fails puts the config and the old key back.
pub async fn set(config_path: &Path, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty()
        || key.len() > 1024
        || key.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!(
            "that TMDB key doesn't look right: paste the API key or the API read access token from https://www.themoviedb.org/settings/api"
        );
    }
    let old_config = std::fs::read_to_string(config_path)
        .with_context(|| format!("can't read {}", config_path.display()))?;
    let dir = config_path.parent().context("the config has no folder")?;
    let secret = dir.join(CREDENTIAL_FILE);
    let old_secret = std::fs::read(&secret).ok();
    fsutil::ensure_private_dir(&dir.join("secrets"))?;
    fsutil::atomic_replace_private(&secret, key.as_bytes())?;
    fsutil::atomic_replace_private(config_path, enable_tmdb(&old_config).as_bytes())?;

    let checked = check(config_path).await;
    if checked.is_err() {
        let _ = fsutil::atomic_replace_private(config_path, old_config.as_bytes());
        let _ = match &old_secret {
            Some(s) => fsutil::atomic_replace_private(&secret, s),
            None => std::fs::remove_file(&secret).map_err(Into::into),
        };
    }
    checked
}

async fn check(config_path: &Path) -> Result<()> {
    let cfg = config::load(config_path).context("the config didn't load with TMDB turned on")?;
    let tmdb = cfg
        .tmdb
        .as_ref()
        .and_then(Tmdb::new)
        .context("the config didn't turn TMDB on")?;
    match tmdb.movie(CHECK_MOVIE).await {
        Ok(_) => Ok(()),
        Err(UpstreamError::Auth { .. }) => bail!(
            "TMDB didn't accept that key: copy the API key or the API read access token from https://www.themoviedb.org/settings/api"
        ),
        Err(e) => bail!(
            "couldn't check the key with TMDB ({}); try again later",
            e.describe()
        ),
    }
}

/// The config with `[tmdb] enabled = true` and the credential file set, everything else as
/// it was (comments included). A config without a `[tmdb]` section gets one at the end.
fn enable_tmdb(text: &str) -> String {
    let cred = format!("credential_file = \"{CREDENTIAL_FILE}\"");
    let mut out: Vec<String> = Vec::new();
    let mut header = None;
    let mut in_tmdb = false;
    let (mut enabled, mut credential) = (false, false);
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_tmdb = t.split('#').next().unwrap_or("").trim() == "[tmdb]";
            if in_tmdb && header.is_none() {
                header = Some(out.len());
            }
        } else if in_tmdb {
            match t.split('=').next().unwrap_or("").trim() {
                "enabled" => {
                    out.push("enabled = true".into());
                    enabled = true;
                    continue;
                }
                "credential_file" => {
                    out.push(cred.clone());
                    credential = true;
                    continue;
                }
                _ => {}
            }
        }
        out.push(line.to_string());
    }
    let missing: Vec<String> = [
        (!enabled).then(|| "enabled = true".to_string()),
        (!credential).then_some(cred),
    ]
    .into_iter()
    .flatten()
    .collect();
    match header {
        Some(i) => {
            out.splice(i + 1..i + 1, missing);
        }
        None => {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push("[tmdb]".into());
            out.extend(missing);
        }
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::enable_tmdb;

    #[test]
    fn turns_on_the_section_the_installer_wrote() {
        let before = "# Arr\n[tvmaze]\nenabled = true\n\n[tmdb]\nenabled = false\n\n[execution]\nenabled = true\n";
        assert_eq!(
            enable_tmdb(before),
            "# Arr\n[tvmaze]\nenabled = true\n\n[tmdb]\ncredential_file = \"secrets/tmdb_credential\"\nenabled = true\n\n[execution]\nenabled = true\n"
        );
    }

    #[test]
    fn replaces_an_old_credential_and_keeps_other_keys() {
        let before =
            "[tmdb] # movies\nenabled = true\ncredential_file = \"old\"\ntimeout_secs = 30\n";
        assert_eq!(
            enable_tmdb(before),
            "[tmdb] # movies\nenabled = true\ncredential_file = \"secrets/tmdb_credential\"\ntimeout_secs = 30\n"
        );
    }

    #[test]
    fn adds_a_section_when_there_is_none() {
        assert_eq!(
            enable_tmdb("[server]\nlisten = \"127.0.0.1:1\""),
            "[server]\nlisten = \"127.0.0.1:1\"\n\n[tmdb]\nenabled = true\ncredential_file = \"secrets/tmdb_credential\"\n"
        );
    }
}
