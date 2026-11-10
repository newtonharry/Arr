//! Phone notifications: standard Web Push (RFC 8030), with VAPID (RFC 8292) and aes128gcm
//! content encryption (RFC 8291/8188), sent from Arr to each browser's push service.
//!
//! Everything is built on `ring`, which Arr already uses, so there is no new crate and no native
//! library beyond it. The VAPID key is `<state>/secrets/vapid_key` (created on first use; an older
//! Arr ignores the file). Subscriptions and the record of what was already announced live in
//! `web.sqlite`, in tables an older Arr never reads. A pass runs every minute, reads the web
//! app's own feed, and sends the items that are new since the last pass; the first pass only
//! records what is there, so turning this on never announces old items.

use crate::fsutil;
use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::SecureRandom;
use ring::{aead, agreement, hkdf, rand, signature};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

/// Most devices one Arr will push to.
const MAX_SUBS: i64 = 100;
const MAX_ENDPOINT: usize = 2048;
/// How long the push service may hold a message for a phone that is off: a day.
const TTL: &str = "86400";
const RECORD_SIZE: u32 = 4096;
/// Sent as the VAPID `sub` claim; services want a contact URL.
const VAPID_SUBJECT: &str = "https://github.com/newtonharry/Arr";
const INIT_MARK: &str = "__init";
const KEEP_SENT: i64 = 30 * 86_400;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

// ---------------------------------------------------------------- the test-only way past the host guard

#[cfg(feature = "test-client")]
static LOOPBACK_OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Lets tests push to a mock on loopback (over plain http). Not in release builds.
#[cfg(feature = "test-client")]
pub fn allow_loopback_for_tests() {
    LOOPBACK_OK.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn loopback_ok() -> bool {
    #[cfg(feature = "test-client")]
    {
        LOOPBACK_OK.load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(not(feature = "test-client"))]
    {
        false
    }
}

#[cfg(feature = "test-client")]
static TICK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Shortens the pass interval for passes that start afterwards. Not in release builds.
#[cfg(feature = "test-client")]
pub fn set_tick_for_tests(ms: u64) {
    TICK_MS.store(ms, std::sync::atomic::Ordering::SeqCst);
}

/// How often a pass runs.
fn tick() -> Duration {
    #[cfg(feature = "test-client")]
    {
        let ms = TICK_MS.load(std::sync::atomic::Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    Duration::from_secs(60)
}

// ---------------------------------------------------------------- endpoint guard

fn public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 100 && o[1] & 0xc0 == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 192 && o[1] == 0 && o[2] == 2)
        || (o[0] == 198 && o[1] & 0xfe == 18)
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113))
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4);
    }
    let s = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || s[0] & 0xfe00 == 0xfc00 // unique local
        || s[0] & 0xffc0 == 0xfe80 // link-local
        || (s[0] == 0x64 && s[1] == 0xff9b) // NAT64
        || (s[0] == 0x2001 && s[1] == 0xdb8) // documentation
        || s[0] == 0)
}

/// Whether an address is a public internet one.
pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => public_v4(v),
        IpAddr::V6(v) => public_v6(v),
    }
}

fn address_ok(ip: IpAddr) -> bool {
    public_ip(ip) || (loopback_ok() && ip.is_loopback())
}

/// A push endpoint Arr may call: https, a sane length, the default port, no credentials, and a
/// host that is not a private, loopback or link-local address (Arr's server makes the request,
/// so this keeps a signed-in person from pointing it at their own network).
pub fn validate_endpoint(endpoint: &str) -> Result<reqwest::Url, &'static str> {
    if endpoint.len() > MAX_ENDPOINT {
        return Err("endpoint too long");
    }
    let url = reqwest::Url::parse(endpoint).map_err(|_| "endpoint is not a URL")?;
    let host = url.host().ok_or("endpoint has no host")?;
    let local_http = loopback_ok()
        && url.scheme() == "http"
        && match &host {
            url::Host::Ipv4(ip) => ip.is_loopback(),
            url::Host::Ipv6(ip) => ip.is_loopback(),
            url::Host::Domain(_) => false,
        };
    if url.scheme() != "https" && !local_http {
        return Err("endpoint must be https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("endpoint must not carry credentials");
    }
    if !local_http && url.port().is_some() {
        return Err("endpoint must use the default port");
    }
    match host {
        url::Host::Ipv4(ip) => {
            if !address_ok(IpAddr::V4(ip)) {
                return Err("endpoint is not a public address");
            }
        }
        url::Host::Ipv6(ip) => {
            if !address_ok(IpAddr::V6(ip)) {
                return Err("endpoint is not a public address");
            }
        }
        url::Host::Domain(d) => {
            let d = d.to_ascii_lowercase();
            let d = d.trim_end_matches('.');
            let local = !d.contains('.')
                || [
                    "local",
                    "localhost",
                    "internal",
                    "lan",
                    "home",
                    "arpa",
                    "corp",
                ]
                .iter()
                .any(|s| d.ends_with(&format!(".{s}")));
            if local {
                return Err("endpoint is not a public host");
            }
        }
    }
    Ok(url)
}

// ---------------------------------------------------------------- VAPID

/// The server's VAPID key pair.
pub struct Vapid {
    pair: signature::EcdsaKeyPair,
    public: String,
}

impl Vapid {
    /// Reads `secrets/vapid_key`, creating it (private, PKCS#8 as base64url) on first use.
    pub fn load_or_create(state_dir: &Path) -> Result<Self> {
        let dir = state_dir.join("secrets");
        fsutil::ensure_private_dir(&dir)?;
        let path = dir.join("vapid_key");
        if !path.exists() {
            let rng = rand::SystemRandom::new();
            let pkcs8 = signature::EcdsaKeyPair::generate_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &rng,
            )
            .map_err(|_| anyhow::anyhow!("cannot generate the notification key"))?;
            // A concurrent start may have created it first; reading below uses whichever won.
            let _ = fsutil::write_new_private_file(&path, B64.encode(pkcs8.as_ref()).as_bytes());
        }
        let text = fsutil::read_secret_file(&path)?;
        let pkcs8 = B64
            .decode(text.as_bytes())
            .context("secrets/vapid_key is not valid")?;
        Self::from_pkcs8(&pkcs8)
    }

    fn from_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        let rng = rand::SystemRandom::new();
        let pair = signature::EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8,
            &rng,
        )
        .map_err(|_| anyhow::anyhow!("secrets/vapid_key is not a P-256 key"))?;
        let public = B64.encode(signature::KeyPair::public_key(&pair).as_ref());
        Ok(Self { pair, public })
    }

    /// The public key, base64url, as the browser's `applicationServerKey`.
    pub fn public_key(&self) -> &str {
        &self.public
    }

    /// The `Authorization` value for a push to `endpoint`, valid for 12 hours.
    fn authorization(&self, endpoint: &reqwest::Url, exp: i64) -> Result<String> {
        let origin = endpoint.origin().ascii_serialization();
        let head = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = B64.encode(
            json!({ "aud": origin, "exp": exp, "sub": VAPID_SUBJECT })
                .to_string()
                .as_bytes(),
        );
        let signing = format!("{head}.{claims}");
        let rng = rand::SystemRandom::new();
        let sig = self
            .pair
            .sign(&rng, signing.as_bytes())
            .map_err(|_| anyhow::anyhow!("cannot sign"))?;
        Ok(format!(
            "vapid t={signing}.{},k={}",
            B64.encode(sig.as_ref()),
            self.public
        ))
    }
}

// ---------------------------------------------------------------- aes128gcm (RFC 8291)

struct Len(usize);
impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, info: &[u8], out: &mut [u8]) -> Result<()> {
    prk.expand(&[info], Len(out.len()))
        .and_then(|o| o.fill(out))
        .map_err(|_| anyhow::anyhow!("key derivation failed"))
}

/// The body of a push message: the encrypted record of RFC 8291 section 3, given the ECDH
/// secret the sender and the browser share.
fn seal(
    ecdh_secret: &[u8],
    as_public: &[u8],
    ua_public: &[u8],
    auth: &[u8],
    salt: &[u8; 16],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(ecdh_secret);
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let mut ikm = [0u8; 32];
    expand(&prk_key, &key_info, &mut ikm)?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    expand(&prk, b"Content-Encoding: aes128gcm\0", &mut cek)?;
    expand(&prk, b"Content-Encoding: nonce\0", &mut nonce)?;

    // One record: the message, then 0x02 to say it is the last.
    let mut record = plaintext.to_vec();
    record.push(2);
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek)
            .map_err(|_| anyhow::anyhow!("bad content key"))?,
    );
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::empty(),
        &mut record,
    )
    .map_err(|_| anyhow::anyhow!("encryption failed"))?;

    let mut body = Vec::with_capacity(21 + as_public.len() + record.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(as_public);
    body.extend_from_slice(&record);
    Ok(body)
}

/// Encrypts `plaintext` for a browser's subscription keys, with a fresh sender key and salt.
pub fn encrypt(ua_public: &[u8], auth: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    if ua_public.len() != 65 || ua_public[0] != 4 || auth.len() != 16 {
        bail!("bad subscription keys");
    }
    if plaintext.len() > 3000 {
        bail!("message too long");
    }
    let rng = rand::SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| anyhow::anyhow!("cannot generate a key"))?;
    let as_public = private
        .compute_public_key()
        .map_err(|_| anyhow::anyhow!("cannot generate a key"))?;
    let mut salt = [0u8; 16];
    rng.fill(&mut salt)
        .map_err(|_| anyhow::anyhow!("no randomness"))?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    agreement::agree_ephemeral(private, &peer, |secret| {
        seal(
            secret,
            as_public.as_ref(),
            ua_public,
            auth,
            &salt,
            plaintext,
        )
    })
    .map_err(|_| anyhow::anyhow!("not a valid browser key"))?
}

// ---------------------------------------------------------------- the feed's new items

/// Something worth a notification.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Item {
    pub key: String,
    pub body: String,
    pub url: String,
}

/// The app route of a media id: a movie's page, or the show of a season or episode.
fn route_of(id: &str) -> String {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let parts: Vec<&str> = id.split(':').collect();
    match parts.as_slice() {
        ["movie", "tmdb", n] if digits(n) => format!("/#/movie/{id}"),
        [
            "series" | "season" | "episode",
            src @ ("tvdb" | "tvmaze"),
            n,
            ..,
        ] if digits(n) => {
            format!("/#/show/series:{src}:{n}")
        }
        _ => "/".into(),
    }
}

fn describe(i: &Value) -> Option<String> {
    let title = i["title"].as_str()?.trim();
    if title.is_empty() {
        return None;
    }
    Some(match i["detail"].as_str().filter(|d| !d.is_empty()) {
        Some(d) => format!("{title}, {d}"),
        None => title.to_string(),
    })
}

/// The Arrived and Needs-you entries of a feed reply.
pub fn items_from_feed(feed: &Value) -> Vec<Item> {
    let mut out = vec![];
    for i in feed["arrived"].as_array().into_iter().flatten() {
        let (Some(what), Some(id), Some(at)) =
            (describe(i), i["media_id"].as_str(), i["at"].as_str())
        else {
            continue;
        };
        out.push(Item {
            key: format!("arrived:{id}:{at}"),
            body: format!("{what} is ready to watch"),
            url: route_of(id),
        });
    }
    for i in feed["needs_you"].as_array().into_iter().flatten() {
        let (Some(what), Some(key)) = (describe(i), i["key"].as_str()) else {
            continue;
        };
        // Keyed by the item alone: its `at` moves on every download poll, so it is not part
        // of the key (an unchanged problem must not be announced again).
        out.push(Item {
            key: format!("needs:{key}"),
            body: format!("{what} needs you"),
            url: route_of(i["media_id"].as_str().unwrap_or_default()),
        });
    }
    out
}

/// The items to announce: none on the very first pass (they are only recorded), otherwise
/// those not seen before.
pub fn fresh<'a>(seen: &HashSet<String>, started: bool, items: &'a [Item]) -> Vec<&'a Item> {
    if !started {
        return vec![];
    }
    items.iter().filter(|i| !seen.contains(&i.key)).collect()
}

// ---------------------------------------------------------------- storage and sending

/// A stored subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sub {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

/// Why a subscription was refused.
pub fn check_keys(p256dh: &str, auth: &str) -> Result<(), &'static str> {
    let k = B64.decode(p256dh).map_err(|_| "p256dh is not base64url")?;
    if k.len() != 65 || k[0] != 4 {
        return Err("p256dh is not a P-256 public key");
    }
    let a = B64.decode(auth).map_err(|_| "auth is not base64url")?;
    if a.len() != 16 {
        return Err("auth must be 16 bytes");
    }
    Ok(())
}

pub struct Push {
    db: Mutex<rusqlite::Connection>,
    vapid: Vapid,
}

impl Push {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join("web.sqlite");
        if path.exists() {
            fsutil::check_private_file(&path)?;
        } else {
            fsutil::write_new_private_file(&path, b"")?;
        }
        let db = rusqlite::Connection::open(&path).context("open web.sqlite")?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS push_subs (
                endpoint TEXT PRIMARY KEY, p256dh TEXT NOT NULL, auth TEXT NOT NULL,
                session TEXT, created INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS push_sent (item TEXT PRIMARY KEY, at INTEGER);",
        )?;
        Ok(Self {
            db: Mutex::new(db),
            vapid: Vapid::load_or_create(state_dir)?,
        })
    }

    pub fn public_key(&self) -> &str {
        self.vapid.public_key()
    }

    /// Adds (or refreshes) a device. False when there are already too many devices.
    pub fn subscribe(&self, sub: &Sub, session: Option<&str>) -> Result<bool> {
        let db = self.db.lock().unwrap();
        let known: i64 = db.query_row(
            "SELECT COUNT(*) FROM push_subs WHERE endpoint != ?1",
            [&sub.endpoint],
            |r| r.get(0),
        )?;
        if known >= MAX_SUBS {
            return Ok(false);
        }
        db.execute(
            "INSERT INTO push_subs (endpoint, p256dh, auth, session, created) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(endpoint) DO UPDATE SET p256dh = ?2, auth = ?3, session = ?4",
            rusqlite::params![sub.endpoint, sub.p256dh, sub.auth, session, now()],
        )?;
        Ok(true)
    }

    pub fn unsubscribe(&self, endpoint: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM push_subs WHERE endpoint = ?1", [endpoint])?;
        Ok(())
    }

    fn subs(&self) -> Vec<Sub> {
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT endpoint, p256dh, auth FROM push_subs") else {
            return vec![];
        };
        st.query_map([], |r| {
            Ok(Sub {
                endpoint: r.get(0)?,
                p256dh: r.get(1)?,
                auth: r.get(2)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// Records the items as announced; returns the keys seen before and whether any pass has run.
    fn seen(&self) -> Result<(HashSet<String>, bool)> {
        let db = self.db.lock().unwrap();
        db.execute(
            "DELETE FROM push_sent WHERE item != ?1 AND at < ?2",
            rusqlite::params![INIT_MARK, now() - KEEP_SENT],
        )?;
        let mut st = db.prepare("SELECT item FROM push_sent")?;
        let all: HashSet<String> = st
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        let started = all.contains(INIT_MARK);
        Ok((all, started))
    }

    fn record(&self, keys: impl IntoIterator<Item = String>) -> Result<()> {
        let db = self.db.lock().unwrap();
        for k in keys {
            db.execute(
                "INSERT OR IGNORE INTO push_sent (item, at) VALUES (?1, ?2)",
                rusqlite::params![k, now()],
            )?;
        }
        Ok(())
    }

    /// One pass over a feed reply: announce what is new to every device (nothing is sent when
    /// there are none), remember everything shown. Returns how many new items there were.
    pub async fn pass(&self, feed: &Value) -> Result<usize> {
        let items = items_from_feed(feed);
        let (seen, started) = self.seen()?;
        let new: Vec<Item> = fresh(&seen, started, &items).into_iter().cloned().collect();
        self.record(
            items
                .iter()
                .map(|i| i.key.clone())
                .chain((!started).then(|| INIT_MARK.to_string())),
        )?;
        if new.is_empty() {
            return Ok(0);
        }
        let subs = self.subs();
        for item in &new {
            let payload = json!({ "title": "Arr", "body": item.body, "url": item.url }).to_string();
            for sub in &subs {
                self.send(sub, payload.as_bytes()).await;
            }
        }
        Ok(new.len())
    }

    /// Sends one message; forgets the device when its push service says it is gone.
    async fn send(&self, sub: &Sub, payload: &[u8]) {
        match self.try_send(sub, payload).await {
            Ok(status) if status == 404 || status == 410 => {
                let _ = self.unsubscribe(&sub.endpoint);
                tracing::info!("a notification device is gone; forgot it");
            }
            Ok(status) if status >= 400 => {
                tracing::warn!(status, "a push service refused a notification");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "could not send a notification"),
        }
    }

    async fn try_send(&self, sub: &Sub, payload: &[u8]) -> Result<u16> {
        let url = validate_endpoint(&sub.endpoint).map_err(anyhow::Error::msg)?;
        let ua = B64.decode(&sub.p256dh)?;
        let auth = B64.decode(&sub.auth)?;
        let body = encrypt(&ua, &auth, payload)?;
        let authorization = self.vapid.authorization(&url, now() + 12 * 3600)?;
        let mut client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            // A proxy would resolve the name itself and bypass the checked address below.
            .no_proxy();
        // Resolve once, check every address, and connect to what was checked.
        if let Some(url::Host::Domain(d)) = url.host() {
            let port = url.port_or_known_default().unwrap_or(443);
            let addrs: Vec<_> = tokio::net::lookup_host((d, port)).await?.collect();
            if addrs.is_empty() || !addrs.iter().all(|a| address_ok(a.ip())) {
                bail!("the push service's address is not public");
            }
            client = client.resolve(d, addrs[0]);
        }
        // Errors never carry the endpoint: its path is the device's subscription token.
        let resp = client
            .build()
            .map_err(reqwest::Error::without_url)?
            .post(url)
            .header("TTL", TTL)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("Urgency", "normal")
            .header("Authorization", authorization)
            .body(body)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        Ok(resp.status().as_u16())
    }
}

/// The feed as the web app sees it, read in-process with the API token.
async fn read_feed(api: &Router, token: &str) -> Option<Value> {
    let mut auth = axum::http::HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    auth.set_sensitive(true);
    let req = Request::builder()
        .uri("/api/v1/feed")
        .header(axum::http::header::AUTHORIZATION, auth)
        .body(Body::empty())
        .ok()?;
    let Ok(resp) = api.clone().oneshot(req).await;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The background task: a pass every minute, for as long as Arr runs.
pub async fn run(push: std::sync::Arc<Push>, api: Router, token: String) {
    let mut every = tokio::time::interval(tick());
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        if let Some(feed) = read_feed(&api, &token).await
            && let Err(e) = push.pass(&feed).await
        {
            tracing::warn!(error = %e, "notification pass failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Vec<u8> {
        B64.decode(s.split_whitespace().collect::<String>())
            .unwrap()
    }

    /// RFC 8291 section 5 / appendix A.
    #[test]
    fn encryption_matches_the_rfc_8291_example() {
        let body = seal(
            &b("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"),
            &b("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"),
            &b("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"),
            &b("BTBZMqHH6r4Tts7J_aSIgg"),
            &b("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap(),
            b"When I grow up, I want to be a watermelon",
        )
        .unwrap();
        let want = b(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml
             mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT
             pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
        );
        assert_eq!(body, want);
        assert_eq!(body.len(), 144);
    }

    /// The browser's side of RFC 8291: a message made by `encrypt` opens with the receiver's key.
    #[test]
    fn encrypt_makes_a_message_the_receiver_can_read() {
        let rng = rand::SystemRandom::new();
        let ua = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let ua_pub = ua.compute_public_key().unwrap();
        let auth = [7u8; 16];
        let body = encrypt(ua_pub.as_ref(), &auth, b"hello").unwrap();
        assert_eq!(&body[16..20], &RECORD_SIZE.to_be_bytes());
        assert_eq!(body[20], 65);
        let as_pub = &body[21..86];
        let salt: [u8; 16] = body[..16].try_into().unwrap();
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_pub);
        let (cek, nonce) = agreement::agree_ephemeral(ua, &peer, |secret| {
            let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, &auth).extract(secret);
            let mut info = b"WebPush: info\0".to_vec();
            info.extend_from_slice(ua_pub.as_ref());
            info.extend_from_slice(as_pub);
            let mut ikm = [0u8; 32];
            expand(&prk_key, &info, &mut ikm).unwrap();
            let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt).extract(&ikm);
            let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
            expand(&prk, b"Content-Encoding: aes128gcm\0", &mut cek).unwrap();
            expand(&prk, b"Content-Encoding: nonce\0", &mut nonce).unwrap();
            (cek, nonce)
        })
        .unwrap();
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
        let mut record = body[86..].to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::empty(),
                &mut record,
            )
            .unwrap();
        assert_eq!(plain, b"hello\x02");
    }

    #[test]
    fn encrypt_refuses_bad_keys() {
        assert!(encrypt(&[4; 10], &[0; 16], b"x").is_err());
        assert!(encrypt(&[4; 65], &[0; 15], b"x").is_err());
        assert!(
            encrypt(&[4; 65], &[0; 16], b"x").is_err(),
            "not on the curve"
        );
    }

    #[test]
    fn endpoints_must_be_public_https() {
        for ok in [
            "https://fcm.googleapis.com/fcm/send/abc",
            "https://updates.push.services.mozilla.com/wpush/v2/abc",
            "https://web.push.apple.com/QGx",
            "https://8.8.8.8/x",
        ] {
            assert!(validate_endpoint(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://fcm.googleapis.com/x",
            "https://localhost/x",
            "https://printer/x",
            "https://nas.local/x",
            "https://router.lan/x",
            "https://127.0.0.1/x",
            "https://10.0.0.5/x",
            "https://192.168.1.1/x",
            "https://172.16.0.1/x",
            "https://169.254.169.254/latest/meta-data",
            "https://100.64.0.1/x",
            "https://0.0.0.0/x",
            "https://[::1]/x",
            "https://[fe80::1]/x",
            "https://[fd00::1]/x",
            "https://[::ffff:10.0.0.1]/x",
            "https://fcm.googleapis.com:8443/x",
            "https://user:pw@fcm.googleapis.com/x",
            "ftp://fcm.googleapis.com/x",
            "not a url",
            "",
        ] {
            assert!(validate_endpoint(bad).is_err(), "{bad}");
        }
        let long = format!("https://fcm.googleapis.com/{}", "a".repeat(MAX_ENDPOINT));
        assert!(validate_endpoint(&long).is_err());
    }

    #[test]
    fn subscription_keys_are_checked() {
        let k = B64.encode([&[4u8][..], &[1u8; 64][..]].concat());
        let a = B64.encode([0u8; 16]);
        assert!(check_keys(&k, &a).is_ok());
        assert!(check_keys(&k, &B64.encode([0u8; 8])).is_err());
        assert!(check_keys(&B64.encode([4u8; 10]), &a).is_err());
        assert!(check_keys("***", &a).is_err());
    }

    fn feed() -> Value {
        json!({
            "arrived": [
                { "media_id": "movie:tmdb:603", "title": "The Matrix", "detail": null, "at": "2026-01-01T00:00:00Z" },
                { "media_id": "season:tvdb:372264:2", "title": "Slow Horses", "detail": "Season 2", "at": "2026-01-02T00:00:00Z" },
            ],
            "needs_you": [
                { "key": "download_failed:g1", "media_id": "episode:tvdb:372264:1:2", "title": "Slow Horses", "detail": "S1 E2" },
            ],
        })
    }

    #[test]
    fn items_say_what_happened_and_where_to_look() {
        let items = items_from_feed(&feed());
        let bodies: Vec<&str> = items.iter().map(|i| i.body.as_str()).collect();
        assert_eq!(
            bodies,
            [
                "The Matrix is ready to watch",
                "Slow Horses, Season 2 is ready to watch",
                "Slow Horses, S1 E2 needs you"
            ]
        );
        assert_eq!(items[0].url, "/#/movie/movie:tmdb:603");
        assert_eq!(items[1].url, "/#/show/series:tvdb:372264");
        assert_eq!(items[2].url, "/#/show/series:tvdb:372264");
        assert_eq!(route_of("garbage"), "/");
        assert!(items_from_feed(&json!({})).is_empty());
    }

    #[test]
    fn an_unchanged_needs_you_item_is_not_announced_again_when_its_time_moves() {
        let item = |at: &str| {
            json!({ "needs_you": [{ "key": "download_failed:g1", "media_id": "movie:tmdb:603",
                "title": "The Matrix", "detail": null, "at": at }] })
        };
        let first = items_from_feed(&item("2026-01-01T00:00:00Z"));
        let seen: HashSet<String> = first.iter().map(|i| i.key.clone()).collect();
        let polled = items_from_feed(&item("2026-01-01T00:05:00Z"));
        assert!(
            fresh(&seen, true, &polled).is_empty(),
            "same problem, later poll"
        );
    }

    #[test]
    fn the_first_pass_sends_nothing_and_later_ones_only_what_is_new() {
        let items = items_from_feed(&feed());
        let none = HashSet::new();
        assert!(fresh(&none, false, &items).is_empty(), "first run");
        let seen: HashSet<String> = items[..2].iter().map(|i| i.key.clone()).collect();
        let new = fresh(&seen, true, &items);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].key, items[2].key);
        let all: HashSet<String> = items.iter().map(|i| i.key.clone()).collect();
        assert!(fresh(&all, true, &items).is_empty());
    }

    #[tokio::test]
    async fn a_restart_does_not_resend_and_a_first_run_only_records() {
        let dir = tempfile::tempdir().unwrap();
        let push = Push::open(dir.path()).unwrap();
        assert_eq!(push.pass(&feed()).await.unwrap(), 0, "first run");
        drop(push);
        let push = Push::open(dir.path()).unwrap();
        assert_eq!(push.pass(&feed()).await.unwrap(), 0, "restart");
        let mut more = feed();
        more["arrived"].as_array_mut().unwrap().push(
            json!({ "media_id": "movie:tmdb:1", "title": "New", "at": "2026-01-03T00:00:00Z" }),
        );
        assert_eq!(push.pass(&more).await.unwrap(), 1);
        assert_eq!(push.pass(&more).await.unwrap(), 0);
    }

    #[test]
    fn the_vapid_key_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let a = Vapid::load_or_create(dir.path()).unwrap();
        let b = Vapid::load_or_create(dir.path()).unwrap();
        assert_eq!(a.public_key(), b.public_key());
        assert_eq!(B64.decode(a.public_key()).unwrap().len(), 65);
        let file = dir.path().join("secrets/vapid_key");
        fsutil::check_private_file(&file).unwrap();
        let url = validate_endpoint("https://fcm.googleapis.com/fcm/send/x").unwrap();
        let auth = a.authorization(&url, 2_000_000_000).unwrap();
        let rest = auth.strip_prefix("vapid t=").unwrap();
        let (jwt, k) = rest.split_once(",k=").unwrap();
        assert_eq!(k, a.public_key());
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["exp"], 2_000_000_000);
        // The signature verifies against the published key.
        let pk = signature::UnparsedPublicKey::new(
            &signature::ECDSA_P256_SHA256_FIXED,
            B64.decode(k).unwrap(),
        );
        pk.verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &B64.decode(parts[2]).unwrap(),
        )
        .unwrap();
    }
}
