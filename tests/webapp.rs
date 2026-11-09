//! The web app: pages and headers, sign-in links and sessions, the allow-listed proxy, the
//! feed, ideas and quick-add endpoints, and Google sign-in for the app. The common helpers
//! serve `api::router`; these tests serve `api::app` (the app's own routes) over the same
//! state.
#![recursion_limit = "256"]

mod common;

use arr::{api, config, fsutil};
use axum::extract::{Form, State};
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use common::*;
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

// ------------------------------------------------------------------ harness

struct Site {
    base: String,
    http: reqwest::Client,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn serve(state: Arc<api::AppState>) -> Site {
    let (app, _stop) = api::app(state).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Site {
        base,
        http: client(),
    }
}

impl Site {
    fn plain(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, format!("{}{path}", self.base))
    }

    /// A request the app makes: the marker header and the session cookie.
    fn app(&self, method: Method, path: &str, cookie: Option<&str>) -> reqwest::RequestBuilder {
        let rb = self.plain(method, path).header("x-arr-app", "1");
        match cookie {
            Some(c) => rb.header("cookie", c),
            None => rb,
        }
    }

    async fn json(&self, rb: reqwest::RequestBuilder) -> (u16, Value) {
        let r = rb.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Signs in with a fresh `arr app-link` link; returns the cookie to send back.
    async fn sign_in(&self, e: &Exec) -> String {
        let token = link_token(e);
        let r = self.consume(&token).await;
        assert_eq!(r.status(), 303);
        session_cookie(&r)
    }

    async fn consume(&self, token: &str) -> reqwest::Response {
        self.plain(Method::POST, "/app/link")
            .form(&[("t", token)])
            .send()
            .await
            .unwrap()
    }
}

fn link_token(e: &Exec) -> String {
    let link = arr::webapp::app_link(&e.state.cfg).unwrap();
    let (path, token) = link.split_once("/app/link?t=").unwrap();
    assert!(path.starts_with("http://localhost:"), "{path}");
    token.to_string()
}

/// `arr_session=<value>` from a reply's `Set-Cookie`.
fn session_cookie(r: &reqwest::Response) -> String {
    r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn exec() -> Exec {
    start_exec(ExecOptions {
        tmdb: true,
        ..Default::default()
    })
    .await
}

fn first_grab(v: &Value) -> String {
    v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{v}"))
        .to_string()
}

// ------------------------------------------------------------------ pages and sessions

#[tokio::test]
async fn pages_and_assets_carry_the_security_headers() {
    let e = exec().await;
    let site = serve(e.state.clone()).await;
    let csp = "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";
    for (path, kind, cache) in [
        ("/", "text/html", "no-cache"),
        ("/app/app.js", "text/javascript", "no-cache"),
        ("/app/app.css", "text/css", "no-cache"),
        ("/app/mascot.svg", "image/svg+xml", "no-cache"),
        ("/app/tmdb.svg", "image/svg+xml", "no-cache"),
        ("/app/qrcodegen.js", "text/javascript", "no-cache"),
        (
            "/app/manifest.webmanifest",
            "application/manifest+json",
            "no-cache",
        ),
        (
            "/app/icons/apple-touch-icon.png",
            "image/png",
            "public, max-age=604800, immutable",
        ),
        (
            "/app/icons/icon-192.png",
            "image/png",
            "public, max-age=604800, immutable",
        ),
        (
            "/app/icons/icon-512.png",
            "image/png",
            "public, max-age=604800, immutable",
        ),
        (
            "/app/icons/icon-maskable-512.png",
            "image/png",
            "public, max-age=604800, immutable",
        ),
        (
            "/app/fonts/nunito-latin.woff2",
            "font/woff2",
            "public, max-age=604800, immutable",
        ),
        (
            "/app/fonts/nunito-latin-ext.woff2",
            "font/woff2",
            "public, max-age=604800, immutable",
        ),
    ] {
        let r = site.plain(Method::GET, path).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
        let h = r.headers();
        assert!(
            h["content-type"].to_str().unwrap().starts_with(kind),
            "{path}"
        );
        assert_eq!(h["cache-control"], cache, "{path}");
        assert_eq!(h["content-security-policy"], csp, "{path}");
        assert_eq!(h["x-frame-options"], "DENY", "{path}");
        assert_eq!(h["x-content-type-options"], "nosniff", "{path}");
        assert_eq!(h["referrer-policy"], "same-origin", "{path}");
        assert!(!r.bytes().await.unwrap().is_empty(), "{path}");
    }
    // The page needs no inline script or style.
    let html = site
        .plain(Method::GET, "/")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("/app/app.js") && html.contains("/app/app.css"));
    assert!(!html.contains("<style") && !html.contains("onclick"));
    // It can be put on a home screen: a manifest, a touch icon and a theme colour.
    for want in [
        "rel=\"manifest\" href=\"/app/manifest.webmanifest\"",
        "rel=\"apple-touch-icon\" href=\"/app/icons/apple-touch-icon.png\"",
        "name=\"theme-color\"",
        "src=\"/app/qrcodegen.js\"",
    ] {
        assert!(html.contains(want), "index.html lacks {want}");
    }
    let manifest: Value = site
        .plain(Method::GET, "/app/manifest.webmanifest")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for (key, want) in [
        ("name", "Arr"),
        ("short_name", "Arr"),
        ("start_url", "/"),
        ("scope", "/"),
        ("display", "standalone"),
        ("background_color", "#fff8ef"),
        ("theme_color", "#1c8576"),
    ] {
        assert_eq!(manifest[key], want, "{key}");
    }
    // Every icon the manifest names is served, as the PNG of the size it says.
    let icons = manifest["icons"].as_array().unwrap();
    assert!(icons.iter().any(|i| i["purpose"] == "maskable"));
    for i in icons {
        let bytes = site
            .plain(Method::GET, i["src"].as_str().unwrap())
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let side: u32 = i["sizes"]
            .as_str()
            .unwrap()
            .split('x')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{i}");
        let dim = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!((dim(16), dim(20)), (side, side), "{i}");
    }
    // The API keeps answering where it always did.
    let (s, v) = site.json(site.plain(Method::GET, "/api/v1/health")).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("ok")), "{v}");
    let (s, v) = site.json(site.plain(Method::GET, "/api/v1/nope")).await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")));
}

#[tokio::test]
async fn app_requests_need_the_marker_and_the_same_origin() {
    let e = exec().await;
    let site = serve(e.state.clone()).await;
    // Without a session: 401 with the sign-in choices (no Google here).
    let (s, v) = site.json(site.app(Method::GET, "/app/api/me", None)).await;
    assert_eq!(s, 401, "{v}");
    assert_eq!(v["signin"]["google"], false);
    assert!(v["error"]["message"].is_string());
    // The marker header is required on every /app/api request, GET included.
    let (s, v) = site.json(site.plain(Method::GET, "/app/api/me")).await;
    assert_eq!((s, v["error"]["code"].as_str()), (403, Some("forbidden")));
    let cookie = site.sign_in(&e).await;
    let (s, _) = site
        .json(
            site.plain(Method::GET, "/app/api/v1/feed")
                .header("cookie", &cookie),
        )
        .await;
    assert_eq!(s, 403);
    // A foreign Origin is refused, even with a good session; the app's own origin is fine.
    let (s, _) = site
        .json(
            site.app(Method::GET, "/app/api/me", Some(&cookie))
                .header("origin", "https://evil.example"),
        )
        .await;
    assert_eq!(s, 403);
    let (s, _) = site
        .json(
            site.app(Method::POST, "/app/api/v1/quick-add", Some(&cookie))
                .header("origin", "https://evil.example")
                .json(&json!({ "media_id": "movie:tmdb:603" })),
        )
        .await;
    assert_eq!(s, 403);
    let (s, v) = site
        .json(
            site.app(Method::GET, "/app/api/me", Some(&cookie))
                .header("origin", site.base.clone()),
        )
        .await;
    assert_eq!((s, &v["email"]), (200, &Value::Null), "{v}");
    // Google sign-in does not exist without [mcp.oauth].
    let (s, _) = site.json(site.plain(Method::GET, "/app/signin")).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn a_link_signs_in_once_and_expires() {
    let e = exec().await;
    let site = serve(e.state.clone()).await;
    let token = link_token(&e);
    // The page never consumes the link (link previews fetch it): it only offers a button.
    for _ in 0..2 {
        let r = site
            .plain(Method::GET, &format!("/app/link?t={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let html = r.text().await.unwrap();
        assert!(
            html.contains("method=\"post\"") && html.contains(&token),
            "{html}"
        );
    }
    // A foreign origin cannot use it, and that does not burn it.
    let r = site
        .plain(Method::POST, "/app/link")
        .header("origin", "https://evil.example")
        .form(&[("t", token.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = site.consume(&token).await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/");
    let set = r.headers()["set-cookie"].to_str().unwrap().to_string();
    for part in [
        "arr_session=",
        "Path=/",
        "Max-Age=2592000",
        "HttpOnly",
        "SameSite=Lax",
    ] {
        assert!(set.contains(part), "{set}");
    }
    assert!(!set.contains("Secure"), "plain http: {set}");
    let cookie = set.split(';').next().unwrap().to_string();
    // Only the hash of the session is stored.
    let db = rusqlite::Connection::open(e.dir.join("web.sqlite")).unwrap();
    let stored: Vec<String> = db
        .prepare("SELECT hash FROM sessions")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(stored.len(), 1);
    assert!(!cookie.contains(&stored[0]) && !stored[0].contains(&cookie[12..]));
    // Signed in: me, then the feed through the proxy.
    let (s, v) = site
        .json(site.app(Method::GET, "/app/api/me", Some(&cookie)))
        .await;
    assert_eq!((s, &v["email"]), (200, &Value::Null), "{v}");
    // No remote access: the app is told there is no public address.
    assert!(v["public_url"].is_null(), "{v}");
    let (s, v) = site
        .json(site.app(Method::GET, "/app/api/v1/feed", Some(&cookie)))
        .await;
    assert_eq!(s, 200, "{v}");
    assert!(v["needs_you"].is_array() && v["coming_up"].is_array());
    // Single use.
    let r = site.consume(&token).await;
    assert_eq!(r.status(), 410);
    assert!(r.headers().get("set-cookie").is_none());
    // Expiry (15 minutes): an old link no longer works.
    let old = link_token(&e);
    db.execute("UPDATE links SET expires_at = 1", []).unwrap();
    assert_eq!(site.consume(&old).await.status(), 410);
    assert_eq!(
        site.consume("not-a-real-token").await.status(),
        410,
        "unknown tokens are refused"
    );
    // Sign out: the cookie is cleared and the session is gone.
    let r = site
        .app(Method::POST, "/app/api/signout", Some(&cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(
        r.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let (s, _) = site
        .json(site.app(Method::GET, "/app/api/me", Some(&cookie)))
        .await;
    assert_eq!(s, 401);
    let (s, _) = site
        .json(site.app(Method::GET, "/app/api/v1/feed", Some(&cookie)))
        .await;
    assert_eq!(s, 401);
}

#[tokio::test]
async fn the_proxy_only_passes_the_allow_listed_routes() {
    let e = exec().await;
    let site = serve(e.state.clone()).await;
    let none = site
        .json(site.app(Method::GET, "/app/api/v1/feed", None))
        .await;
    assert_eq!(none.0, 401);
    let cookie = site.sign_in(&e).await;
    let c = Some(cookie.as_str());
    for (method, path) in [
        (Method::POST, "/app/api/v1/library"),
        (Method::GET, "/app/api/v1/settings"),
        (Method::GET, "/app/api/v1/health"),
        (Method::GET, "/app/api/v1/queue"),
        (Method::GET, "/app/api/v1/openapi.json"),
        (Method::DELETE, "/app/api/v1/library/movie:tmdb:603"),
        (Method::POST, "/app/api/v1/queue/grb_x/cancel"),
        (Method::GET, "/app/api/v1/quick-add"),
        (Method::POST, "/app/api/v1/feed"),
        (Method::GET, "/app/api/v1/library/movies/extra"),
    ] {
        let (s, v) = site.json(site.app(method.clone(), path, c)).await;
        assert_eq!(
            (s, v["error"]["code"].as_str()),
            (404, Some("not_found")),
            "{method} {path}"
        );
    }
    // The query string reaches the API, which still validates it.
    let (s, v) = site
        .json(site.app(Method::GET, "/app/api/v1/discover?kind=series&q=fargo", c))
        .await;
    assert_eq!(s, 200, "{v}");
    assert!(
        v["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["title"] == "Fargo")
    );
    let (s, _) = site
        .json(site.app(Method::GET, "/app/api/v1/discover?kind=series", c))
        .await;
    assert_eq!(s, 400);
    let (s, v) = site
        .json(site.app(Method::GET, "/app/api/v1/library/movies?limit=2", c))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    // The API token is never needed (or accepted) from the browser side.
    let (s, _) = site
        .json(
            site.app(Method::GET, "/app/api/v1/feed", None)
                .bearer_auth(TOKEN),
        )
        .await;
    assert_eq!(s, 401);
}

// ------------------------------------------------------------------ feed

fn matrix_files() -> Vec<(String, Vec<u8>)> {
    vec![("The.Matrix.1999.1080p.WEB-DL.x264-UM.mkv".into(), mkv(3))]
}

#[tokio::test]
async fn the_feed_tells_what_is_happening_in_plain_words() {
    let e = exec().await;
    let site = serve(e.state.clone()).await;
    let cookie = site.sign_in(&e).await;
    let feed = || async {
        let (s, v) = site
            .json(site.app(Method::GET, "/app/api/v1/feed", Some(&cookie)))
            .await;
        assert_eq!(s, 200, "{v}");
        v
    };
    let empty = feed().await;
    assert_eq!(empty["movies_enabled"], true);
    for k in ["needs_you", "on_its_way", "arrived", "looking", "coming_up"] {
        assert_eq!(empty[k], json!([]), "{k}");
    }
    assert!(OffsetDateTime::parse(empty["generated_at"].as_str().unwrap(), &Rfc3339).is_ok());

    // A download in progress.
    let (hash, r) = e.publish(
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        matrix_files(),
        8.0,
        false,
    );
    e.search_results("The Matrix 1999", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let matrix = first_grab(&v);
    let v = feed().await;
    let item = &v["on_its_way"][0];
    assert_eq!(item["grab_id"], matrix.as_str(), "{v}");
    assert_eq!(
        (
            item["title"].clone(),
            item["year"].clone(),
            item["detail"].clone()
        ),
        (json!("The Matrix"), json!(1999), Value::Null)
    );
    assert_eq!(item["media_id"], "movie:tmdb:603");
    assert_eq!(item["stage"], "found", "not submitted yet");
    assert_eq!(item["paused"], false);
    e.tick().await;
    e.set_state(&hash, "downloading", 0.5);
    e.tick().await;
    let v = feed().await;
    assert_eq!(v["on_its_way"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["on_its_way"][0]["stage"], "downloading");
    assert_eq!(v["on_its_way"][0]["progress"], 0.5, "{v}");
    assert!(!v.to_string().contains("1080p"), "no release names: {v}");

    // A download that failed needs the person.
    let title = "Slow.Horses.S01E02.1080p.WEB-DL.x264-ERR";
    let (bad_hash, r) = e.publish(
        title,
        title,
        vec![(format!("{title}.mkv"), mkv(1))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E02", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let failed = first_grab(&v);
    e.tick().await;
    e.set_state(&bad_hash, "error", 0.2);
    let t0 = OffsetDateTime::now_utc();
    e.tick_at(t0).await;
    e.tick_at(t0 + std::time::Duration::from_secs(31 * 60))
        .await;
    assert_eq!(e.grab_row(&failed).state, "failed");
    let v = feed().await;
    let need = &v["needs_you"][0];
    assert_eq!(need["problem"], "The download didn't work", "{v}");
    assert_eq!(need["grab_id"], failed.as_str());
    assert_eq!(need["title"], "Slow Horses");
    assert!(need["detail"].as_str().unwrap().starts_with("S1 E2"), "{v}");
    assert!(need["key"].as_str().unwrap().contains(&failed));
    assert!(need["at"].is_string());

    // It arrived.
    e.complete(&hash);
    e.tick().await;
    assert_eq!(e.grab_row(&matrix).state, "imported");
    let v = feed().await;
    assert_eq!(v["on_its_way"], json!([]), "{v}");
    let a = &v["arrived"][0];
    assert_eq!(
        (a["title"].clone(), a["media_id"].clone()),
        (json!("The Matrix"), json!("movie:tmdb:603")),
        "{v}"
    );
    assert!(a["at"].is_string());

    // A request nobody could fill yet, and an episode coming up.
    let (s, q) = site
        .json(
            site.app(Method::POST, "/app/api/v1/quick-add", Some(&cookie))
                .json(&json!({ "media_id": "movie:tmdb:900001" })),
        )
        .await;
    assert_eq!((s, q["outcome"].as_str()), (200, Some("looking")), "{q}");
    let soon = (OffsetDateTime::now_utc() + std::time::Duration::from_secs(3 * 86_400))
        .format(&Rfc3339)
        .unwrap();
    e.mock.lock().unwrap().tvmaze_extra.insert(
        "9005".into(),
        vec![json!({ "season": 2, "number": 3, "name": "Next Up", "type": "regular", "airstamp": soon, "runtime": 1 })],
    );
    let (s, q) = site
        .json(
            site.app(Method::POST, "/app/api/v1/quick-add", Some(&cookie))
                .json(&json!({ "media_id": "series:tvdb:999005" })),
        )
        .await;
    assert_eq!(s, 200, "{q}");
    let v = feed().await;
    let looking: Vec<&str> = v["looking"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["title"].as_str().unwrap())
        .collect();
    assert!(looking.contains(&"Leviticus"), "{v}");
    let l = v["looking"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["title"] == "Leviticus")
        .unwrap();
    assert_eq!(l["until"], Value::Null);
    assert!(l["next_try_at"].is_string());
    let c = &v["coming_up"][0];
    assert_eq!(
        (
            c["title"].clone(),
            c["detail"].clone(),
            c["media_id"].clone()
        ),
        (
            json!("Parity Partial"),
            json!("S2 E3 \u{b7} Next Up"),
            json!("episode:tvdb:999005:2:3")
        ),
        "{v}"
    );

    // Retry goes through the proxy for a download.
    let (s, v) = site
        .json(site.app(
            Method::POST,
            &format!("/app/api/v1/queue/{failed}/retry"),
            Some(&cookie),
        ))
        .await;
    assert_eq!(s, 200, "{v}");
}

// ------------------------------------------------------------------ ideas

fn scheduled(id: u32, name: &str, tvdb: Option<u32>, weight: u32, summary: &str) -> Value {
    json!({
        "id": id * 10, "name": "An episode", "season": 1, "number": 1,
        "_embedded": { "show": {
            "id": id, "name": name, "type": "Scripted", "premiered": "2022-04-01", "weight": weight,
            "externals": { "thetvdb": tvdb, "imdb": null },
            "network": null, "webChannel": { "name": "Apple TV+" },
            "summary": summary,
        } }
    })
}

#[tokio::test]
async fn ideas_come_from_the_web_schedule_and_are_cached() {
    let e = exec().await;
    let site = serve(e.state.clone()).await;
    let cookie = site.sign_in(&e).await;
    let long = format!("<p>{}</p>", "word ".repeat(100));
    {
        let mut m = e.mock.lock().unwrap();
        m.schedule = vec![
            scheduled(11, "Low Weight", Some(5001), 10, "<p>Plain.</p>"),
            scheduled(
                372,
                "Slow Horses",
                Some(372264),
                90,
                "<p>Spies &amp; <b>liars</b>.</p>",
            ),
            scheduled(13, "No Tvdb Show", None, 99, &long),
            scheduled(11, "Low Weight", Some(5001), 10, "<p>Plain.</p>"),
            scheduled(14, "Odd Rating", None, 5, "<p>Plain.</p>"),
            scheduled(15, "Odder Rating", None, 4, "<p>Plain.</p>"),
            scheduled(16, "Too High", None, 3, "<p>Plain.</p>"),
        ];
        // TVmaze's viewer rating: rounded to one decimal; none, zero, out of range or a
        // rating in an unexpected shape is no rating, and never costs the day's other shows.
        m.schedule[1]["_embedded"]["show"]["rating"] = json!({ "average": 8.26 });
        m.schedule[2]["_embedded"]["show"]["rating"] = json!({ "average": null });
        m.schedule[0]["_embedded"]["show"]["rating"] = json!({ "average": 0 });
        m.schedule[4]["_embedded"]["show"]["rating"] = json!("unknown");
        m.schedule[5]["_embedded"]["show"]["rating"] = json!({ "average": "unknown" });
        m.schedule[6]["_embedded"]["show"]["rating"] = json!({ "average": 11 });
    }
    let ideas = || async {
        let (s, v) = site
            .json(site.app(Method::GET, "/app/api/v1/ideas", Some(&cookie)))
            .await;
        assert_eq!(s, 200, "{v}");
        v
    };
    let v = ideas().await;
    assert_eq!(v["status"], "ok", "{v}");
    let items = v["items"].as_array().unwrap();
    let titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
    assert_eq!(
        titles,
        [
            "No Tvdb Show",
            "Slow Horses",
            "Low Weight",
            "Odd Rating",
            "Odder Rating",
            "Too High"
        ],
        "deduped, by weight"
    );
    assert_eq!(items[0]["media_id"], "series:tvmaze:13");
    assert_eq!(items[1]["media_id"], "series:tvdb:372264");
    assert_eq!(items[1]["in_library"], true);
    assert_eq!(items[2]["in_library"], false);
    assert_eq!(items[1]["network"], "Apple TV+");
    assert_eq!(items[1]["year"], 2022);
    assert_eq!(items[1]["summary"], "Spies & liars.");
    assert_eq!(items[1]["rating"], 8.3);
    assert!(items[0]["rating"].is_null(), "{v}");
    for i in [2, 3, 4, 5] {
        assert!(items[i]["rating"].is_null(), "{v}");
    }
    let summary = items[0]["summary"].as_str().unwrap();
    assert!(
        summary.chars().count() <= 200 && !summary.contains('<'),
        "{summary}"
    );
    // Today and the six days before it.
    let dates = e.mock.lock().unwrap().schedule_dates.clone();
    let mut unique = dates.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 7, "{dates:?}");
    let today = OffsetDateTime::now_utc().date().to_string();
    assert!(dates.contains(&today), "{dates:?} {today}");
    // Cached for six hours: no further calls.
    let before = dates.len();
    e.mock.lock().unwrap().schedule.clear();
    assert_eq!(ideas().await["items"].as_array().unwrap().len(), 6);
    assert_eq!(e.mock.lock().unwrap().schedule_dates.len(), before);
}

#[tokio::test]
async fn ideas_say_when_they_are_unavailable_or_off() {
    let e = exec().await;
    e.mock.lock().unwrap().schedule_fail = true;
    let (s, v, _) = e.json(e.get("/api/v1/ideas")).await;
    assert_eq!(
        (s, v["status"].as_str(), v["items"].clone()),
        (200, Some("unavailable"), json!([]))
    );
    let off = start_exec(ExecOptions {
        tvmaze: false,
        ..Default::default()
    })
    .await;
    let (s, v, _) = off.json(off.get("/api/v1/ideas")).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("disabled")), "{v}");
}

// ------------------------------------------------------------------ quick add

#[tokio::test]
async fn quick_add_gets_a_movie_or_a_show() {
    let e = exec().await;
    e.mock.lock().unwrap().tmdb_movies.insert(
        "900002".into(),
        tmdb_movie_with(900002, "Later Movie", 2099, "2099-03-04"),
    );
    let site = serve(e.state.clone()).await;
    let cookie = site.sign_in(&e).await;
    let add = |id: &'static str| {
        let (site, cookie) = (&site, cookie.clone());
        async move {
            site.json(
                site.app(Method::POST, "/app/api/v1/quick-add", Some(&cookie))
                    .json(&json!({ "media_id": id })),
            )
            .await
        }
    };
    // A movie with a release to get: on its way.
    let (_, r) = e.publish(
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        "The.Matrix.1999.1080p.WEB-DL.x264-UM",
        matrix_files(),
        8.0,
        false,
    );
    e.search_results("The Matrix 1999", vec![r]);
    let (s, v) = add("movie:tmdb:603").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (
            v["title"].clone(),
            v["detail"].clone(),
            v["outcome"].clone(),
            v["until"].clone()
        ),
        (
            json!("The Matrix"),
            Value::Null,
            json!("on_its_way"),
            Value::Null
        ),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().len() > 10);
    // Again: already being downloaded.
    let (_, v) = add("movie:tmdb:603").await;
    assert_eq!(v["outcome"], "on_its_way", "{v}");
    // No release anywhere: Arr keeps looking.
    let (s, v) = add("movie:tmdb:900001").await;
    assert_eq!(
        (s, v["outcome"].as_str(), v["title"].as_str()),
        (200, Some("looking"), Some("Leviticus")),
        "{v}"
    );
    // Not out yet: waiting, with the date.
    let (s, v) = add("movie:tmdb:900002").await;
    assert_eq!(
        (s, v["outcome"].as_str(), v["until"].as_str()),
        (200, Some("waiting"), Some("2099-03-04")),
        "{v}"
    );
    assert!(v["message"].as_str().unwrap().contains("4 Mar"), "{v}");
    // Already in Plex.
    let (s, v) = add("movie:tmdb:604").await;
    assert_eq!((s, v["outcome"].as_str()), (200, Some("in_plex")), "{v}");

    // A show: follow new episodes and get the latest season that has aired.
    let (s, v) = add("series:tvdb:999005").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (
            v["title"].clone(),
            v["detail"].clone(),
            v["outcome"].clone()
        ),
        (json!("Parity Partial"), json!("Season 2"), json!("looking")),
        "{v}"
    );
    let (_, lib, _) = e.json(e.get("/api/v1/library/series?q=parity")).await;
    let seasons = lib["items"][0]["seasons"].as_array().unwrap();
    let monitored = |n: u64| {
        seasons.iter().find(|s| s["number"] == n).unwrap()["monitored_episodes"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(
        monitored(1),
        1,
        "only the unaired episode of season 1: {lib}"
    );
    assert_eq!(monitored(2), 2, "the whole latest season: {lib}");
    // A show with nothing aired yet is only followed.
    let (s, v) = add("series:tvdb:999006").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (
            v["outcome"].clone(),
            v["detail"].clone(),
            v["title"].clone()
        ),
        (json!("following"), Value::Null, json!("Coming Soon Show")),
        "{v}"
    );
    // Bad requests keep the API's error envelope.
    for body in [json!({ "media_id": "nope" }), json!({})] {
        let (s, v) = site
            .json(
                site.app(Method::POST, "/app/api/v1/quick-add", Some(&cookie))
                    .json(&body),
            )
            .await;
        assert!(matches!(s, 400 | 422), "{body}: {s} {v}");
        assert!(v["error"]["code"].is_string());
    }
}

#[tokio::test]
async fn quick_add_finds_movies_through_plex_alone() {
    let e = start_exec(ExecOptions::default()).await;
    let (s, v, text) = e
        .json(
            e.post("/api/v1/quick-add")
                .json(&json!({ "media_id": "movie:tmdb:910001" })),
        )
        .await;
    assert_eq!(s, 200, "{text}");
    assert_eq!(v["title"], "Plex Released Movie", "{v}");
    assert_ne!(v["outcome"], "movies_off");
    let (_, f, _) = e.json(e.get("/api/v1/feed")).await;
    assert_eq!(f["movies_enabled"], true);
    assert_eq!(f["tmdb_backup"], false);
}

// ------------------------------------------------------------------ Google sign-in for the app

const GOOGLE_ID: &str = "web-google-client.apps.googleusercontent.com";
const OWNER: &str = "owner@example.com";

struct Google {
    claims: Value,
}

async fn start_google() -> (std::net::SocketAddr, Arc<Mutex<Google>>) {
    let state = Arc::new(Mutex::new(Google {
        claims: json!({
            "iss": "https://accounts.google.com", "aud": GOOGLE_ID, "sub": "1234567890",
            "email": OWNER, "email_verified": true,
        }),
    }));
    let app =
        axum::Router::new()
            .route(
                "/google/token",
                post(
                    |State(g): State<Arc<Mutex<Google>>>,
                     Form(_): Form<HashMap<String, String>>| async move {
                        let mut claims = g.lock().unwrap().claims.clone();
                        claims["exp"] = json!(OffsetDateTime::now_utc().unix_timestamp() + 3600);
                        let tok = format!(
                            "{}.{}.unsigned",
                            B64.encode(br#"{"alg":"RS256"}"#),
                            B64.encode(claims.to_string())
                        );
                        axum::Json(json!({ "id_token": tok }))
                    },
                ),
            )
            .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, state)
}

struct GoogleEnv {
    dir: tempfile::TempDir,
    google: SocketAddrHolder,
    port: u16,
    base: String,
    server: Option<tokio::task::JoinHandle<()>>,
    http: reqwest::Client,
}

type SocketAddrHolder = std::net::SocketAddr;

impl GoogleEnv {
    async fn new(emails: &[&str]) -> (GoogleEnv, Arc<Mutex<Google>>) {
        let (google, state) = start_google().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        fsutil::ensure_private_dir(&dir.path().join("secrets")).unwrap();
        for (n, v) in [
            ("api_token", "api-token-web-0123456789abcdef"),
            ("mcp_token", "mcp-token-web-0123456789abcdef"),
            ("google_secret", "google-secret-web-0123456789"),
        ] {
            fsutil::write_new_private_file(&dir.path().join("secrets").join(n), v.as_bytes())
                .unwrap();
        }
        let mut env = GoogleEnv {
            dir,
            google,
            port,
            base: format!("http://127.0.0.1:{port}"),
            server: None,
            http: client(),
        };
        env.start(emails, listener);
        (env, state)
    }

    fn start(&mut self, emails: &[&str], listener: tokio::net::TcpListener) {
        let g = self.google;
        let text = format!(
            "[server]\napi_token_file = \"secrets/api_token\"\n\
             [tvmaze]\nenabled = false\n\
             [mcp]\nenabled = true\ntoken_file = \"secrets/mcp_token\"\nallowed_hosts = [\"127.0.0.1\"]\n\
             [mcp.oauth]\npublic_url = '{}'\ngoogle_client_id = '{GOOGLE_ID}'\n\
             google_client_secret_file = \"secrets/google_secret\"\nallowed_emails = {}\n\
             trusted_client_metadata = [\"http://{g}/oauth/\"]\n\
             google_authorization_endpoint = \"http://{g}/google/auth\"\n\
             google_token_endpoint = \"http://{g}/google/token\"\n",
            self.base,
            serde_json::to_string(emails).unwrap()
        );
        fsutil::atomic_replace_private(&self.dir.path().join("config.toml"), text.as_bytes())
            .unwrap();
        let cfg = config::load(&self.dir.path().join("config.toml")).unwrap();
        let state = Arc::new(api::AppState::new(cfg).unwrap());
        let (app, _stop) = api::app(state).unwrap();
        self.server = Some(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
    }

    async fn restart(&mut self, emails: &[&str]) {
        let h = self.server.take().unwrap();
        h.abort();
        let _ = h.await;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", self.port))
            .await
            .unwrap();
        self.http = client();
        self.start(emails, listener);
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// `/app/signin`, Google's answer, `/auth/callback`: the callback's reply.
    async fn sign_in(&self) -> reqwest::Response {
        let r = self.http.get(self.url("/app/signin")).send().await.unwrap();
        assert_eq!(r.status(), 302);
        let browser = r.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let google = reqwest::Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
        let q: HashMap<_, _> = google.query_pairs().into_owned().collect();
        assert_eq!(google.path(), "/google/auth");
        assert_eq!(q["client_id"], GOOGLE_ID);
        assert_eq!(q["redirect_uri"], self.url("/auth/callback"));
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(q["state"].len() >= 32 && q["code_challenge"].len() >= 32);
        self.http
            .get(self.url(&format!(
                "/auth/callback?code=google-code&state={}",
                q["state"]
            )))
            .header("cookie", browser)
            .send()
            .await
            .unwrap()
    }

    async fn me(&self, cookie: &str) -> (u16, Value) {
        let r = self
            .http
            .get(self.url("/app/api/me"))
            .header("x-arr-app", "1")
            .header("cookie", cookie)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn google_sign_in_gives_the_app_a_session_for_an_allowed_account() {
    let (mut env, google) = GoogleEnv::new(&[OWNER]).await;
    // Before signing in: unauthorised, and the app is told Google is available.
    let r = env
        .http
        .get(env.url("/app/api/me"))
        .header("x-arr-app", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(r.json::<Value>().await.unwrap()["signin"]["google"], true);
    // The page and assets are served here too.
    assert_eq!(
        env.http.get(env.url("/")).send().await.unwrap().status(),
        200
    );

    let r = env.sign_in().await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/");
    let set = r.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(
        set.starts_with("arr_session=") && set.contains("HttpOnly") && set.contains("SameSite=Lax"),
        "{set}"
    );
    let cookie = set.split(';').next().unwrap().to_string();
    let (s, v) = env.me(&cookie).await;
    assert_eq!((s, v["email"].as_str()), (200, Some(OWNER)), "{v}");
    // With remote access, the app is told the public address (for "Use Arr on your phone").
    assert_eq!(
        v["public_url"].as_str(),
        Some(env.base.trim_end_matches('/')),
        "{v}"
    );
    // The proxy reaches the API with its token (the catalog is simply not imported here).
    let r = env
        .http
        .get(env.url("/app/api/v1/ideas"))
        .header("x-arr-app", "1")
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["status"], "disabled");

    // The session survives a restart, but only while the address is still allowed.
    env.restart(&[OWNER]).await;
    assert_eq!(env.me(&cookie).await.0, 200);
    env.restart(&["someone.else@example.com"]).await;
    assert_eq!(env.me(&cookie).await.0, 401);

    // An account that is not allowed gets no session.
    env.restart(&[OWNER]).await;
    google.lock().unwrap().claims["email"] = json!("stranger@example.com");
    let r = env.sign_in().await;
    assert_eq!(r.status(), 403);
    assert!(r.headers().get("set-cookie").is_none());
    // An unverified address is refused too.
    google.lock().unwrap().claims = json!({
        "iss": "https://accounts.google.com", "aud": GOOGLE_ID, "sub": "1",
        "email": OWNER, "email_verified": false,
    });
    assert_eq!(env.sign_in().await.status(), 403);
}

#[tokio::test]
async fn the_google_callback_only_completes_in_the_browser_that_started_it() {
    let (env, _google) = GoogleEnv::new(&[OWNER]).await;
    let r = env.http.get(env.url("/app/signin")).send().await.unwrap();
    let google = reqwest::Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    let state = google
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let r = env
        .http
        .get(env.url(&format!("/auth/callback?code=c&state={state}")))
        .header("cookie", "arr_signin=another-browser")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.headers().get("set-cookie").is_none());
    // The MCP sign-in is untouched: /authorize still insists on a valid request.
    let r = env.http.get(env.url("/authorize")).send().await.unwrap();
    assert_eq!(r.status(), 400);
    // Only the configured host may start a sign-in.
    let r = env
        .http
        .get(env.url("/app/signin"))
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

// ------------------------------------------------------------------ posters

impl Site {
    async fn art(&self, id: &str, cookie: Option<&str>) -> reqwest::Response {
        let rb = self.plain(Method::GET, &format!("/app/art/{}", id.replace(':', "%3A")));
        match cookie {
            Some(c) => rb.header("cookie", c),
            None => rb,
        }
        .send()
        .await
        .unwrap()
    }
}

fn image_fetches(e: &Exec) -> Vec<String> {
    e.mock.lock().unwrap().image_log.clone()
}

#[tokio::test]
async fn posters_are_fetched_once_and_served_from_the_cache() {
    let e = exec().await;
    let site = serve(e.state.clone()).await;
    // A session is required, and only real media ids are accepted.
    assert_eq!(site.art("movie:tmdb:900001", None).await.status(), 401);
    let cookie = site.sign_in(&e).await;
    assert_eq!(
        site.art("movie:imdb:tt1", Some(&cookie)).await.status(),
        400
    );
    assert_eq!(site.art("nonsense", Some(&cookie)).await.status(), 400);
    assert!(image_fetches(&e).is_empty());

    // A movie poster comes from the TMDB image host, then from disk.
    for _ in 0..2 {
        let r = site.art("movie:tmdb:900001", Some(&cookie)).await;
        assert_eq!(r.status(), 200);
        let h = r.headers();
        assert_eq!(h["content-type"], "image/jpeg");
        assert_eq!(h["cache-control"], "private, max-age=604800");
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert_eq!(&r.bytes().await.unwrap()[..3], &[0xff, 0xd8, 0xff]);
    }
    assert_eq!(image_fetches(&e), ["/t/p/w342/leviticus.jpg"]);

    // A show's poster, and its seasons and episodes, share one fetch (TVDB id 372264 is
    // TVmaze 9001; the show is also reachable by its TVmaze id).
    for id in [
        "series:tvmaze:9001",
        "series:tvdb:372264",
        "series:tvmaze:9001",
        "season:tvdb:372264:1",
        "episode:tvmaze:9001:1:2",
    ] {
        let r = site.art(id, Some(&cookie)).await;
        assert_eq!(r.status(), 200, "{id}");
        assert_eq!(r.headers()["content-type"], "image/jpeg");
    }
    let fetches = image_fetches(&e);
    assert_eq!(fetches.len(), 2, "{fetches:?}");
    assert_eq!(fetches[1], "/uploads/9001.jpg");
    assert!(e.state.cfg.state_dir.join("art/show-9001").exists());
    // A TVDB id the catalog doesn't know is looked up at TVmaze.
    assert_eq!(
        site.art("series:tvdb:999001", Some(&cookie)).await.status(),
        200
    );
    assert_eq!(image_fetches(&e).len(), 3);
}

#[tokio::test]
async fn a_missing_poster_is_a_quiet_404_and_is_remembered() {
    let e = exec().await;
    e.mock.lock().unwrap().tmdb_movies.insert(
        "900002".into(),
        tmdb_movie_with(900002, "No Poster", 2025, "2025-08-15"),
    );
    let site = serve(e.state.clone()).await;
    let cookie = site.sign_in(&e).await;
    for _ in 0..2 {
        let r = site.art("movie:tmdb:900002", Some(&cookie)).await;
        assert_eq!(r.status(), 404);
        assert!(r.bytes().await.unwrap().is_empty());
    }
    assert!(e.state.cfg.state_dir.join("art/movie-900002.none").exists());
    // A show TVmaze doesn't know, and one TVmaze has no picture for, are the same.
    assert_eq!(
        site.art("series:tvmaze:4242", Some(&cookie)).await.status(),
        404
    );
    e.mock
        .lock()
        .unwrap()
        .tvmaze_images
        .insert(9002, json!(null));
    assert_eq!(
        site.art("series:tvmaze:9002", Some(&cookie)).await.status(),
        404
    );
    assert!(e.state.cfg.state_dir.join("art/show-9002.none").exists());
    assert!(image_fetches(&e).is_empty());
}

#[tokio::test]
async fn posters_come_only_from_the_configured_image_hosts() {
    let e = exec().await;
    let port = e.mock_addr.port();
    // Same machine, different host name: not the configured image host.
    e.mock.lock().unwrap().tvmaze_images.insert(
        9002,
        json!({ "medium": format!("http://localhost:{port}/uploads/9002.jpg") }),
    );
    let site = serve(e.state.clone()).await;
    let cookie = site.sign_in(&e).await;
    assert_eq!(
        site.art("series:tvmaze:9002", Some(&cookie)).await.status(),
        404
    );
    // The configured host, with another address hidden in its path: joining that path must
    // not move the fetch to the other host.
    let mock = e.mock_addr;
    e.mock.lock().unwrap().tvmaze_images.insert(
        9003,
        json!({ "medium": format!("http://{mock}/http://localhost:{port}/uploads/9003.jpg") }),
    );
    assert_eq!(
        site.art("series:tvmaze:9003", Some(&cookie)).await.status(),
        404
    );
    assert!(image_fetches(&e).is_empty());
}

#[tokio::test]
async fn movie_posters_need_plex_or_tmdb_to_know_the_movie() {
    let e = start_exec(ExecOptions::default()).await;
    let site = serve(e.state.clone()).await;
    let cookie = site.sign_in(&e).await;
    assert_eq!(
        site.art("movie:tmdb:900001", Some(&cookie)).await.status(),
        404
    );
    assert!(image_fetches(&e).is_empty());
}

#[tokio::test]
async fn movie_posters_come_from_plex_on_its_image_host_only() {
    // No TMDB key at all.
    let e = start_exec(ExecOptions::default()).await;
    let site = serve(e.state.clone()).await;
    let cookie = site.sign_in(&e).await;
    let r = site.art("movie:tmdb:910001", Some(&cookie)).await;
    assert_eq!(r.status(), 200);
    assert_eq!(&r.bytes().await.unwrap()[..3], &[0xff, 0xd8, 0xff]);
    assert_eq!(image_fetches(&e), ["/uploads/rel0001.jpg"]);
    // A poster address on another host is never fetched.
    assert_eq!(
        site.art("movie:tmdb:910003", Some(&cookie)).await.status(),
        404
    );
    assert_eq!(image_fetches(&e).len(), 1);
}

#[tokio::test]
async fn a_movie_plex_has_no_poster_for_falls_back_to_tmdb() {
    let e = exec().await;
    let site = serve(e.state.clone()).await;
    let cookie = site.sign_in(&e).await;
    // Plex doesn't know this one; TMDB's poster is used.
    assert_eq!(
        site.art("movie:tmdb:900001", Some(&cookie)).await.status(),
        200
    );
    assert_eq!(image_fetches(&e), ["/t/p/w342/leviticus.jpg"]);
}
