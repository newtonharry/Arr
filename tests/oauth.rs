//! OAuth sign-in for the remote MCP endpoint, end to end on loopback: a mock serves the
//! connector's metadata documents and keys and plays Google's token endpoint.

use arr::{api, config, fsutil};
use axum::Router;
use axum::extract::{Form, Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use url::Url;

const GOOGLE_ID: &str = "test-google-client.apps.googleusercontent.com";
const GOOGLE_SECRET: &str = "google-secret-value-0123456789";
const MCP_TOKEN: &str = "mcp-token-oauth-0123456789abcdef";
const API_TOKEN: &str = "api-token-oauth-0123456789abcdef";
const EMAIL: &str = "owner@example.com";
const REDIRECT: &str = "https://connector.example/callback";
const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
const KEY_DER: &[u8] = include_bytes!("fixtures/oauth_test_rsa.pk8");

// ------------------------------------------------------------------ mock connector + Google

#[derive(Default)]
struct MockState {
    /// Served at `/oauth/...` (client documents, keys).
    docs: HashMap<String, Value>,
    claims: Value,
    google_forms: Vec<HashMap<String, String>>,
}

type Mock = Arc<Mutex<MockState>>;

fn keypair() -> RsaKeyPair {
    RsaKeyPair::from_pkcs8(KEY_DER).unwrap()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

async fn start_mock() -> (SocketAddr, Mock) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let pk = RsaPublicKeyComponents::<Vec<u8>>::from(keypair().public());
    let doc = |path: &str, id: &str, method: &str| {
        let mut d = json!({
            "client_id": format!("http://{addr}/oauth/{id}"),
            "client_name": "<b>Chat&GPT</b>",
            "redirect_uris": [REDIRECT, "https://connector.example/other"],
            "token_endpoint_auth_method": method,
            "grant_types": ["authorization_code", "refresh_token"],
            "jwks_uri": format!("http://{addr}/oauth/jwks.json"),
        });
        if id == "mismatch/client.json" {
            d["client_id"] = json!(format!("http://{addr}/oauth/other/client.json"));
        }
        (path.to_string(), d)
    };
    let mut state = MockState {
        claims: Value::Null,
        ..Default::default()
    };
    state.docs.extend([
        doc("client.json", "client.json", "private_key_jwt"),
        doc("mismatch/client.json", "mismatch/client.json", "none"),
        doc("public/client.json", "public/client.json", "none"),
    ]);
    let (path, mut d) = doc("nomethod/client.json", "nomethod/client.json", "none");
    d.as_object_mut()
        .unwrap()
        .remove("token_endpoint_auth_method");
    state.docs.insert(path, d);
    state.docs.insert(
        "jwks.json".into(),
        json!({ "keys": [
            { "kty": "EC", "kid": "ec", "crv": "P-256", "x": "AA", "y": "AA" },
            { "kty": "RSA", "kid": "k1", "use": "sig", "alg": "RS256",
              "n": B64.encode(&pk.n), "e": B64.encode(&pk.e) },
        ] }),
    );
    let mock: Mock = Arc::new(Mutex::new(state));
    let app = Router::new()
        .route(
            "/oauth/{*rest}",
            get(
                |State(m): State<Mock>, AxPath(rest): AxPath<String>| async move {
                    match m.lock().unwrap().docs.get(&rest) {
                        Some(v) => axum::Json(v.clone()).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            ),
        )
        .route(
            "/google/token",
            post(
                |State(m): State<Mock>, Form(f): Form<HashMap<String, String>>| async move {
                    let mut w = m.lock().unwrap();
                    w.google_forms.push(f);
                    let mut claims = w.claims.clone();
                    if claims.get("exp").is_none() {
                        claims["exp"] = json!(now() + 3600);
                    }
                    let tok = format!(
                        "{}.{}.unsigned",
                        B64.encode(br#"{"alg":"RS256"}"#),
                        B64.encode(claims.to_string())
                    );
                    axum::Json(json!({ "id_token": tok, "access_token": "google-at" }))
                },
            ),
        )
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, mock)
}

fn default_claims() -> Value {
    json!({
        "iss": "https://accounts.google.com", "aud": GOOGLE_ID, "sub": "1234567890",
        "email": EMAIL, "email_verified": true,
    })
}

// ------------------------------------------------------------------ the server under test

struct Env {
    dir: tempfile::TempDir,
    mock: Mock,
    mock_addr: SocketAddr,
    port: u16,
    base: String,
    server: Option<tokio::task::JoinHandle<()>>,
    http: reqwest::Client,
}

impl Env {
    async fn new(emails: &[&str]) -> Env {
        let (mock_addr, mock) = start_mock().await;
        mock.lock().unwrap().claims = default_claims();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        fsutil::ensure_private_dir(&dir.path().join("secrets")).unwrap();
        for (n, v) in [
            ("api_token", API_TOKEN),
            ("mcp_token", MCP_TOKEN),
            ("google_secret", GOOGLE_SECRET),
        ] {
            fsutil::write_new_private_file(&dir.path().join("secrets").join(n), v.as_bytes())
                .unwrap();
        }
        let mut env = Env {
            dir,
            mock,
            mock_addr,
            port,
            base: format!("http://127.0.0.1:{port}"),
            server: None,
            http: client(),
        };
        env.write_config(emails);
        env.start(listener);
        env
    }

    fn write_config(&self, emails: &[&str]) {
        let emails = serde_json::to_string(emails).unwrap();
        let m = self.mock_addr;
        let text = format!(
            "[server]\napi_token_file = \"secrets/api_token\"\n\
             [mcp]\nenabled = true\ntoken_file = \"secrets/mcp_token\"\nallowed_hosts = [\"127.0.0.1\"]\n\
             [mcp.oauth]\npublic_url = \"{}\"\ngoogle_client_id = \"{GOOGLE_ID}\"\n\
             google_client_secret_file = \"secrets/google_secret\"\nallowed_emails = {emails}\n\
             trusted_client_metadata = [\"http://{m}/oauth/\"]\n\
             google_authorization_endpoint = \"http://{m}/google/auth\"\n\
             google_token_endpoint = \"http://{m}/google/token\"\n",
            self.base
        );
        fsutil::atomic_replace_private(&self.dir.path().join("config.toml"), text.as_bytes())
            .unwrap();
    }

    fn start(&mut self, listener: tokio::net::TcpListener) {
        let cfg = config::load(&self.dir.path().join("config.toml")).unwrap();
        let state = Arc::new(api::AppState::new(cfg).unwrap());
        let (app, _stop) = api::app(state).unwrap();
        self.server = Some(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
    }

    /// Same state directory and port, new allow-list.
    async fn restart(&mut self, emails: &[&str]) {
        let h = self.server.take().unwrap();
        h.abort();
        let _ = h.await;
        self.write_config(emails);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", self.port))
            .await
            .unwrap();
        self.http = client();
        self.start(listener);
    }

    fn client_id(&self) -> String {
        format!("http://{}/oauth/client.json", self.mock_addr)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn authorize_query(&self, cid: &str, redirect: &str) -> Vec<(&'static str, String)> {
        vec![
            ("response_type", "code".into()),
            ("client_id", cid.into()),
            ("redirect_uri", redirect.into()),
            ("code_challenge", challenge(VERIFIER)),
            ("code_challenge_method", "S256".into()),
            ("state", "client-state-1".into()),
        ]
    }

    async fn authorize(&self, q: &[(&str, String)]) -> reqwest::Response {
        self.http
            .get(qs(self.url("/authorize"), q))
            .send()
            .await
            .unwrap()
    }

    /// Runs /authorize, consent and the Google round trip; returns the redirect back to the
    /// client.
    async fn sign_in(&self, cid: &str, redirect: &str) -> Url {
        let r = self.authorize(&self.authorize_query(cid, redirect)).await;
        assert_eq!(r.status(), 200);
        let page = r.text().await.unwrap();
        let txn = page
            .split("name=\"txn\" value=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_string();
        let r = self
            .http
            .post(self.url("/authorize/consent"))
            .header("origin", self.url(""))
            .form(&[("txn", txn.as_str()), ("decision", "approve")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 302);
        let cookie = Self::browser(&r);
        let google = Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
        let state = param(&google, "state");
        let r = self
            .http
            .get(qs(
                self.url("/auth/callback"),
                &[("code", "google-code"), ("state", state.as_str())],
            ))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 302);
        Url::parse(r.headers()["location"].to_str().unwrap()).unwrap()
    }

    /// A fresh authorization code for the main client.
    async fn code(&self) -> String {
        let back = self.sign_in(&self.client_id(), REDIRECT).await;
        param(&back, "code")
    }

    fn assertion(&self, tweak: impl FnOnce(&mut Value)) -> String {
        let mut claims = json!({
            "iss": self.client_id(), "sub": self.client_id(),
            "aud": self.url("/token"), "exp": now() + 300, "iat": now(),
            "jti": format!("jti-{}", Self::unique()),
        });
        tweak(&mut claims);
        sign(&claims, "k1")
    }

    /// The `Cookie` header carrying the sign-in binding set by a consent approval.
    fn browser(r: &reqwest::Response) -> String {
        let c = r.headers()["set-cookie"].to_str().unwrap();
        assert!(c.contains("HttpOnly") && c.contains("Secure") && c.contains("SameSite=Lax"));
        c.split(';').next().unwrap().to_string()
    }

    fn unique() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    /// POST /token with the main client's assertion plus `fields`.
    async fn token(&self, fields: &[(&str, &str)]) -> (u16, Value) {
        self.token_with(self.assertion(|_| {}), fields).await
    }

    async fn token_with(&self, assertion: String, fields: &[(&str, &str)]) -> (u16, Value) {
        let mut form: Vec<(&str, &str)> = fields.to_vec();
        form.push((
            "client_assertion_type",
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
        ));
        form.push(("client_assertion", &assertion));
        self.post_token(&form).await
    }

    async fn post_token(&self, form: &[(&str, &str)]) -> (u16, Value) {
        let r = self
            .http
            .post(self.url("/token"))
            .form(form)
            .send()
            .await
            .unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }

    /// Exchanges a fresh code for tokens.
    async fn tokens(&self) -> Value {
        let code = self.code().await;
        let (s, v) = self
            .token(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", VERIFIER),
            ])
            .await;
        assert_eq!(s, 200, "{v}");
        v
    }

    async fn mcp_status(&self, bearer: Option<&str>) -> (u16, Option<String>) {
        let mut rb = self
            .http
            .post(self.url("/mcp"))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string());
        if let Some(b) = bearer {
            rb = rb.bearer_auth(b);
        }
        let r = rb.send().await.unwrap();
        let www = r
            .headers()
            .get("www-authenticate")
            .map(|v| v.to_str().unwrap().to_string());
        (r.status().as_u16(), www)
    }
}

fn qs<K: AsRef<str>, V: AsRef<str>>(base: String, q: &[(K, V)]) -> String {
    Url::parse_with_params(&base, q).unwrap().to_string()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap()
}

fn param(u: &Url, k: &str) -> String {
    u.query_pairs()
        .find(|(n, _)| n == k)
        .unwrap_or_else(|| panic!("no {k} in {u}"))
        .1
        .into_owned()
}

fn has_param(u: &Url, k: &str) -> bool {
    u.query_pairs().any(|(n, _)| n == k)
}

fn challenge(verifier: &str) -> String {
    B64.encode(Sha256::digest(verifier.as_bytes()))
}

fn sign(claims: &Value, kid: &str) -> String {
    let head = B64.encode(json!({ "alg": "RS256", "typ": "JWT", "kid": kid }).to_string());
    let body = B64.encode(claims.to_string());
    let msg = format!("{head}.{body}");
    let mut sig = vec![0u8; keypair().public().modulus_len()];
    keypair()
        .sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            msg.as_bytes(),
            &mut sig,
        )
        .unwrap();
    format!("{msg}.{}", B64.encode(sig))
}

fn mcp_tokens(v: &Value) -> (String, String) {
    (
        v["access_token"].as_str().unwrap().to_string(),
        v["refresh_token"].as_str().unwrap().to_string(),
    )
}

async fn list_tools(env: &Env, token: &str) -> usize {
    use rmcp::ServiceExt;
    use rmcp::model::ClientConfig;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(env.url("/mcp"))
            .auth_header(token.to_string()),
    );
    let client = ClientConfig::default().serve(transport).await.unwrap();
    let n = client.list_all_tools().await.unwrap().len();
    let _ = client.cancel().await;
    n
}

// ------------------------------------------------------------------ tests

#[tokio::test]
async fn discovery_documents_describe_the_server_without_registration() {
    let env = Env::new(&[EMAIL]).await;
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let r = env.http.get(env.url(path)).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["cache-control"], "no-store");
        let v: Value = r.json().await.unwrap();
        assert_eq!(
            v,
            json!({ "resource": env.url("/mcp"), "authorization_servers": [env.url("/")],
                    "bearer_methods_supported": ["header"] })
        );
    }
    let v: Value = env
        .http
        .get(env.url("/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["issuer"], env.url("/"));
    assert_eq!(v["authorization_endpoint"], env.url("/authorize"));
    assert_eq!(v["token_endpoint"], env.url("/token"));
    assert_eq!(v["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(
        v["token_endpoint_auth_methods_supported"],
        json!(["private_key_jwt", "none"])
    );
    assert_eq!(v["client_id_metadata_document_supported"], true);
    assert!(v.get("registration_endpoint").is_none(), "{v}");
    // The Host allow-list applies to every endpoint.
    let r = env
        .http
        .get(env.url("/.well-known/oauth-authorization-server"))
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // POSTs from another origin are refused.
    let r = env
        .http
        .post(env.url("/token"))
        .header("origin", "https://evil.example")
        .form(&[("grant_type", "refresh_token")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn authorize_refuses_unknown_clients_and_redirects_only_to_registered_addresses() {
    let env = Env::new(&[EMAIL]).await;
    let m = env.mock_addr;
    let no_redirect = |r: &reqwest::Response| {
        assert_eq!(r.status(), 400);
        assert!(r.headers().get("location").is_none());
    };
    // Not under a trusted prefix.
    let r = env
        .authorize(&env.authorize_query(&format!("http://{m}/untrusted/client.json"), REDIRECT))
        .await;
    no_redirect(&r);
    // Trusted prefix but the pattern does not match.
    let r = env
        .authorize(&env.authorize_query(&format!("http://{m}/oauth/a/b/client.json"), REDIRECT))
        .await;
    no_redirect(&r);
    // The document names another client_id.
    let r = env
        .authorize(
            &env.authorize_query(&format!("http://{m}/oauth/mismatch/client.json"), REDIRECT),
        )
        .await;
    no_redirect(&r);
    // The document does not say how the client authenticates.
    let r = env
        .authorize(
            &env.authorize_query(&format!("http://{m}/oauth/nomethod/client.json"), REDIRECT),
        )
        .await;
    no_redirect(&r);
    // A redirect address the document does not list.
    let r = env
        .authorize(&env.authorize_query(&env.client_id(), "https://evil.example/cb"))
        .await;
    no_redirect(&r);
    // From here on errors go back to the client.
    let cases: Vec<(&str, Option<&str>)> = vec![
        ("response_type", Some("token")),
        ("code_challenge", None),
        ("code_challenge", Some("short")),
        ("code_challenge_method", Some("plain")),
        ("resource", Some("https://elsewhere.example/mcp")),
    ];
    for (k, v) in cases {
        let mut q = env.authorize_query(&env.client_id(), REDIRECT);
        q.retain(|(n, _)| *n != k);
        if let Some(v) = v {
            q.push((k, v.to_string()));
        }
        let r = env.authorize(&q).await;
        assert_eq!(r.status(), 302, "{k}");
        let u = Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
        assert!(u.as_str().starts_with(REDIRECT));
        assert_eq!(param(&u, "error"), "invalid_request", "{k}");
        assert_eq!(param(&u, "state"), "client-state-1");
    }
    // A matching resource is accepted.
    let mut q = env.authorize_query(&env.client_id(), REDIRECT);
    q.push(("resource", env.url("/mcp")));
    assert_eq!(env.authorize(&q).await.status(), 200);
}

#[tokio::test]
async fn sign_in_issues_tokens_that_survive_a_restart_and_refresh_rotates() {
    let mut env = Env::new(&[EMAIL]).await;
    let r = env
        .authorize(&env.authorize_query(&env.client_id(), REDIRECT))
        .await;
    assert_eq!(r.status(), 200);
    let h = r.headers().clone();
    assert!(
        h["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    // form-action would also govern the 302 after the consent POST (to Google or the client).
    assert!(
        !h["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("form-action")
    );
    assert_eq!(h["x-frame-options"], "DENY");
    assert_eq!(h["referrer-policy"], "same-origin");
    assert_eq!(h["cache-control"], "no-store");
    let page = r.text().await.unwrap();
    assert!(
        page.contains("&lt;b&gt;Chat&amp;GPT&lt;/b&gt;"),
        "escaped name: {page}"
    );
    assert!(page.contains("connector.example"));
    assert!(!page.contains("<b>Chat"));
    let txn = page
        .split("name=\"txn\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    assert!(txn.len() >= 43);

    // Approve: on to Google with the right parameters.
    let r = env
        .http
        .post(env.url("/authorize/consent"))
        .header("origin", env.url(""))
        .form(&[("txn", txn.as_str()), ("decision", "approve")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302);
    let cookie = Env::browser(&r);
    let google = Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(google.path(), "/google/auth");
    assert_eq!(param(&google, "client_id"), GOOGLE_ID);
    assert_eq!(param(&google, "redirect_uri"), env.url("/auth/callback"));
    assert_eq!(param(&google, "response_type"), "code");
    assert_eq!(param(&google, "scope"), "openid email");
    assert_eq!(param(&google, "code_challenge_method"), "S256");
    assert_eq!(param(&google, "prompt"), "select_account");
    // The transaction is single use.
    let again = env
        .http
        .post(env.url("/authorize/consent"))
        .header("origin", env.url(""))
        .form(&[("txn", txn.as_str()), ("decision", "approve")])
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 400);

    // Google's callback: a code for the client, with its state and the issuer.
    let state = param(&google, "state");
    let r = env
        .http
        .get(qs(
            env.url("/auth/callback"),
            &[("code", "google-code"), ("state", state.as_str())],
        ))
        .header("cookie", cookie.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302);
    let back = Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    assert!(back.as_str().starts_with(REDIRECT));
    assert_eq!(param(&back, "state"), "client-state-1");
    assert_eq!(param(&back, "iss"), env.url("/"));
    let code = param(&back, "code");
    {
        let w = env.mock.lock().unwrap();
        let f = &w.google_forms[0];
        assert_eq!(f["grant_type"], "authorization_code");
        assert_eq!(f["code"], "google-code");
        assert_eq!(f["client_secret"], GOOGLE_SECRET);
        assert_eq!(f["redirect_uri"], env.url("/auth/callback"));
        assert_eq!(
            challenge(&f["code_verifier"]),
            param(&google, "code_challenge")
        );
    }
    // The callback state is single use too.
    let r = env
        .http
        .get(qs(
            env.url("/auth/callback"),
            &[("code", "google-code"), ("state", state.as_str())],
        ))
        .header("cookie", cookie.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // The token request.
    let (s, v) = env
        .token(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
        ])
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["token_type"], "Bearer");
    assert_eq!(v["expires_in"], 3600);
    let (at, rt) = mcp_tokens(&v);
    assert!(at.starts_with("arr_at_") && rt.starts_with("arr_rt_"));
    assert_eq!(list_tools(&env, &at).await, 6);

    // Refresh rotates: the old refresh token is dead, the new pair works.
    let (s, v) = env
        .token(&[("grant_type", "refresh_token"), ("refresh_token", &rt)])
        .await;
    assert_eq!(s, 200, "{v}");
    let (at2, rt2) = mcp_tokens(&v);
    assert_ne!(rt, rt2);
    let (s, v) = env
        .token(&[("grant_type", "refresh_token"), ("refresh_token", &rt)])
        .await;
    assert_eq!((s, v["error"].as_str()), (400, Some("invalid_grant")));
    assert_eq!(list_tools(&env, &at2).await, 6);

    // Tokens survive a restart, and only their hashes are stored.
    env.restart(&[EMAIL]).await;
    assert_eq!(list_tools(&env, &at2).await, 6);
    let raw = std::fs::read(env.dir.path().join("oauth.sqlite")).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains(&at2));
    let (s, v) = env
        .token(&[("grant_type", "refresh_token"), ("refresh_token", &rt2)])
        .await;
    assert_eq!(s, 200, "{v}");

    // Removing the address from the allow-list ends access and refresh.
    let (at3, rt3) = mcp_tokens(&v);
    env.restart(&["someone.else@example.com"]).await;
    assert_eq!(env.mcp_status(Some(&at3)).await.0, 401);
    let (s, v) = env
        .token(&[("grant_type", "refresh_token"), ("refresh_token", &rt3)])
        .await;
    assert_eq!((s, v["error"].as_str()), (400, Some("invalid_grant")));
}

#[tokio::test]
async fn identities_that_are_not_allowed_never_get_a_code() {
    let env = Env::new(&[EMAIL]).await;
    let cases: Vec<(&str, Value)> = vec![
        ("email", json!("stranger@example.com")),
        ("email_verified", json!(false)),
        ("email_verified", json!("false")),
        ("aud", json!("another-client")),
        ("aud", json!([GOOGLE_ID, "another"])),
        ("azp", json!("another-client")),
        ("iss", json!("https://evil.example")),
        ("exp", json!(1)),
    ];
    for (k, v) in cases {
        env.mock.lock().unwrap().claims = default_claims();
        env.mock.lock().unwrap().claims[k] = v.clone();
        let back = env.sign_in(&env.client_id(), REDIRECT).await;
        assert_eq!(param(&back, "error"), "access_denied", "{k}={v}");
        assert!(!has_param(&back, "code"), "{k}={v}");
        assert_eq!(param(&back, "state"), "client-state-1");
    }
    // Case and the string form of email_verified are accepted; so is a matching azp.
    let mut c = default_claims();
    c["email"] = json!("Owner@Example.COM");
    c["email_verified"] = json!("true");
    c["aud"] = json!([GOOGLE_ID, "another"]);
    c["azp"] = json!(GOOGLE_ID);
    env.mock.lock().unwrap().claims = c;
    assert!(has_param(
        &env.sign_in(&env.client_id(), REDIRECT).await,
        "code"
    ));
    // Google reporting an error (the user cancelled) is access_denied.
    let r = env
        .authorize(&env.authorize_query(&env.client_id(), REDIRECT))
        .await;
    let page = r.text().await.unwrap();
    let txn = page
        .split("name=\"txn\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let r = env
        .http
        .post(env.url("/authorize/consent"))
        .header("origin", env.url(""))
        .form(&[("txn", txn), ("decision", "approve")])
        .send()
        .await
        .unwrap();
    let cookie = Env::browser(&r);
    let google = Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    let r = env
        .http
        .get(qs(
            env.url("/auth/callback"),
            &[
                ("error", "access_denied"),
                ("state", &param(&google, "state")),
            ],
        ))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    let back = Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    assert_eq!(param(&back, "error"), "access_denied");
}

#[tokio::test]
async fn denying_consent_returns_access_denied() {
    let env = Env::new(&[EMAIL]).await;
    let r = env
        .authorize(&env.authorize_query(&env.client_id(), REDIRECT))
        .await;
    let page = r.text().await.unwrap();
    let txn = page
        .split("name=\"txn\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let r = env
        .http
        .post(env.url("/authorize/consent"))
        .header("origin", env.url(""))
        .form(&[("txn", txn), ("decision", "deny")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302);
    let u = Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
    assert!(u.as_str().starts_with(REDIRECT));
    assert_eq!(param(&u, "error"), "access_denied");
    assert_eq!(param(&u, "state"), "client-state-1");
    assert!(env.mock.lock().unwrap().google_forms.is_empty());
}

#[tokio::test]
async fn token_endpoint_binds_codes_to_client_redirect_and_verifier() {
    let env = Env::new(&[EMAIL]).await;
    let exchange = |code: String, redirect: &'static str, verifier: &'static str| {
        let env = &env;
        async move {
            env.token(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", redirect),
                ("code_verifier", verifier),
            ])
            .await
        }
    };
    let invalid_grant = |(s, v): (u16, Value)| (s, v["error"].as_str().map(str::to_string));
    let expected = (400, Some("invalid_grant".to_string()));
    // Wrong verifier (still a well-formed one).
    let code = env.code().await;
    let other = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopq";
    assert_eq!(
        invalid_grant(exchange(code.clone(), REDIRECT, other).await),
        expected
    );
    // The failed attempt consumed the code.
    assert_eq!(
        invalid_grant(exchange(code, REDIRECT, VERIFIER).await),
        expected
    );
    // Reuse.
    let code = env.code().await;
    assert_eq!(exchange(code.clone(), REDIRECT, VERIFIER).await.0, 200);
    assert_eq!(
        invalid_grant(exchange(code, REDIRECT, VERIFIER).await),
        expected
    );
    // Another redirect address.
    let code = env.code().await;
    assert_eq!(
        invalid_grant(exchange(code, "https://connector.example/other", VERIFIER).await),
        expected
    );
    // A malformed verifier and an unknown grant type are request errors.
    let code = env.code().await;
    let (s, v) = exchange(code, REDIRECT, "short").await;
    assert_eq!((s, v["error"].as_str()), (400, Some("invalid_request")));
    let (s, v) = env.token(&[("grant_type", "password")]).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("unsupported_grant_type"))
    );
    // Nothing secret comes back in an error.
    let (_, v) = env
        .token(&[
            ("grant_type", "authorization_code"),
            ("code", "nope-code"),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
        ])
        .await;
    assert!(!v.to_string().contains("nope-code"));
}

#[tokio::test]
async fn client_authentication_requires_a_valid_signed_assertion() {
    let env = Env::new(&[EMAIL]).await;
    let grant = |code: &str| -> Vec<(String, String)> {
        vec![
            ("grant_type".into(), "authorization_code".into()),
            ("code".into(), code.into()),
            ("redirect_uri".into(), REDIRECT.into()),
            ("code_verifier".into(), VERIFIER.into()),
        ]
    };
    let attempt = |assertion: String| {
        let env = &env;
        async move {
            let code = env.code().await;
            let g = grant(&code);
            let f: Vec<(&str, &str)> = g.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            let (s, v) = env.token_with(assertion, &f).await;
            (s, v["error"].as_str().map(str::to_string))
        }
    };
    let refused = (401, Some("invalid_client".to_string()));
    // Bad signature: a valid token with its signature replaced.
    let good = env.assertion(|_| {});
    let mut parts: Vec<&str> = good.split('.').collect();
    let forged_sig = B64.encode([7u8; 256]);
    parts[2] = &forged_sig;
    assert_eq!(attempt(parts.join(".")).await, refused);
    // Claims changed after signing.
    let tampered = {
        let p: Vec<&str> = good.split('.').collect();
        let mut c: Value = serde_json::from_slice(&B64.decode(p[1]).unwrap()).unwrap();
        c["jti"] = json!("another");
        format!("{}.{}.{}", p[0], B64.encode(c.to_string()), p[2])
    };
    assert_eq!(attempt(tampered).await, refused);
    // Wrong audience, expired, too far ahead, missing jti, wrong subject, not-yet-valid.
    assert_eq!(
        attempt(env.assertion(|c| c["aud"] = json!("https://other.example/token"))).await,
        refused
    );
    assert_eq!(
        attempt(env.assertion(|c| c["exp"] = json!(now() - 10))).await,
        refused
    );
    assert_eq!(
        attempt(env.assertion(|c| c["exp"] = json!(now() + 7200))).await,
        refused
    );
    assert_eq!(
        attempt(env.assertion(|c| {
            c.as_object_mut().unwrap().remove("jti");
        }))
        .await,
        refused
    );
    assert_eq!(
        attempt(env.assertion(|c| c["sub"] = json!("someone"))).await,
        refused
    );
    assert_eq!(
        attempt(env.assertion(|c| c["nbf"] = json!(now() + 600))).await,
        refused
    );
    // Unknown key id: refetched once, still unknown.
    assert_eq!(
        attempt(sign(&json!({ "iss": env.client_id(), "sub": env.client_id(), "aud": env.url("/token"), "exp": now() + 60, "jti": "x1" }), "nope")).await,
        refused
    );
    // Audience variants and an array are accepted; a replay is not.
    for aud in [
        json!(env.url("/")),
        json!(env.base.clone()),
        json!(["x", env.url("/token")]),
    ] {
        assert_eq!(attempt(env.assertion(|c| c["aud"] = aud)).await.0, 200);
    }
    let replayed = env.assertion(|c| c["jti"] = json!("fixed-jti"));
    assert_eq!(attempt(replayed.clone()).await.0, 200);
    assert_eq!(attempt(replayed).await, refused);
    // This client must authenticate: no assertion, and no client_secret either.
    let code = env.code().await;
    let cid = env.client_id();
    let (s, v) = env
        .post_token(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
            ("client_id", &cid),
        ])
        .await;
    assert_eq!((s, v["error"].as_str()), (401, Some("invalid_client")));
    let (s, v) = env
        .post_token(&[
            ("grant_type", "refresh_token"),
            ("client_id", &cid),
            ("client_secret", "x"),
        ])
        .await;
    assert_eq!((s, v["error"].as_str()), (401, Some("invalid_client")));
}

#[tokio::test]
async fn public_clients_may_skip_the_assertion_only_if_their_metadata_says_none() {
    let env = Env::new(&[EMAIL]).await;
    let cid = format!("http://{}/oauth/public/client.json", env.mock_addr);
    let back = env.sign_in(&cid, REDIRECT).await;
    let code = param(&back, "code");
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", REDIRECT),
        ("code_verifier", VERIFIER),
        ("client_id", cid.as_str()),
    ];
    let (s, v) = env.post_token(&form).await;
    assert_eq!(s, 200, "{v}");
    // A code issued to one client is useless to another.
    let code = env.code().await;
    let (s, v) = env
        .post_token(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
            ("client_id", &cid),
        ])
        .await;
    assert_eq!((s, v["error"].as_str()), (400, Some("invalid_grant")));
}

#[tokio::test]
async fn mcp_accepts_the_static_token_or_a_live_access_token_and_challenges_otherwise() {
    let env = Env::new(&[EMAIL]).await;
    let challenge = format!(
        "Bearer resource_metadata=\"{}\"",
        env.url("/.well-known/oauth-protected-resource/mcp")
    );
    for bearer in [None, Some("arr_at_not-a-real-token"), Some("garbage")] {
        let (s, www) = env.mcp_status(bearer).await;
        assert_eq!(s, 401);
        assert_eq!(www.as_deref(), Some(challenge.as_str()));
    }
    assert_ne!(
        env.mcp_status(Some(MCP_TOKEN)).await.0,
        401,
        "the static token still works"
    );
    let v = env.tokens().await;
    let (at, rt) = mcp_tokens(&v);
    assert_ne!(env.mcp_status(Some(&at)).await.0, 401);
    // A refresh token is not an access token.
    assert_eq!(env.mcp_status(Some(&rt)).await.0, 401);
    // The REST API accepts neither the MCP token nor an OAuth token.
    for t in [&at, MCP_TOKEN] {
        let r = env
            .http
            .get(env.url("/api/v1/readiness"))
            .bearer_auth(t)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
    }
    let r = env
        .http
        .get(env.url("/api/v1/readiness"))
        .bearer_auth(API_TOKEN)
        .send()
        .await
        .unwrap();
    assert_ne!(r.status(), 401);
    assert_eq!(env.mcp_status(Some(API_TOKEN)).await.0, 401);
    // Expired tokens are refused.
    let db = rusqlite::Connection::open(env.dir.path().join("oauth.sqlite")).unwrap();
    db.execute("UPDATE tokens SET expires_at = 1", []).unwrap();
    assert_eq!(env.mcp_status(Some(&at)).await.0, 401);
    let (s, v) = env
        .token(&[("grant_type", "refresh_token"), ("refresh_token", &rt)])
        .await;
    assert_eq!((s, v["error"].as_str()), (400, Some("invalid_grant")));
}

#[tokio::test]
async fn a_forwarded_google_link_cannot_complete_someone_elses_sign_in() {
    let env = Env::new(&[EMAIL]).await;
    for cookie in [None, Some("arr_signin=not-the-approving-browser")] {
        // The attacker approves a sign-in in their own browser...
        let r = env
            .authorize(&env.authorize_query(&env.client_id(), REDIRECT))
            .await;
        let page = r.text().await.unwrap();
        let txn = page
            .split("name=\"txn\" value=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let r = env
            .http
            .post(env.url("/authorize/consent"))
            .header("origin", env.url(""))
            .form(&[("txn", txn), ("decision", "approve")])
            .send()
            .await
            .unwrap();
        let google = Url::parse(r.headers()["location"].to_str().unwrap()).unwrap();
        // ...and the victim's browser completes Google's step without that cookie.
        let mut req = env.http.get(qs(
            env.url("/auth/callback"),
            &[("code", "google-code"), ("state", &param(&google, "state"))],
        ));
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 400, "{cookie:?}");
        assert!(r.headers().get("location").is_none());
        assert!(
            env.mock.lock().unwrap().google_forms.is_empty(),
            "no code exchange"
        );
    }
}
