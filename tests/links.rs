//! Release link resolution. Prowlarr's search API may put its HTTP download proxy in
//! `magnetUrl` with no `downloadUrl` (seen in practice: every grab was refused as "not a
//! magnet URI"). Links are classified by content, fetched only through the configured
//! origin's proxy paths, and an unusable field never hides a usable one.
#![recursion_limit = "256"]

mod common;

use arr::downloads::{self, EnqueueError, EnqueueRequest, LinkKind, ReleaseLink};
use arr::ids::MediaId;
use arr::prowlarr::ProwlarrRelease;
use arr::quality::Quality;
use arr::release::Revision;
use common::*;
use serde_json::{Value, json};

const MATRIX: &str = "The.Matrix.1999.1080p.WEB-DL.x264-PROXY";
const MATRIX_ID: MediaId = MediaId::Movie { tmdb: 603 };

/// A published Matrix release: info-hash, search-result JSON (proxy `downloadUrl`, literal
/// `magnetUrl`), and the proxy URL with a placeholder link key.
fn matrix(e: &Exec) -> (String, Value) {
    e.publish(
        MATRIX,
        MATRIX,
        vec![(format!("{MATRIX}.mkv"), mkv(2))],
        0.05,
        true,
    )
}

/// The configured Prowlarr's download proxy for `link`, shaped like Prowlarr's `magnetUrl`.
fn proxy(e: &Exec, link: &str) -> String {
    format!(
        "http://{}/prowlarr/1/download?apikey={PROWLARR_KEY}&link={link}&file=Example",
        e.mock_addr
    )
}

fn link(hash: &str, magnet: Option<String>, download: Option<String>) -> ReleaseLink {
    ReleaseLink {
        release_ref: format!("guid:1:fixture-{hash}"),
        target: MATRIX_ID,
        media_id: MATRIX_ID,
        info_hash: Some(hash.to_string()),
        magnet,
        download_url: download,
        title: MATRIX.into(),
        size: Some(50 * 1024 * 1024),
        indexer_id: Some(1),
        quality: Quality::Webdl1080,
        revision: Revision {
            version: 1,
            real: 0,
        },
        full_season: false,
        covers: vec![MATRIX_ID],
        replaces: vec![],
        interactive: true,
    }
}

async fn enqueue(e: &Exec, l: ReleaseLink) -> Result<String, (String, String)> {
    let req = EnqueueRequest {
        link: l,
        plan_id: None,
        origin: "interactive",
    };
    match downloads::enqueue(&e.state, req).await {
        Ok(g) => Ok(g.id),
        Err(EnqueueError::Refused(code, msg)) => Err((code.to_string(), msg)),
        Err(EnqueueError::Upstream(u)) => Err(("upstream".into(), u.describe())),
        Err(EnqueueError::Internal(err)) => panic!("{err:#}"),
    }
}

fn fetched(e: &Exec) -> Vec<String> {
    e.mock.lock().unwrap().downloads.clone()
}

/// The last add: `Some(magnet)` for a magnet add, `None` for a `.torrent` file.
fn last_add_magnet(e: &Exec) -> Option<String> {
    e.qb.lock()
        .unwrap()
        .add_fields
        .last()
        .expect("an add")
        .get("urls")
        .cloned()
}

#[tokio::test]
async fn http_magnet_url_proxy_is_fetched_queued_submitted_and_imported() {
    let e = start_exec(ExecOptions::default()).await;
    let (hash, mut r) = matrix(&e);
    r["magnetUrl"] = json!(proxy(&e, &hash));
    r["downloadUrl"] = Value::Null;
    // What the search API deserialises: a "magnet" field that is an HTTP proxy link.
    let parsed: ProwlarrRelease = serde_json::from_value(r.clone()).unwrap();
    assert!(parsed.download_url.is_none());
    assert_eq!(
        downloads::link_kind(parsed.magnet_url.as_ref().unwrap().expose()),
        LinkKind::Http
    );
    e.search_results("The Matrix 1999", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_no_leaks(&text);
    let ex = &v["plan"]["execution"];
    assert_eq!(
        (v["plan"]["state"].as_str(), ex["outcome"].as_str()),
        (Some("download_queued"), Some("queued")),
        "{text}"
    );
    assert_eq!(ex["refused"], json!([]));
    let id = ex["grabs"][0].as_str().unwrap().to_string();
    // Fetched once through the configured proxy: the `.torrent` itself, no fabricated magnet.
    assert_eq!(fetched(&e), vec![hash.clone()]);
    e.tick().await;
    {
        let q = e.qb.lock().unwrap();
        let t = q.torrents.get(&hash).expect("submitted to qBittorrent");
        assert_eq!(
            (t.category.as_str(), t.tags.as_str()),
            ("radarr", downloads::TAG)
        );
    }
    assert_eq!(last_add_magnet(&e), None, "added as a .torrent file");
    let g = e.grab_row(&id);
    assert_eq!(g.info_hash, hash);
    assert!(g.submitted_at.is_some(), "{g:?}");
    qb::complete(&e.qb, &hash);
    e.tick().await;
    assert!(e.state.index().movie(603).unwrap().has_file);
    assert_eq!(e.grab_row(&id).state, "imported");
}

#[tokio::test]
async fn proxy_redirects_resolve_to_a_validated_magnet_or_a_same_origin_torrent() {
    // Magnet-only indexer: the proxy answers 302 to the magnet.
    let e = start_exec(ExecOptions::default()).await;
    let (hash, mut r) = matrix(&e);
    let magnet = r["magnetUrl"].as_str().unwrap().to_string();
    e.mock
        .lock()
        .unwrap()
        .redirects
        .insert("to-magnet".into(), magnet.clone());
    r["magnetUrl"] = json!(proxy(&e, "to-magnet"));
    r["downloadUrl"] = Value::Null;
    e.search_results("The Matrix 1999", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_eq!(v["plan"]["execution"]["outcome"], "queued", "{text}");
    assert_eq!(fetched(&e), ["to-magnet"]);
    e.tick().await;
    assert_eq!(last_add_magnet(&e).as_deref(), Some(magnet.as_str()));
    assert!(e.qb.lock().unwrap().torrents.contains_key(&hash));

    // One same-origin hop to the torrent (relative `Location`).
    let e = start_exec(ExecOptions::default()).await;
    let (hash, mut r) = matrix(&e);
    e.mock.lock().unwrap().redirects.insert(
        "hop".into(),
        format!("/prowlarr/1/download?apikey={PROWLARR_KEY}&link={hash}"),
    );
    r["magnetUrl"] = json!(proxy(&e, "hop"));
    r["downloadUrl"] = Value::Null;
    e.search_results("The Matrix 1999", vec![r]);
    let (_, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    assert_eq!(v["plan"]["execution"]["outcome"], "queued", "{text}");
    assert_eq!(fetched(&e), ["hop".to_string(), hash.clone()]);
    e.tick().await;
    assert_eq!(last_add_magnet(&e), None);
    assert!(e.qb.lock().unwrap().torrents.contains_key(&hash));
}

#[tokio::test]
async fn an_unusable_link_field_never_hides_a_usable_one() {
    for case in 0..5 {
        let e = start_exec(ExecOptions::default()).await;
        let (hash, r) = matrix(&e);
        let magnet = r["magnetUrl"].as_str().unwrap().to_string();
        let good = proxy(&e, &hash);
        let foreign = format!(
            "http://tracker.invalid/1/download?apikey={PROWLARR_KEY}&passkey={FAKE_PASSKEY}"
        );
        let (m, d, want_magnet) = match case {
            // An invalid magnet, another scheme or a foreign URL in `magnetUrl`.
            0 => ("magnet:?xt=urn:btih:nothex&dn=x".to_string(), good, false),
            1 => (
                format!("ftp://prowlarr/1/download?apikey={PROWLARR_KEY}"),
                good,
                false,
            ),
            2 => (foreign, good, false),
            // A foreign `downloadUrl` (tried first, refused without a request), then the
            // proxy in `magnetUrl`.
            3 => (good, foreign, false),
            // A literal magnet in `downloadUrl` is used without any fetch.
            _ => (good, magnet.clone(), true),
        };
        let id = enqueue(&e, link(&hash, Some(m), Some(d)))
            .await
            .unwrap_or_else(|err| panic!("case {case}: {err:?}"));
        assert_eq!(
            fetched(&e),
            if want_magnet {
                vec![]
            } else {
                vec![hash.clone()]
            },
            "case {case}"
        );
        e.tick().await;
        assert!(
            e.qb.lock().unwrap().torrents.contains_key(&hash),
            "case {case}"
        );
        assert_eq!(
            last_add_magnet(&e),
            want_magnet.then(|| magnet.clone()),
            "case {case}"
        );
        assert_eq!(e.grab_row(&id).info_hash, hash);
    }
}

#[tokio::test]
async fn links_that_cannot_be_resolved_fail_closed_without_leaking() {
    let e = start_exec(ExecOptions::default()).await;
    let (hash, _) = matrix(&e);
    let base = format!("http://{}/prowlarr", e.mock_addr);
    {
        let mut m = e.mock.lock().unwrap();
        m.redirects.insert(
            "off-origin".into(),
            format!("http://tracker.invalid/get.torrent?passkey={FAKE_PASSKEY}"),
        );
        m.redirects
            .insert("bad-magnet".into(), "magnet:?xt=urn:btih:nothex".into());
        m.redirects.insert(
            "loop".into(),
            format!("/prowlarr/1/download?apikey={PROWLARR_KEY}&link=loop2"),
        );
        m.redirects.insert(
            "loop2".into(),
            format!("/prowlarr/1/download?apikey={PROWLARR_KEY}&link={hash}"),
        );
        m.torrents
            .insert("junk".into(), b"<html>not a torrent</html>".to_vec());
    }
    let p = |link: &str| Some(proxy(&e, link));
    let cases: Vec<(Option<String>, Option<String>, &str, &str)> = vec![
        (None, None, "no_download_link", "no download link"),
        (
            Some("magnet:?xt=urn:btih:zz".into()),
            None,
            "invalid_payload",
            "magnetUrl: not a valid magnet URI",
        ),
        (
            Some(format!("ftp://prowlarr/1/download?apikey={PROWLARR_KEY}")),
            None,
            "invalid_payload",
            "magnetUrl: unsupported link scheme",
        ),
        (
            Some("javascript:alert(1)".into()),
            Some("not a url".into()),
            "invalid_payload",
            "downloadUrl: not a URL",
        ),
        (
            Some(format!(
                "http://tracker.invalid/prowlarr/1/download?apikey={PROWLARR_KEY}&link={hash}"
            )),
            None,
            "upstream",
            "not on the configured Prowlarr origin",
        ),
        (
            Some(format!(
                "{base}/api/v1/search?query=x&apikey={PROWLARR_KEY}"
            )),
            None,
            "upstream",
            "not a Prowlarr download proxy path",
        ),
        (
            Some(format!("{}#frag", proxy(&e, &hash))),
            None,
            "upstream",
            "not a Prowlarr download proxy path",
        ),
        (
            Some(proxy(&e, &hash).replace("http://", "http://user:pw@")),
            None,
            "upstream",
            "carries credentials",
        ),
        (
            p("off-origin"),
            None,
            "upstream",
            "not on the configured Prowlarr origin",
        ),
        (
            p("bad-magnet"),
            None,
            "upstream",
            "redirect to an invalid magnet",
        ),
        (p("loop"), None, "upstream", "too many redirects"),
        (p("junk"), None, "upstream", "not a valid .torrent"),
        (p("unknown"), None, "upstream", "not found"),
        // Both fields unusable: the fetch error is reported, nothing is guessed.
        (
            Some("magnet:?xt=urn:btih:zz".into()),
            p("junk"),
            "upstream",
            "not a valid .torrent",
        ),
    ];
    for (i, (m, d, code, says)) in cases.into_iter().enumerate() {
        let err = enqueue(&e, link(&hash, m, d)).await.unwrap_err();
        assert_eq!(err.0, code, "case {i}: {err:?}");
        assert!(err.1.contains(says), "case {i}: {err:?}");
        for secret in [
            PROWLARR_KEY,
            FAKE_PASSKEY,
            "apikey",
            "tracker.invalid",
            "http",
            "magnet:?",
            "link=",
        ] {
            assert!(
                !err.1.contains(secret),
                "case {i} leaked {secret:?}: {err:?}"
            );
        }
    }
    // The advertised info-hash is still checked against the fetched payload.
    let mut wrong = link(&hash, p(&hash), None);
    wrong.info_hash = Some("0123456789abcdef0123456789abcdef01234567".into());
    let err = enqueue(&e, wrong).await.unwrap_err();
    assert_eq!(err.0, "invalid_payload", "{err:?}");
    assert!(err.1.contains("info-hash does not match"), "{err:?}");
    // Nothing entered the outbox or reached qBittorrent.
    let (_, q, text) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"], json!([]), "{text}");
    e.tick().await;
    assert!(e.qb.lock().unwrap().torrents.is_empty());
}

#[tokio::test]
async fn an_execution_that_queues_nothing_says_so_and_keeps_the_request() {
    let e = start_exec(ExecOptions::default()).await;
    let (_, mut r) = matrix(&e);
    r["magnetUrl"] = json!(format!(
        "http://tracker.invalid/1/download?apikey={PROWLARR_KEY}&passkey={FAKE_PASSKEY}"
    ));
    r["downloadUrl"] = Value::Null;
    e.search_results("The Matrix 1999", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "movie:tmdb:603" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_no_leaks(&text);
    let plan = &v["plan"];
    let ex = &plan["execution"];
    assert_eq!(
        (
            plan["state"].as_str(),
            ex["mode"].as_str(),
            ex["outcome"].as_str(),
            ex["submitted"].as_bool()
        ),
        (
            Some("shadow_planned"),
            Some("executed"),
            Some("nothing_queued"),
            Some(false)
        ),
        "{text}"
    );
    assert_eq!(ex["grabs"], json!([]));
    assert_eq!(ex["refused"][0]["code"], "payload_fetch_failed", "{text}");
    // The explicit request is not "satisfied": it stays active and is retried with backoff.
    let (_, q, text) = e.json(e.get("/api/v1/requests")).await;
    let req = &q["items"][0];
    assert_eq!(
        (req["target"].as_str(), req["state"].as_str()),
        (Some("movie:tmdb:603"), Some("active")),
        "{text}"
    );
    assert!(
        req["last_result"]
            .as_str()
            .unwrap()
            .contains("nothing queued (payload_fetch_failed)"),
        "{text}"
    );
    assert!(fetched(&e).is_empty(), "a foreign link is never requested");
}

fn requests(e: &Exec) -> Vec<Value> {
    let rows = e
        .state
        .store
        .blocking(|c| arr::worker::list_requests(c, false))
        .unwrap();
    rows.into_iter()
        .filter(|r| r["target"] == "season:tvdb:999005:1")
        .collect()
}

#[tokio::test]
async fn a_partially_refused_execution_keeps_the_request_until_the_missing_unit_is_queued() {
    let e = start_exec(ExecOptions::default()).await;
    let ep = |n: u8| format!("Parity.Partial.S01E0{n}.1080p.WEB-DL.x264-NEW");
    let (e1_hash, e1) = e.publish(
        &ep(1),
        &ep(1),
        vec![(format!("{}.mkv", ep(1)), mkv(2))],
        0.05,
        false,
    );
    // E02's only approved candidate has an unusable link: refused at enqueue.
    let (_, mut bad) = e.publish(
        &ep(2),
        &ep(2),
        vec![(format!("{}.mkv", ep(2)), mkv(3))],
        0.05,
        false,
    );
    bad["magnetUrl"] = json!("magnet:?xt=urn:btih:zz");
    bad["downloadUrl"] = Value::Null;
    e.search_results("Parity Partial S01E01", vec![e1]);
    e.search_results("Parity Partial S01E02", vec![bad]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:999005:1" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    assert_no_leaks(&text);
    let ex = &v["plan"]["execution"];
    assert_eq!(
        (v["plan"]["state"].as_str(), ex["outcome"].as_str()),
        (Some("download_queued"), Some("partially_queued")),
        "{text}"
    );
    assert_eq!(ex["grabs"].as_array().unwrap().len(), 1);
    assert_eq!(ex["refused"][0]["code"], "invalid_payload", "{text}");
    // The refused, still-missing episode keeps the explicit request active.
    let r = requests(&e);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["state"], "active", "{r:?}");
    assert!(
        r[0]["last_result"]
            .as_str()
            .unwrap()
            .contains("partially queued (invalid_payload)"),
        "{r:?}"
    );
    // Start the worker clock after the API committed the enqueue. If the request crosses
    // a second boundary, a clock captured before it would precede next_attempt_at.
    let t0 = time::OffsetDateTime::now_utc();
    e.tick_at(t0).await;
    assert_eq!(e.qb.lock().unwrap().add_fields.len(), 1);

    // A valid alternative appears; the retry (after its backoff) queues only E02.
    let (e2_hash, e2) = e.publish(
        &format!("{}-ALT", ep(2)),
        &format!("{}-ALT", ep(2)),
        vec![(format!("{}-ALT.mkv", ep(2)), mkv(4))],
        0.05,
        false,
    );
    e.search_results("Parity Partial S01E02", vec![e2]);
    let later = t0 + std::time::Duration::from_secs(2 * 3600);
    e.tick_at(later).await;
    e.tick_at(later + std::time::Duration::from_secs(60)).await;
    {
        let q = e.qb.lock().unwrap();
        assert_eq!(q.add_fields.len(), 2, "E01 is not added again");
        assert!(q.torrents.contains_key(&e1_hash) && q.torrents.contains_key(&e2_hash));
    }
    let (_, queue, text) = e.json(e.get("/api/v1/queue")).await;
    assert_eq!(queue["items"].as_array().unwrap().len(), 2, "{text}");
    let r = requests(&e);
    assert_eq!(r[0]["state"], "satisfied", "{r:?}");

    // Replaying the add while everything is queued creates no active request.
    let (s, v, text) = e
        .json(
            e.post("/api/v1/library")
                .json(&json!({ "media_id": "season:tvdb:999005:1" })),
        )
        .await;
    assert_eq!(
        (s, v["plan"]["state"].as_str()),
        (201, Some("in_progress")),
        "{text}"
    );
    let r = requests(&e);
    assert_eq!(
        (r.len(), r[0]["state"].as_str()),
        (1, Some("satisfied")),
        "{r:?}"
    );
    e.tick_at(later + std::time::Duration::from_secs(120)).await;
    assert_eq!(e.qb.lock().unwrap().add_fields.len(), 2, "no duplicate");
}

#[tokio::test]
async fn a_refusal_for_an_already_queued_unit_creates_no_request() {
    let e = start_exec(ExecOptions::default()).await;
    let (_, r) = matrix(&e);
    e.search_results("The Matrix 1999", vec![r]);
    // A shadow plan, then the same release is grabbed interactively.
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603" })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let plan_id = v["plan"]["id"].as_str().unwrap().to_string();
    let release_ref = v["plan"]["candidates"][0]["release_ref"].clone();
    let (s, _, text) = e
        .json(
            e.post("/api/v1/grabs")
                .json(&json!({ "media_id": "movie:tmdb:603", "release_ref": release_ref })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    // Executing the stale plan refuses its candidate: the movie is already being downloaded.
    let (s, v, text) = e
        .json(e.post(&format!("/api/v1/plans/{plan_id}/execute")))
        .await;
    assert_eq!(s, 200, "{text}");
    let ex = &v["plan"]["execution"];
    assert_eq!(
        (ex["outcome"].as_str(), ex["refused"][0]["code"].as_str()),
        (Some("nothing_queued"), Some("covers_queued")),
        "{text}"
    );
    let rows = e
        .state
        .store
        .blocking(|c| arr::worker::list_requests(c, false))
        .unwrap();
    assert!(
        rows.iter().all(|r| r["target"] != "movie:tmdb:603"),
        "{rows:?}"
    );
}
