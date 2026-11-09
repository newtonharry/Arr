//! Checking TMDB keys, so Arr can find movies you don't own yet. `arr set tmdb-key` (and the
//! older `arr tmdb-key`) saves one; the setup page and installers check one first.

use crate::{
    config::{MetadataConfig, Secret},
    metadata::Tmdb,
    upstream::UpstreamError,
};
use anyhow::{Context, Result, bail};
use reqwest::Url;
use std::time::Duration;

/// Where a saved key lives, beside the config.
pub const CREDENTIAL_FILE: &str = "secrets/tmdb_credential";
/// Asked for to check a key: The Matrix, which every key can read.
const CHECK_MOVIE: u32 = 603;
/// Where TMDB's API lives (overridden in tests).
pub const API_URL: &str = "https://api.themoviedb.org/";
/// What Arr tells an assistant, and whoever runs it, when movie search isn't set up.
pub const MISSING: &str = "Movie search isn't set up: Arr needs a free TMDB key to find movies the user doesn't own yet (TV works without one). Ask the user to get one at https://www.themoviedb.org/settings/api (steps: https://github.com/newtonharry/Arr/blob/main/docs/TMDB.md), then run the Arr install command again with --tmdb-key KEY (on Windows, $env:ARR_TMDB_KEY = 'KEY' before the install line).";

/// The key without spaces around it, or why it can't be one.
pub fn shape(key: &str) -> Result<&str> {
    let key = key.trim();
    if key.is_empty()
        || key.len() > 1024
        || key.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!(
            "that doesn't look like a TMDB key: copy the API Read Access Token or the API Key from https://www.themoviedb.org/settings/api"
        );
    }
    Ok(key)
}

/// Checks `key` with TMDB at `base_url` without saving it (the setup page, `--check`).
pub async fn verify(base_url: &Url, key: &str) -> Result<()> {
    let cfg = MetadataConfig {
        base_url: base_url.clone(),
        image_base_url: base_url.clone(),
        timeout: Duration::from_secs(20),
        credential: Some(Secret::new(shape(key)?.to_string())),
    };
    ask(&Tmdb::new(&cfg).context("no key")?).await
}

/// Asks TMDB for a movie every key can read.
pub async fn ask(tmdb: &Tmdb) -> Result<()> {
    match tmdb.movie(CHECK_MOVIE).await {
        Ok(_) => Ok(()),
        Err(UpstreamError::Auth { .. }) => bail!(
            "TMDB didn't accept that key: copy the whole API Read Access Token or API Key from https://www.themoviedb.org/settings/api (a brand-new key can take a few minutes to work)"
        ),
        Err(e) => bail!(
            "couldn't reach TMDB to check the key ({}); try again in a minute",
            e.describe()
        ),
    }
}
