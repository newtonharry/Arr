//! The show and movie pages of the web app: the page endpoints, getting a season or an
//! episode, following new episodes, and stopping. Served through the web app's proxy, as the
//! page uses them.
#![recursion_limit = "256"]

mod common;

use arr::api;
use common::*;
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;

// ------------------------------------------------------------------ harness

struct Site {
    base: String,
    http: reqwest::Client,
    cookie: String,
}

async fn exec() -> Exec {
    start_exec(ExecOptions {
        tmdb: true,
        ..Default::default()
    })
    .await
}

async fn signed_in(state: Arc<api::AppState>) -> Site {
    let (app, _stop) = api::app(state.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let link = arr::webapp::app_link(&state.cfg).unwrap();
    let token = link.split_once("/app/link?t=").unwrap().1.to_string();
    let r = http
        .post(format!("{base}/app/link"))
        .form(&[("t", token.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let cookie = r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    Site { base, http, cookie }
}

impl Site {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut rb = self
            .http
            .request(method, format!("{}/app/api/v1{path}", self.base))
            .header("x-arr-app", "1")
            .header("cookie", &self.cookie);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let r = rb.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.call(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.call(Method::POST, path, Some(body)).await
    }

    async fn show(&self, id: &str) -> Value {
        let (s, v) = self.get(&format!("/show/{id}")).await;
        assert_eq!(s, 200, "{v}");
        v
    }
}

fn season(show: &Value, n: u64) -> &Value {
    show["seasons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["number"] == n)
        .unwrap_or_else(|| panic!("no season {n}: {show}"))
}

/// Monitored episodes per season of a catalog series, from the library list.
async fn monitored(e: &Exec, q: &str, n: u64) -> u64 {
    let (_, lib, _) = e
        .json(e.get(&format!("/api/v1/library/series?q={q}")))
        .await;
    lib["items"][0]["seasons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["number"] == n)
        .map_or(0, |s| s["monitored_episodes"].as_u64().unwrap())
}

// ------------------------------------------------------------------ the show page

#[tokio::test]
async fn the_show_page_of_a_catalog_show_says_what_is_in_plex() {
    let e = exec().await;
    let site = signed_in(e.state.clone()).await;
    let v = site.show("series:tvdb:372264").await;
    assert_eq!(v["media_id"], "series:tvdb:372264");
    assert_eq!(
        (v["title"].clone(), v["year"].clone()),
        (json!("Slow Horses"), json!(2022))
    );
    assert_eq!(v["network"], "Apple TV+");
    assert_eq!(v["status"], "Running");
    assert_eq!(v["summary"], "About Slow Horses & its people.");
    assert_eq!(
        v["following"], true,
        "an imported, monitored show follows new episodes"
    );
    let s1 = season(&v, 1);
    assert_eq!(
        (
            s1["episode_count"].clone(),
            s1["aired_count"].clone(),
            s1["in_plex"].clone(),
            s1["wanted"].clone(),
            s1["state"].clone()
        ),
        (json!(6), json!(6), json!(2), json!(4), json!("partly")),
        "{v}"
    );
    // The same show by its TVmaze key resolves to the catalog entry.
    let by_maze = site.show("series:tvmaze:9001").await;
    assert_eq!(by_maze["media_id"], "series:tvdb:372264");
    // Its episodes, with their states.
    let (s, eps) = site.get("/show/series:tvdb:372264/season/1").await;
    assert_eq!(s, 200, "{eps}");
    let states: Vec<&str> = eps["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["state"].as_str().unwrap())
        .collect();
    assert_eq!(
        states,
        ["in_plex", "wanted", "in_plex", "wanted", "wanted", "wanted"]
    );
    assert_eq!(eps["episodes"][1]["media_id"], "episode:tvdb:372264:1:2");
    assert_eq!(eps["episodes"][1]["air_date"], "2025-02-02");
    assert_eq!(eps["episodes"][1]["title"], "Ep 2");
    let (s, v) = site.get("/show/series:tvdb:372264/season/9").await;
    assert_eq!((s, v["error"]["code"].as_str()), (404, Some("not_found")));
}

#[tokio::test]
async fn the_show_page_of_a_show_outside_the_catalog_comes_from_tvmaze() {
    let e = exec().await;
    let site = signed_in(e.state.clone()).await;
    let v = site.show("series:tvdb:999005").await;
    assert_eq!(v["title"], "Parity Partial");
    assert_eq!(v["following"], false);
    assert_eq!(season(&v, 1)["episode_count"], 3);
    assert_eq!(season(&v, 1)["aired_count"], 2);
    assert_eq!(
        season(&v, 1)["state"],
        "none",
        "nothing is wanted until a person asks"
    );
    assert_eq!(season(&v, 1)["next_air_date"], "2099-01-24");
    assert_eq!(season(&v, 2)["episode_count"], 2);
    // Mixed states: two that aired and one that has not.
    let (_, eps) = site.get("/show/series:tvdb:999005/season/1").await;
    let states: Vec<&str> = eps["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["none", "none", "upcoming"]);
    assert_eq!(eps["episodes"][2]["air_date"], "2099-01-24");
    // A show with nothing aired is "upcoming" as a whole.
    let v = site.show("series:tvdb:999006").await;
    assert_eq!(season(&v, 1)["state"], "upcoming");
    // Looking at a page adds nothing to the catalog.
    let (_, lib, _) = e.json(e.get("/api/v1/library/series?q=parity")).await;
    assert_eq!(lib["items"].as_array().unwrap().len(), 0, "{lib}");
    // Unknown shows are a calm 404; a movie id is not a show.
    let (s, v) = site.get("/show/series:tvdb:424242").await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (404, Some("metadata_unavailable")),
        "{v}"
    );
    let (s, _) = site.get("/show/movie:tmdb:603").await;
    assert_eq!(s, 400);
    // Without TVmaze a show outside the catalog has no page.
    let off = start_exec(ExecOptions {
        tvmaze: false,
        ..Default::default()
    })
    .await;
    let site = signed_in(off.state.clone()).await;
    let (s, _) = site.get("/show/series:tvdb:999005").await;
    assert_eq!(s, 404);
}

// ------------------------------------------------------------------ getting a season or an episode

#[tokio::test]
async fn getting_a_season_or_an_episode_monitors_exactly_that() {
    let e = exec().await;
    let site = signed_in(e.state.clone()).await;
    let (s, v) = site
        .post("/quick-add", json!({ "media_id": "season:tvdb:999005:2" }))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (
            v["media_id"].clone(),
            v["title"].clone(),
            v["detail"].clone(),
            v["outcome"].clone()
        ),
        (
            json!("season:tvdb:999005:2"),
            json!("Parity Partial"),
            json!("Season 2"),
            json!("looking")
        ),
        "{v}"
    );
    let page = site.show("series:tvdb:999005").await;
    assert_eq!(season(&page, 2)["state"], "wanted", "{page}");
    assert_eq!(season(&page, 2)["wanted"], 2);
    assert_eq!(
        season(&page, 1)["state"],
        "none",
        "season 1 was not asked for"
    );
    assert_eq!(
        page["following"], false,
        "getting a season does not follow new episodes"
    );
    // One episode of another season.
    let (s, v) = site
        .post(
            "/quick-add",
            json!({ "media_id": "episode:tvdb:999005:1:2" }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["detail"], "S1 E2 \u{b7} Ep 2");
    let (_, eps) = site.get("/show/series:tvdb:999005/season/1").await;
    let states: Vec<&str> = eps["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["none", "wanted", "upcoming"]);
    assert_eq!(monitored(&e, "parity", 1).await, 1);
    assert_eq!(monitored(&e, "parity", 2).await, 2);
    // A season the show does not have is refused, and a bad id too.
    let (s, v) = site
        .post("/quick-add", json!({ "media_id": "season:tvdb:999005:9" }))
        .await;
    assert_eq!(s, 404, "{v}");
    let (s, _) = site
        .post("/quick-add", json!({ "media_id": "season:nope" }))
        .await;
    assert!(matches!(s, 400 | 422));
}

// ------------------------------------------------------------------ following new episodes

#[tokio::test]
async fn following_changes_only_what_has_not_aired() {
    let e = exec().await;
    let site = signed_in(e.state.clone()).await;
    // Off for a show that is not here: nothing is added.
    let (s, v) = site
        .post(
            "/follow",
            json!({ "media_id": "series:tvdb:999005", "on": false }),
        )
        .await;
    assert_eq!((s, v["following"].clone()), (200, json!(false)), "{v}");
    let (_, lib, _) = e.json(e.get("/api/v1/library/series?q=parity")).await;
    assert_eq!(lib["items"].as_array().unwrap().len(), 0);
    // On: the show joins the catalog with only its coming episode monitored.
    let (s, v) = site
        .post(
            "/follow",
            json!({ "media_id": "series:tvdb:999005", "on": true }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["media_id"].clone(), v["following"].clone()),
        (json!("series:tvdb:999005"), json!(true))
    );
    assert_eq!(
        monitored(&e, "parity", 1).await,
        1,
        "the episode of 2099 only"
    );
    assert_eq!(monitored(&e, "parity", 2).await, 0);
    let page = site.show("series:tvdb:999005").await;
    assert_eq!(page["following"], true);
    assert_eq!(
        season(&page, 1)["state"],
        "none",
        "aired episodes are not asked for"
    );
    // Get season 2, then switch following off: season 2 stays, what is to come does not.
    let (s, _) = site
        .post("/quick-add", json!({ "media_id": "season:tvdb:999005:2" }))
        .await;
    assert_eq!(s, 200);
    let (s, v) = site
        .post(
            "/follow",
            json!({ "media_id": "series:tvdb:999005", "on": false }),
        )
        .await;
    assert_eq!((s, v["following"].clone()), (200, json!(false)), "{v}");
    assert_eq!(
        monitored(&e, "parity", 1).await,
        0,
        "nothing more is expected"
    );
    assert_eq!(
        monitored(&e, "parity", 2).await,
        2,
        "a chosen season keeps its episodes"
    );
    let page = site.show("series:tvdb:999005").await;
    assert_eq!(page["following"], false);
    assert_eq!(season(&page, 2)["state"], "wanted");
    // On again, and a second time: the same.
    for _ in 0..2 {
        let (_, v) = site
            .post(
                "/follow",
                json!({ "media_id": "series:tvdb:999005", "on": true }),
            )
            .await;
        assert_eq!(v["following"], true);
    }
    assert_eq!(monitored(&e, "parity", 1).await, 1);
    assert_eq!(monitored(&e, "parity", 2).await, 2);
    // Only a series can be followed; unknown fields are refused.
    let (s, _) = site
        .post(
            "/follow",
            json!({ "media_id": "movie:tmdb:603", "on": true }),
        )
        .await;
    assert_eq!(s, 400);
    let (s, _) = site
        .post("/follow", json!({ "media_id": "series:tvdb:999005" }))
        .await;
    assert!(matches!(s, 400 | 422));
}

// ------------------------------------------------------------------ stopping

#[tokio::test]
async fn stopping_a_movie_arr_is_looking_for_ends_the_search() {
    let e = exec().await;
    let site = signed_in(e.state.clone()).await;
    // Arr is looking for it (nothing to download yet).
    let (s, v) = site
        .post("/quick-add", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    assert_eq!((s, v["outcome"].clone()), (200, json!("looking")), "{v}");
    let (s, page) = site.get("/movie/movie:tmdb:900001").await;
    assert_eq!(s, 200, "{page}");
    assert_eq!(
        (
            page["title"].clone(),
            page["year"].clone(),
            page["runtime_minutes"].clone(),
            page["state"].clone()
        ),
        (json!("Leviticus"), json!(2025), json!(86), json!("wanted")),
        "{page}"
    );
    let (_, reqs, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(reqs["items"][0]["state"], "active", "{reqs}");
    let (s, v) = site
        .post("/stop", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["stopped"].clone(), v["cancelled"].clone()),
        (json!(true), json!(0))
    );
    let (_, page) = site.get("/movie/movie:tmdb:900001").await;
    assert_eq!(page["state"], "none");
    let (_, reqs, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(reqs["items"][0]["state"], "cancelled", "{reqs}");
    let (_, st, _) = e.json(e.get("/api/v1/status/movie:tmdb:900001")).await;
    assert_eq!(st["monitored"], false, "{st}");
    // A copy turns up later: nothing picks it up now.
    let t = "Leviticus.2025.1080p.WEB-DL.x264-UM";
    let (_, r) = e.publish(t, t, vec![(format!("{t}.mkv"), mkv(1))], 1.0, false);
    e.search_results("Leviticus 2025", vec![r]);
    e.tick_at(time::OffsetDateTime::now_utc() + time::Duration::days(2))
        .await;
    let (_, q, _) = e.json(e.get("/api/v1/queue")).await;
    assert_eq!(q["items"].as_array().unwrap().len(), 0, "{q}");
    // Stopping again is harmless; something Arr never had is a calm 404.
    let (s, _) = site
        .post("/stop", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    assert_eq!(s, 200);
    let (s, v) = site
        .post("/stop", json!({ "media_id": "movie:tmdb:777" }))
        .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (404, Some("not_managed")),
        "{v}"
    );
    let (s, _) = site
        .post("/stop", json!({ "media_id": "series:tvdb:372264" }))
        .await;
    assert_eq!(s, 400, "a whole show is stopped by its seasons");
}

#[tokio::test]
async fn stopping_a_season_cancels_its_download_and_leaves_the_library_alone() {
    let e = start_exec(ExecOptions::default()).await;
    let site = signed_in(e.state.clone()).await;
    // Episode 4 arrives in the library.
    let done = "Slow.Horses.S01E04.1080p.WEB-DL.x264-UM";
    let (hash4, r) = e.publish(
        done,
        done,
        vec![(format!("{done}.mkv"), mkv(2))],
        1.5,
        false,
    );
    e.search_results("Slow Horses S01E04", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:4", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let g4 = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    e.fake.lock().unwrap().complete(&hash4);
    e.tick().await;
    assert_eq!(e.grab_row(&g4).state, "imported");
    let library = e.tv.join(format!("Slow Horses/Season 1/{done}.mkv"));
    assert!(library.is_file());
    // Episode 2 is on its way.
    let now = "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM";
    let (hash2, r) = e.publish(now, now, vec![(format!("{now}.mkv"), mkv(1))], 1.5, false);
    e.search_results("Slow Horses S01E02", vec![r]);
    let (_, v, _) = e
        .json(e.post("/api/v1/plans").json(
            &json!({ "media_id": "episode:tvdb:372264:1:2", "execute": true, "replan": true }),
        ))
        .await;
    let g2 = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    assert_eq!(e.grab_row(&g2).state, "downloading");
    let page = site.show("series:tvdb:372264").await;
    assert_eq!(season(&page, 1)["state"], "on_its_way", "{page}");
    assert_eq!(season(&page, 1)["on_its_way"], 1);
    let (_, eps) = site.get("/show/series:tvdb:372264/season/1").await;
    assert_eq!(eps["episodes"][1]["state"], "on_its_way");
    assert_eq!(
        eps["episodes"][3]["state"], "in_plex",
        "episode 4 is in the library now"
    );
    // Stop the season.
    let (s, v) = site
        .post("/stop", json!({ "media_id": "season:tvdb:372264:1" }))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["cancelled"].clone(), v["finishing"].clone()),
        (json!(1), json!(0)),
        "{v}"
    );
    assert_eq!(e.grab_row(&g2).state, "cancelled");
    assert!(
        !e.fake.lock().unwrap().torrents.contains_key(&hash2),
        "the unfinished download is removed from the client"
    );
    // The library is untouched: the imported file is still there and still counted.
    assert!(library.is_file(), "an imported file is never removed");
    assert!(playable(&library).await);
    assert_eq!(e.grab_row(&g4).state, "imported");
    assert_eq!(
        monitored(&e, "slow", 1).await,
        0,
        "Arr stops looking for the season"
    );
    let page = site.show("series:tvdb:372264").await;
    assert_eq!(season(&page, 1)["state"], "partly");
    assert_eq!(season(&page, 1)["in_plex"], 3);
    assert_eq!(season(&page, 1)["wanted"], 0);
    // The automatic search no longer wants anything of season 1 (other seasons may stay wanted).
    let (_, w, _) = e
        .json(e.get("/api/v1/wanted?media_id=series:tvdb:372264"))
        .await;
    let of_season_1 = w["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| {
            i["target"]
                .as_str()
                .unwrap()
                .starts_with("episode:tvdb:372264:1:")
        })
        .count();
    assert_eq!(of_season_1, 0, "{w}");
}

#[tokio::test]
async fn stopping_an_episode_only_cancels_its_own_download() {
    let e = start_exec(ExecOptions::default()).await;
    let site = signed_in(e.state.clone()).await;
    let t = "Slow.Horses.S01E05.1080p.WEB-DL.x264-UM";
    let (_, r) = e.publish(t, t, vec![(format!("{t}.mkv"), mkv(1))], 1.5, false);
    e.search_results("Slow Horses S01E05", vec![r]);
    let (_, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:5", "execute": true })),
        )
        .await;
    let g = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    // Stopping another episode of the season does not touch it.
    let (s, v) = site
        .post("/stop", json!({ "media_id": "episode:tvdb:372264:1:6" }))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["cancelled"], 0);
    assert_eq!(e.grab_row(&g).state, "downloading");
    // Stopping its own episode cancels it.
    let (_, v) = site
        .post("/stop", json!({ "media_id": "episode:tvdb:372264:1:5" }))
        .await;
    assert_eq!(v["cancelled"], 1, "{v}");
    assert_eq!(e.grab_row(&g).state, "cancelled");
    let (_, eps) = site.get("/show/series:tvdb:372264/season/1").await;
    assert_eq!(eps["episodes"][4]["state"], "none");
    assert_eq!(eps["episodes"][5]["state"], "none");
}

// ------------------------------------------------------------------ movies

#[tokio::test]
async fn the_movie_page_follows_the_movie_through_its_states() {
    let e = exec().await;
    e.mock.lock().unwrap().tmdb_movies.insert(
        "900002".into(),
        tmdb_movie_with(900002, "Later Movie", 2099, "2099-03-04"),
    );
    let site = signed_in(e.state.clone()).await;
    // In Plex.
    let (s, v) = site.get("/movie/movie:tmdb:604").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["title"].clone(), v["year"].clone(), v["state"].clone()),
        (json!("The Matrix Reloaded"), json!(2003), json!("in_plex"))
    );
    // Not added yet: details come from Plex (a summary too), and nothing is wanted.
    let (s, v) = site
        .get(&format!("/movie/movie:tmdb:{PLEX_RELEASED}"))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["title"].clone(), v["state"].clone(), v["summary"].clone()),
        (
            json!("Plex Released Movie"),
            json!("none"),
            json!("About Plex Released Movie.")
        )
    );
    assert_eq!(v["runtime_minutes"], 100);
    // Added before it is out: waiting, with the date.
    site.post("/quick-add", json!({ "media_id": "movie:tmdb:900002" }))
        .await;
    let (_, v) = site.get("/movie/movie:tmdb:900002").await;
    assert_eq!(
        (v["state"].clone(), v["until"].clone()),
        (json!("waiting"), json!("2099-03-04"))
    );
    // On its way, with progress.
    let name = "The.Matrix.1999.1080p.WEB-DL.x264-UM";
    let (hash, r) = e.publish(name, name, matrix_files(), 8.0, false);
    e.search_results("The Matrix 1999", vec![r]);
    site.post("/quick-add", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    e.tick().await;
    e.fake.lock().unwrap().set_state(&hash, "downloading", 0.62);
    e.tick().await;
    let (_, v) = site.get("/movie/movie:tmdb:603").await;
    assert_eq!(v["state"], "on_its_way", "{v}");
    assert!((v["progress"].as_f64().unwrap() - 0.62).abs() < 0.01, "{v}");
    // Stopping it cancels the download.
    let (_, v) = site
        .post("/stop", json!({ "media_id": "movie:tmdb:603" }))
        .await;
    assert_eq!(v["cancelled"], 1, "{v}");
    let (_, v) = site.get("/movie/movie:tmdb:603").await;
    assert_eq!(v["state"], "none");
    // No metadata anywhere: a calm 404.
    let (s, _) = site.get("/movie/movie:tmdb:123456789").await;
    assert_eq!(s, 404);
}

fn matrix_files() -> Vec<(String, Vec<u8>)> {
    vec![(
        "The.Matrix.1999.1080p.WEB-DL.x264-UM.mkv".to_string(),
        mkv(1),
    )]
}

// ------------------------------------------------------------------ the proxy

#[tokio::test]
async fn the_app_reaches_the_page_routes_and_nothing_broader() {
    let e = exec().await;
    let site = signed_in(e.state.clone()).await;
    for (m, p) in [
        (Method::GET, "/show/series:tvdb:372264"),
        (Method::GET, "/show/series:tvdb:372264/season/1"),
        (Method::GET, "/movie/movie:tmdb:603"),
    ] {
        let (s, v) = site.call(m.clone(), p, None).await;
        assert_eq!(s, 200, "{m} {p} {v}");
    }
    for (m, p) in [
        (Method::PATCH, "/library/series:tvdb:372264"),
        (Method::PATCH, "/library/season:tvdb:372264:1"),
        (Method::DELETE, "/library/series:tvdb:372264"),
        (Method::DELETE, "/show/series:tvdb:372264"),
        (Method::POST, "/show/series:tvdb:372264"),
        (Method::POST, "/movie/movie:tmdb:603"),
        (Method::GET, "/follow"),
        (Method::GET, "/stop"),
        (Method::GET, "/show/series:tvdb:372264/season/1/extra"),
        (Method::GET, "/show"),
        (Method::POST, "/queue/grb_x/cancel"),
    ] {
        let (s, v) = site.call(m.clone(), p, Some(json!({}))).await;
        assert_eq!(
            (s, v["error"]["code"].as_str()),
            (404, Some("not_found")),
            "{m} {p}"
        );
    }
    // The marker header and the session are still needed.
    let r = site
        .http
        .post(format!("{}/app/api/v1/stop", site.base))
        .json(&json!({ "media_id": "movie:tmdb:603" }))
        .header("cookie", &site.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = site
        .http
        .post(format!("{}/app/api/v1/follow", site.base))
        .header("x-arr-app", "1")
        .json(&json!({ "media_id": "series:tvdb:372264", "on": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

// ------------------------------------------------------------------ review fixes

fn managed_with_tmdb() -> ExecOptions {
    ExecOptions {
        categories: Some(("um-movies", "um-tv")),
        tmdb: true,
        ..Default::default()
    }
}

async fn run_command(e: &Exec, body: Value) {
    let (s, v, text) = e.json(e.post("/api/v1/commands").json(&body)).await;
    assert!(s == 202 || s == 200, "{text}");
    let id = v["id"].as_str().unwrap().to_string();
    e.tick().await;
    let (_, v, _) = e.json(e.get(&format!("/api/v1/commands/{id}"))).await;
    assert_eq!(v["command"]["state"], "completed", "{v}");
}

fn client_adds(e: &Exec) -> usize {
    e.fake.lock().unwrap().add_fields.len()
}

#[tokio::test]
async fn switching_following_off_stops_late_episodes_but_a_chosen_season_keeps_taking_them() {
    let e = exec().await;
    let site = signed_in(e.state.clone()).await;
    site.post(
        "/follow",
        json!({ "media_id": "series:tvdb:999005", "on": true }),
    )
    .await;
    site.post("/quick-add", json!({ "media_id": "season:tvdb:999005:2" }))
        .await;
    let (_, v) = site
        .post(
            "/follow",
            json!({ "media_id": "series:tvdb:999005", "on": false }),
        )
        .await;
    assert_eq!(v["following"], false);
    // The provider now lists a coming episode of season 1 and a late one of season 2.
    e.mock.lock().unwrap().tvmaze_extra.insert(
        "9005".into(),
        vec![
            json!({ "season": 1, "number": 4, "name": "Ep 4", "type": "regular", "airstamp": "2099-02-01T02:00:00+00:00", "runtime": 1 }),
            json!({ "season": 2, "number": 3, "name": "Ep 3", "type": "regular", "airstamp": "2025-06-15T02:00:00+00:00", "runtime": 1 }),
        ],
    );
    run_command(
        &e,
        json!({ "name": "refresh_metadata", "media_id": "series:tvdb:999005" }),
    )
    .await;
    assert_eq!(
        monitored(&e, "parity", 1).await,
        0,
        "following is off: nothing new in season 1"
    );
    assert_eq!(
        monitored(&e, "parity", 2).await,
        3,
        "a season someone chose takes its late episode"
    );
}

#[tokio::test]
async fn a_stopped_episode_is_not_grabbed_by_its_seasons_request() {
    let e = start_exec(managed_with_tmdb()).await;
    let site = signed_in(e.state.clone()).await;
    let (_, v) = site
        .post("/quick-add", json!({ "media_id": "season:tvdb:999005:2" }))
        .await;
    assert_eq!(v["outcome"], "looking", "{v}");
    let (_, reqs, _) = e.json(e.get("/api/v1/requests")).await;
    assert_eq!(reqs["items"][0]["state"], "active", "{reqs}");
    let (s, v) = site
        .post("/stop", json!({ "media_id": "episode:tvdb:999005:2:1" }))
        .await;
    assert_eq!(s, 200, "{v}");
    // A copy of just that episode turns up for the season's search and its own.
    let t = "Parity.Partial.S02E01.1080p.WEB-DL.x264-UM";
    let (_, r) = e.publish(t, t, vec![(format!("{t}.mkv"), mkv(1))], 1.0, false);
    e.search_results("Parity Partial S02", vec![r.clone()]);
    e.search_results("Parity Partial S02E01", vec![r]);
    let now = time::OffsetDateTime::now_utc();
    for days in [1, 3] {
        e.tick_at(now + time::Duration::days(days)).await;
    }
    let searched = e.mock.lock().unwrap().log.iter().any(|q| q.contains("S02"));
    assert!(searched, "the retry did search for the season");
    let (_, q, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"], json!([]), "{q}");
    assert_eq!(client_adds(&e), 0);
    // Asking for the episode itself works again.
    let (s, _) = site
        .post(
            "/quick-add",
            json!({ "media_id": "episode:tvdb:999005:2:1" }),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(monitored(&e, "parity", 2).await, 2);
}

#[tokio::test]
async fn a_download_enqueued_after_a_stop_is_refused() {
    let e = start_exec(managed_with_tmdb()).await;
    let site = signed_in(e.state.clone()).await;
    let t = "Leviticus.2025.1080p.WEB-DL.DDP5.1.H.264-GRP";
    let (hash, rel) = e.publish(t, t, vec![(format!("{t}.mkv"), mkv(2))], 4.0, false);
    e.search_results("Leviticus 2025", vec![rel]);
    let add = json!({ "media_id": "movie:tmdb:900001", "search": false });
    let (s, _, text) = e.json(e.post("/api/v1/library").json(&add)).await;
    assert_eq!(s, 201, "{text}");
    let (_, v, text) = e
        .json(e.get("/api/v1/media/movie:tmdb:900001/releases"))
        .await;
    let release_ref = v["releases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["decision"]["approved"] == true)
        .unwrap_or_else(|| panic!("an approved release: {text}"))["release_ref"]
        .clone();
    let grab = json!({ "media_id": "movie:tmdb:900001", "release_ref": release_ref });
    // The grab passes its checks, then its torrent fetch is held open.
    let gate = Arc::new(tokio::sync::Notify::new());
    e.mock.lock().unwrap().download_gate = Some(gate.clone());
    let pending = tokio::spawn(e.post("/api/v1/grabs").json(&grab).send());
    for _ in 0..1000 {
        if e.mock.lock().unwrap().downloads.contains(&hash) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        e.mock.lock().unwrap().downloads.contains(&hash),
        "the fetch started"
    );
    let (s, v) = site
        .post("/stop", json!({ "media_id": "movie:tmdb:900001" }))
        .await;
    assert_eq!(
        (s, v["cancelled"].clone()),
        (200, json!(0)),
        "nothing recorded yet: {v}"
    );
    gate.notify_one();
    let r = pending.await.unwrap().unwrap();
    let status = r.status().as_u16();
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (409, Some("not_adopted")),
        "{v}"
    );
    e.tick().await;
    assert_eq!(client_adds(&e), 0);
    let (_, q, _) = e.json(e.get("/api/v1/queue?all=true")).await;
    assert_eq!(q["items"], json!([]), "{q}");
}

#[tokio::test]
async fn a_download_that_has_all_its_data_is_left_to_finish_by_stop() {
    let e = start_exec(ExecOptions::default()).await;
    let site = signed_in(e.state.clone()).await;
    let t = "Slow.Horses.S01E02.1080p.WEB-DL.x264-UM";
    let (hash, r) = e.publish(t, t, vec![(format!("{t}.mkv"), mkv(1))], 1.5, false);
    e.search_results("Slow Horses S01E02", vec![r]);
    let (_, v, _) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "episode:tvdb:372264:1:2", "execute": true })),
        )
        .await;
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    // The worker has just moved it on: all its data is there and it is about to import.
    let g = id.clone();
    e.state
        .store
        .blocking(move |c| {
            Ok(c.execute("UPDATE grabs SET state = 'completed' WHERE id = ?1", [&g])?)
        })
        .unwrap();
    let (s, v) = site
        .post("/stop", json!({ "media_id": "season:tvdb:372264:1" }))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        (v["cancelled"].clone(), v["finishing"].clone()),
        (json!(0), json!(1)),
        "{v}"
    );
    assert_eq!(e.grab_row(&id).state, "completed");
    assert!(
        e.fake.lock().unwrap().torrents.contains_key(&hash),
        "its data is not removed"
    );
}

#[tokio::test]
async fn a_season_that_appeared_while_following_stops_taking_episodes_when_it_is_switched_off() {
    let e = exec().await;
    let site = signed_in(e.state.clone()).await;
    site.post(
        "/follow",
        json!({ "media_id": "series:tvdb:999005", "on": true }),
    )
    .await;
    let ep = |s: u32, n: u32, at: &str| json!({ "season": s, "number": n, "name": format!("Ep {n}"), "type": "regular", "airstamp": at, "runtime": 1 });
    let refresh = json!({ "name": "refresh_metadata", "media_id": "series:tvdb:999005" });
    // A whole new season appears while following: it is followed.
    let mut extra = vec![ep(3, 1, "2099-03-01T02:00:00+00:00")];
    e.mock
        .lock()
        .unwrap()
        .tvmaze_extra
        .insert("9005".into(), extra.clone());
    run_command(&e, refresh.clone()).await;
    assert_eq!(monitored(&e, "parity", 3).await, 1);
    site.post(
        "/follow",
        json!({ "media_id": "series:tvdb:999005", "on": false }),
    )
    .await;
    assert_eq!(monitored(&e, "parity", 3).await, 0);
    // A late episode of that season is no longer picked up.
    extra.push(ep(3, 2, "2099-03-08T02:00:00+00:00"));
    e.mock
        .lock()
        .unwrap()
        .tvmaze_extra
        .insert("9005".into(), extra);
    run_command(&e, refresh).await;
    assert_eq!(monitored(&e, "parity", 3).await, 0, "following is off");
}
