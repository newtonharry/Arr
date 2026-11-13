//! Signing in to Plex through plex.tv, shared by the setup page and `arr plex-signin`.
//!
//! `arr plex-signin` repairs an install whose Plex token was revoked (a password change or
//! "sign out of all devices"): it signs in again with a plex.tv PIN, takes the token of the
//! same Plex server the config points at, checks it works, and only then replaces the token
//! file. The config is never touched. With `--check` it only says whether Plex still accepts
//! the saved token, for the installers.

use crate::config::{self, PlexConfig, Secret};
use crate::fsutil;
use crate::plex::{Plex, Section};
use crate::upstream::{self, UpstreamError};
use anyhow::{Context, Result};
use reqwest::Url;
use serde::Deserialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const PLEX_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_PLEX_JSON: usize = 2 * 1024 * 1024;
/// How long `arr plex-signin` waits for you to sign in.
const SIGNIN_LIMIT: Duration = Duration::from_secs(10 * 60);
const POLL: Duration = Duration::from_secs(2);

/// Arr's client id, so Plex lists Arr once under Authorized Devices however often it signs in.
pub fn client_id(state_dir: &Path) -> Result<String> {
    let secrets = state_dir.join("secrets");
    fsutil::ensure_private_dir(&secrets)?;
    let file = secrets.join("plex_client_id");
    if !file.exists() {
        fsutil::write_new_private_file(&file, fsutil::random_hex(16)?.as_bytes())?;
    }
    fsutil::read_secret_file(&file)
}

/// plex.tv, as Arr talks to it.
pub struct PlexTv {
    pub base: Url,
    /// The sign-in page (app.plex.tv/auth).
    pub auth: Url,
    pub client_id: String,
    pub http: reqwest::Client,
}

/// A sign-in in progress.
pub struct Pin {
    pub id: u64,
    pub code: String,
    /// Where the user signs in.
    pub url: String,
}

pub enum PinState {
    Waiting,
    SignedIn(Secret),
    /// plex.tv forgot the PIN (it lasts about 30 minutes).
    Expired,
}

#[derive(Deserialize)]
struct PinJson {
    id: u64,
    code: String,
    #[serde(rename = "authToken", default)]
    auth_token: Option<String>,
}

/// A Plex server the signed-in account owns.
pub struct Server {
    pub id: String,
    pub name: String,
    pub token: Secret,
    pub ports: Vec<u16>,
}

#[derive(Deserialize)]
struct Resource {
    #[serde(default)]
    name: String,
    #[serde(default)]
    provides: String,
    #[serde(default)]
    owned: bool,
    #[serde(rename = "clientIdentifier", default)]
    client_identifier: String,
    #[serde(rename = "accessToken", default)]
    access_token: Option<String>,
    #[serde(default)]
    connections: Vec<Connection>,
}

#[derive(Deserialize)]
struct Connection {
    #[serde(default)]
    port: u16,
}

fn bad_address() -> UpstreamError {
    UpstreamError::BadResponse {
        reason: "bad plex.tv address".into(),
    }
}

pub fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl PlexTv {
    /// The headers plex.tv (and a Plex being claimed) wants from every app.
    pub fn headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.header("Accept", "application/json")
            .header("X-Plex-Product", "Arr")
            .header("X-Plex-Version", env!("CARGO_PKG_VERSION"))
            .header("X-Plex-Client-Identifier", &self.client_id)
    }

    pub fn url(&self, path: &str) -> Result<Url, UpstreamError> {
        self.base.join(path).map_err(|_| bad_address())
    }

    /// Starts a sign-in; the user finishes it at `Pin::url`.
    pub async fn new_pin(&self) -> Result<Pin, UpstreamError> {
        let url = self.url("api/v2/pins?strong=true")?;
        let r = upstream::send(self.headers(self.http.post(url))).await?;
        let pin: PinJson = upstream::read_json(r, MAX_PLEX_JSON).await?;
        let mut auth = self.auth.clone();
        auth.set_fragment(Some(&format!(
            "?clientID={}&code={}&context%5Bdevice%5D%5Bproduct%5D=Arr",
            urlencode(&self.client_id),
            urlencode(&pin.code)
        )));
        Ok(Pin {
            id: pin.id,
            code: pin.code,
            url: auth.to_string(),
        })
    }

    pub async fn check_pin(&self, id: u64, code: &str) -> Result<PinState, UpstreamError> {
        let url = self.url(&format!("api/v2/pins/{id}?code={}", urlencode(code)))?;
        let r = upstream::send(self.headers(self.http.get(url))).await?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(PinState::Expired);
        }
        let pin: PinJson = upstream::read_json(r, MAX_PLEX_JSON).await?;
        Ok(match pin.auth_token.filter(|t| !t.is_empty()) {
            Some(t) => PinState::SignedIn(Secret::new(t)),
            None => PinState::Waiting,
        })
    }

    /// The Plex servers this account owns, with the ports they listen on.
    pub async fn servers(&self, account: &Secret) -> Result<Vec<Server>, UpstreamError> {
        let url = self.url("api/v2/resources?includeHttps=1&includeRelay=0")?;
        let r = upstream::send(
            self.headers(self.http.get(url))
                .header("X-Plex-Token", account.expose()),
        )
        .await?;
        let list: Vec<Resource> = upstream::read_json(r, MAX_PLEX_JSON).await?;
        Ok(list
            .into_iter()
            .filter(|r| r.owned && r.provides.split(',').any(|p| p.trim() == "server"))
            .filter_map(|r| {
                let token = r.access_token.filter(|t| !t.is_empty())?;
                Some(Server {
                    id: r.client_identifier,
                    name: if r.name.is_empty() {
                        "Plex".into()
                    } else {
                        r.name
                    },
                    token: Secret::new(token),
                    ports: r
                        .connections
                        .iter()
                        .map(|c| c.port)
                        .filter(|p| *p > 0)
                        .collect(),
                })
            })
            .take(20)
            .collect())
    }
}

#[derive(Deserialize)]
struct IdentityJson {
    #[serde(rename = "MediaContainer")]
    container: Identity,
}

/// Who a Plex server says it is (`/identity`, which needs no token).
#[derive(Deserialize)]
pub struct Identity {
    #[serde(rename = "machineIdentifier", default)]
    pub machine_identifier: String,
    /// False on a new Plex that no account owns yet.
    #[serde(default)]
    pub claimed: Option<bool>,
}

pub async fn identity(http: &reqwest::Client, base: &Url) -> Result<Identity, UpstreamError> {
    let url = base.join("identity").map_err(|_| bad_address())?;
    let r = upstream::send(http.get(url).header("Accept", "application/json")).await?;
    Ok(upstream::read_json::<IdentityJson>(r, 64 * 1024)
        .await?
        .container)
}

/// A Plex server's libraries, read with `token`.
pub async fn sections(base_url: Url, token: Secret) -> Result<Vec<Section>, UpstreamError> {
    Plex::new(PlexConfig {
        discover_url: base_url.clone(),
        metadata_url: base_url.clone(),
        image_base_url: base_url.clone(),
        base_url,
        token,
        movie_section: None,
        tv_section: None,
        sport_section: None,
        movies_path: None,
        tv_path: None,
        sport_path: None,
        timeout: PLEX_TIMEOUT,
    })
    .sections()
    .await
}

// ------------------------------------------------------------------ arr plex-signin

pub struct Options {
    pub config: PathBuf,
    pub check: bool,
    pub plex_tv: Url,
    pub plex_auth: Url,
}

/// `arr plex-signin`: prints one `Error:` line on failure and returns the exit code.
pub async fn main(opts: Options) -> i32 {
    let run = async {
        let (base_url, token_file) = plex_settings(&opts.config)?;
        if opts.check {
            check(base_url, &token_file).await
        } else {
            signin(&opts, base_url, &token_file)
                .await
                .map_err(|e| (1, e))
        }
    };
    match run.await {
        Ok(()) => 0,
        Err((code, e)) => {
            eprintln!("Error: {}", crate::fsutil::one_line(&format!("{e:#}")));
            code
        }
    }
}

type Failed = (i32, anyhow::Error);

/// `[plex] base_url`, and the token file (relative to the config's folder).
fn plex_settings(config: &Path) -> Result<(Url, PathBuf), Failed> {
    #[derive(Deserialize)]
    struct File {
        plex: Option<PlexPart>,
    }
    #[derive(Deserialize)]
    struct PlexPart {
        base_url: String,
        token_file: String,
    }
    let read = || -> Result<(Url, PathBuf)> {
        let text = std::fs::read_to_string(config)
            .with_context(|| format!("can't read {}", config.display()))?;
        let file: File = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{} isn't valid: {}", config.display(), e.message()))?;
        let plex = file
            .plex
            .with_context(|| format!("{} has no [plex] section", config.display()))?;
        let dir = std::path::absolute(config)?
            .parent()
            .context("the config has no folder")?
            .to_path_buf();
        Ok((
            config::parse_base_url(&plex.base_url, "plex.base_url")?,
            dir.join(plex.token_file),
        ))
    };
    read().map_err(|e| (1, e))
}

/// Whether Plex still accepts the saved token: 0 yes, 1 refused, 2 unreachable.
async fn check(base_url: Url, token_file: &Path) -> Result<(), Failed> {
    let token = fsutil::read_secret_file(token_file)
        .map_err(|e| (1, e.context("Arr's Plex sign-in is missing")))?;
    match sections(base_url, Secret::new(token)).await {
        Ok(_) => {
            println!("Plex accepts Arr's sign-in.");
            Ok(())
        }
        Err(UpstreamError::Auth { .. }) => Err((
            1,
            anyhow::anyhow!(
                "Plex no longer accepts Arr's sign-in (it was signed out, or the Plex password changed); run `arr plex-signin` to sign in again"
            ),
        )),
        Err(e) => Err((2, anyhow::anyhow!("couldn't reach Plex: {}", e.describe()))),
    }
}

async fn signin(opts: &Options, base_url: Url, token_file: &Path) -> Result<()> {
    // The config's folder is Arr's state folder.
    let config = std::path::absolute(&opts.config)?;
    let tv = PlexTv {
        base: opts.plex_tv.clone(),
        auth: opts.plex_auth.clone(),
        client_id: client_id(config.parent().context("the config has no folder")?)?,
        http: upstream::http_client(PLEX_TIMEOUT, reqwest::redirect::Policy::none()),
    };
    let reach = |e: UpstreamError| anyhow::anyhow!("couldn't reach plex.tv: {}", e.describe());
    let pin = tv.new_pin().await.map_err(reach)?;
    println!("Open this link to sign in to Plex: {}", pin.url);
    let _ = std::io::stdout().flush();

    let started = Instant::now();
    let account = loop {
        if started.elapsed() > SIGNIN_LIMIT {
            anyhow::bail!("the Plex sign-in wasn't finished in 10 minutes; nothing was changed");
        }
        tokio::time::sleep(POLL).await;
        // A failed look is retried: plex.tv may have a bad moment while you sign in.
        match tv.check_pin(pin.id, &pin.code).await {
            Ok(PinState::SignedIn(t)) => break t,
            Ok(PinState::Expired) => {
                anyhow::bail!("the Plex sign-in expired; nothing was changed")
            }
            Ok(PinState::Waiting) | Err(_) => {}
        }
    };

    let plex_down = |e: UpstreamError| anyhow::anyhow!("couldn't reach Plex: {}", e.describe());
    let id = identity(&tv.http, &base_url).await.map_err(plex_down)?;
    let servers = tv.servers(&account).await.map_err(reach)?;
    let server = servers
        .iter()
        .find(|s| !id.machine_identifier.is_empty() && s.id == id.machine_identifier)
        .context(
            "the Plex Arr uses isn't one of this Plex account's servers; sign in with the account that owns it. Nothing was changed",
        )?;
    sections(base_url, server.token.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Plex didn't accept the new sign-in: {}", e.describe()))?;
    fsutil::atomic_replace_private(token_file, server.token.expose().as_bytes())?;
    println!("Signed in to Plex again. Restart Arr to use it.");
    Ok(())
}
