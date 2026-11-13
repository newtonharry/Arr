//! Sport: finding the releases of an event. The indexers are the loopback fake of the shared
//! mock (Prowlarr's search), the schedule source is its fake ESPN; nothing here reaches either
//! real service. The route is read only, so most of what is proved is what did not happen.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

fn rfc(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap()
}

/// A start a few days ago, on the hour, so the event is over by Arr's reckoning.
fn days_ago(n: i64) -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    (now - time::Duration::days(n))
        .replace_minute(0)
        .unwrap()
        .replace_second(0)
        .unwrap()
        .replace_nanosecond(0)
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
async fn insert_event(
    e: &Exec,
    id: &str,
    name: &str,
    start: OffsetDateTime,
    state: &str,
    home: Option<(&str, &str)>,
    away: Option<(&str, &str)>,
) {
    let league = id.split(':').nth(1).unwrap().to_string();
    let (id, name, state) = (id.to_string(), name.to_string(), state.to_string());
    let (h, a) = (
        home.map(|(i, n)| (i.to_string(), n.to_string())),
        away.map(|(i, n)| (i.to_string(), n.to_string())),
    );
    let at = rfc(start);
    e.state
        .store
        .with(move |c| {
            c.execute(
                "INSERT INTO sport_events (id, league, season, starts_at_utc, ends_at_utc, name, short_name, home_id, home_name, away_id, away_name, round, venue, state, first_seen_utc, updated_utc)
                 VALUES (?1, ?2, NULL, ?3, NULL, ?4, NULL, ?5, ?6, ?7, ?8, NULL, NULL, ?9, ?3, ?3)",
                rusqlite::params![
                    id,
                    league,
                    at,
                    name,
                    h.as_ref().map(|x| x.0.clone()),
                    h.as_ref().map(|x| x.1.clone()),
                    a.as_ref().map(|x| x.0.clone()),
                    a.as_ref().map(|x| x.1.clone()),
                    state
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

fn stock(e: &Exec, query: &str, items: Vec<Value>) {
    e.mock
        .lock()
        .unwrap()
        .releases
        .insert(query.to_string(), items);
}

fn rel(title: &str, published: OffsetDateTime, seeders: i64) -> Value {
    let mut r = release(title, 4.0, seeders);
    r["publishDate"] = json!(rfc(published));
    r
}

async fn table_counts(e: &Exec) -> Vec<(String, i64)> {
    e.state
        .store
        .with(|c| {
            let mut out = vec![];
            for t in [
                "grabs",
                "grab_targets",
                "release_links",
                "files",
                "file_targets",
                "import_journal",
                "blocklist",
                "events",
                "plans",
                "media",
                "commands",
                "sport_events",
                "sport_follows",
            ] {
                let n: i64 = c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))?;
                out.push((t.to_string(), n));
            }
            Ok(out)
        })
        .await
        .unwrap()
}

async fn copies(e: &Exec, id: &str, query: &str) -> (u16, Value, String) {
    e.json(e.get(&format!("/api/v1/sport/event/{id}/copies{query}")))
        .await
}

async fn ufc_setup() -> (Exec, OffsetDateTime) {
    let e = start_exec(ExecOptions::default()).await;
    let start = days_ago(2);
    insert_event(
        &e,
        "event:ufc:600061400",
        "UFC 334: Gane vs. Hokit",
        start,
        "played",
        None,
        None,
    )
    .await;
    (e, start)
}

fn parts(v: &Value) -> Vec<String> {
    v["copies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["part"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_card_gets_its_parts_and_everything_else_is_counted_not_shown() {
    let (e, start) = ufc_setup().await;
    let day_after = start + time::Duration::hours(30);
    stock(
        &e,
        "UFC 334",
        vec![
            rel(
                "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-nVa",
                day_after,
                40,
            ),
            rel("UFC.334.Prelims.1080p.WEB-DL.H264.Fight-BB", day_after, 12),
            rel(
                "UFC.334.Early.Prelims.720p.WEB-DL.H264.Fight-BB",
                day_after,
                4,
            ),
            rel(
                "UFC 334 Countdown 1080p WEBRip h264-TJ [TJET]",
                day_after,
                5,
            ),
            rel(
                "UFC 334 Embedded-Vlog Series-Episode 2 1080p WEBRip h264-TJ [TJET]",
                day_after,
                5,
            ),
            rel(
                "UFC 335 Oliveira vs Lopes Main Card 1080p WEB-DL",
                day_after,
                5,
            ),
            rel("NBA 2K27 Update v2 1-RUNE", day_after, 5),
        ],
    );
    stock(
        &e,
        "UFC gane hokit",
        vec![
            rel("UFC 334 Gane KO Hokit 1080p WEB-DL", day_after, 9),
            rel(
                "UFC 334 Gane vs Hokit 1080P 60FPS Full Event WEB-DL AAC2.0. X265 POOTLED",
                day_after,
                26,
            ),
        ],
    );
    let (s, v, body) = copies(&e, "event:ufc:600061400", "").await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(v["available"], true);
    assert_eq!(v["event"]["id"], "event:ufc:600061400");
    assert_eq!(v["parts"].as_array().unwrap().len(), 3);
    let got = parts(&v);
    assert_eq!(got.iter().filter(|p| *p == "main").count(), 1, "{got:?}");
    assert!(
        got.contains(&"prelims".into()) && got.contains(&"early-prelims".into()),
        "{got:?}"
    );
    assert!(got.contains(&"whole".into()), "{got:?}");
    assert_eq!(v["copies"].as_array().unwrap().len(), 4, "{body}");
    // One recommendation per part, and the main card is the 1080p WEB-DL.
    let main = v["copies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["part"] == "main")
        .unwrap();
    assert!(main["caution"].is_null());
    assert_eq!(main["resolution"], "1080p");
    assert!(main["caution"].is_null());
    assert_eq!(main["part_label"], "Main card");
    // What was left out is counted by reason, in words.
    let skipped = v["skipped"].as_array().unwrap();
    assert!(
        skipped
            .iter()
            .any(|s| s["reason"] == "not the event itself (a show about it)" && s["count"] == 3),
        "{skipped:?}"
    );
    assert!(
        skipped
            .iter()
            .any(|s| s["reason"] == "not sport at all (a video game, an episode)"),
        "{skipped:?}"
    );
    // No result text and no link material reaches the client.
    for needle in ["KO", "passkey", "magnet:", "apikey", "tracker.example"] {
        assert!(!body.contains(needle), "{needle} in {body}");
    }
    assert_no_leaks(&body);
    assert_eq!(v["searched"]["complete"], true);
    assert!(v["searched"]["queries"].as_u64().unwrap() <= 3);
}

/// Every key of every copy, and that none of them holds text the release wrote.
#[tokio::test]
async fn no_text_a_release_wrote_reaches_the_client() {
    let (e, start) = ufc_setup().await;
    let t = start + time::Duration::hours(30);
    let hash = "0123456789abcdef0123456789abcdef01234567";
    stock(
        &e,
        "UFC 334",
        vec![
            rel(
                "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-nVa",
                t,
                40,
            ),
            // Matches the card, with a result in its tag, in a bracket with a hidden character,
            // and a hash as its last word.
            rel("UFC.334.Gane.vs.Hokit.Prelims.1080p.WEB-DL.H264-KO", t, 40),
            rel(
                "UFC.334.Gane.vs.Hokit.Early.Prelims.1080p.WEB-DL [K\u{200b}O]",
                t,
                40,
            ),
            rel(
                &format!("UFC.334.Gane.vs.Hokit.Main.Card.720p.WEB-DL {hash}"),
                t,
                40,
            ),
            rel(
                "UFC 334 Gane vs Hokit Main Card 720p [EN, Gane wins by KO]",
                t,
                40,
            ),
        ],
    );
    let (s, v, body) = copies(&e, "event:ufc:600061400", "").await;
    assert_eq!(s, 200, "{body}");
    let list = v["copies"].as_array().unwrap();
    assert!(list.len() >= 3, "{body}");
    let mut keys: Vec<&str> = list[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "caution",
            "fps",
            "id",
            "indexer",
            "label",
            "language",
            "part",
            "part_label",
            "published_at",
            "resolution",
            "seeders",
            "size_bytes",
            "source",
            "speed"
        ],
        "no title, no tag, no channel"
    );
    for needle in ["H264", "nVa", hash, "wins", "KO", "K\u{200b}O"] {
        assert!(!body.contains(needle), "{needle} in {body}");
    }
    // Each copy says whether to be careful, and why.
    for c in list {
        if !c["caution"].is_null() {
            assert!(
                c["caution"]["reason"].is_string() && c["caution"]["note"].is_string(),
                "{c}"
            );
        }
    }
}

#[tokio::test]
async fn looking_for_copies_changes_nothing() {
    let (e, start) = ufc_setup().await;
    stock(
        &e,
        "UFC 334",
        vec![rel(
            "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-nVa",
            start + time::Duration::hours(30),
            40,
        )],
    );
    let before = table_counts(&e).await;
    let (s, v, _) = copies(&e, "event:ufc:600061400", "?part=main&fresh=true").await;
    assert_eq!(s, 200);
    assert_eq!(v["copies"].as_array().unwrap().len(), 1);
    assert_eq!(
        table_counts(&e).await,
        before,
        "no table gained or lost a row"
    );
    // The route reads: it has no write verb, and nothing was sent to a download client.
    let (s, _, _) = e
        .json(e.post("/api/v1/sport/event/event:ufc:600061400/copies"))
        .await;
    assert_eq!(s, 405);
    assert!(e.fake.lock().unwrap().torrents.is_empty());
}

#[tokio::test]
async fn an_event_that_is_not_over_cancelled_or_postponed_asks_no_indexer() {
    let e = start_exec(ExecOptions::default()).await;
    let now = OffsetDateTime::now_utc();
    insert_event(
        &e,
        "event:ufc:1",
        "UFC 400: A vs. B",
        now - time::Duration::hours(2),
        "scheduled",
        None,
        None,
    )
    .await;
    insert_event(
        &e,
        "event:ufc:2",
        "UFC 401: C vs. D",
        now + time::Duration::days(3),
        "scheduled",
        None,
        None,
    )
    .await;
    insert_event(
        &e,
        "event:ufc:3",
        "UFC 402: E vs. F",
        days_ago(3),
        "cancelled",
        None,
        None,
    )
    .await;
    insert_event(
        &e,
        "event:ufc:4",
        "UFC 403: G vs. H",
        days_ago(3),
        "postponed",
        None,
        None,
    )
    .await;
    for (id, note) in [
        ("event:ufc:1", "not_over_yet"),
        ("event:ufc:2", "not_over_yet"),
        ("event:ufc:3", "cancelled"),
        ("event:ufc:4", "postponed"),
    ] {
        let (s, v, body) = copies(&e, id, "").await;
        assert_eq!(s, 200, "{body}");
        assert_eq!(
            (v["available"].clone(), v["note"].clone()),
            (json!(false), json!(note)),
            "{id}"
        );
        assert_eq!(v["copies"], json!([]));
    }
    assert!(e.mock.lock().unwrap().log.is_empty(), "no search was sent");
}

#[tokio::test]
async fn a_failing_indexer_gives_a_partial_answer_not_an_error() {
    let (e, start) = ufc_setup().await;
    let t = start + time::Duration::hours(30);
    stock(
        &e,
        "UFC gane hokit",
        vec![rel(
            "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-nVa",
            t,
            40,
        )],
    );
    e.mock.lock().unwrap().failing_queries = vec!["UFC 334".into()];
    let (s, v, body) = copies(&e, "event:ufc:600061400", "").await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        v["copies"].as_array().unwrap().len(),
        1,
        "what did answer is kept"
    );
    assert_eq!(v["searched"]["complete"], false);
    assert!(v["searched"]["problem"].is_string(), "{v}");
    // Every query failing is still an answer, with nothing in it (asked after the cooldown).
    e.state
        .sports
        .as_ref()
        .unwrap()
        .copies
        .set_limits(Duration::ZERO, Duration::ZERO, 100);
    e.mock.lock().unwrap().failing_queries =
        vec!["UFC 334".into(), "UFC gane hokit".into(), "UFC gane".into()];
    let (s, v, body) = copies(&e, "event:ufc:600061400", "").await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(v["copies"], json!([]));
    assert_eq!(v["searched"]["complete"], false);
    assert!(v["searched"]["problem"].is_string());
}

#[tokio::test]
async fn a_slow_indexer_does_not_hold_the_request() {
    let (e, start) = ufc_setup().await;
    stock(
        &e,
        "UFC 334",
        vec![rel(
            "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-nVa",
            start + time::Duration::hours(30),
            40,
        )],
    );
    {
        let mut m = e.mock.lock().unwrap();
        for q in ["UFC gane hokit", "UFC gane"] {
            m.slow_queries.push((q.into(), Duration::from_secs(30)));
        }
    }
    let started = Instant::now();
    let (s, v, body) = copies(&e, "event:ufc:600061400", "").await;
    assert_eq!(s, 200, "{body}");
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(v["searched"]["complete"], false);
    assert_eq!(v["copies"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_game_is_found_by_its_teams_and_date_and_a_show_about_the_league_is_not() {
    let e = start_exec(ExecOptions::default()).await;
    let start = days_ago(2).replace_hour(8).unwrap();
    insert_event(
        &e,
        "event:a-league-men:401",
        "Melbourne City v Western Sydney Wanderers",
        start,
        "played",
        Some(("1", "Melbourne City")),
        Some(("2", "Western Sydney Wanderers")),
    )
    .await;
    league_known(
        &e,
        "a-league-men",
        &[("1", "Melbourne City"), ("2", "Western Sydney Wanderers")],
        0,
    )
    .await;
    let d = start.date();
    let ymd = format!("{:04} {:02} {:02}", d.year(), u8::from(d.month()), d.day());
    stock(
        &e,
        &format!("A-League {ymd}"),
        vec![
            rel(
                &format!(
                    "A-League {ymd} Melbourne City Vs Western Sydney Wanderers 1080p HDTV H264-DARKSPORT"
                ),
                start + time::Duration::hours(10),
                20,
            ),
            // A show episode that mentions the league, and a highlights programme.
            rel(
                "Aoashi S02E03 Premier League Match 7-Kashiwa Dai Business School 1080p CR WEB-DL MULTi AAC2.0 H.264-VARYG",
                start + time::Duration::hours(10),
                50,
            ),
            rel(
                &format!(
                    "A-League {ymd} Melbourne City Vs Western Sydney Wanderers Highlights 1080p"
                ),
                start + time::Duration::hours(10),
                50,
            ),
            rel(
                "A-League Show S03E10 Round Table 720p WEB",
                start + time::Duration::hours(10),
                50,
            ),
            // The same teams on another day.
            rel(
                "A-League 2026 01 03 Melbourne City Vs Western Sydney Wanderers 1080p HDTV H264-DARKSPORT",
                start + time::Duration::hours(10),
                50,
            ),
        ],
    );
    let (s, v, body) = copies(&e, "event:a-league-men:401", "").await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(v["parts"], json!([]));
    let list = v["copies"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{body}");
    assert_eq!(list[0]["part"], "whole");
    assert!(list[0]["caution"].is_null());
    assert!(list[0]["caution"].is_null());
}

#[tokio::test]
async fn bad_ids_parts_and_a_missing_event_are_refused_in_words() {
    let (e, _) = ufc_setup().await;
    for (id, query, status) in [
        ("event:nowhere:1", "", 404),
        ("event:ufc:600061401", "", 404),
        ("event:ufc:0600061400", "", 400),
        ("event:ufc:abc", "", 400),
        ("event:ufc", "", 400),
        ("event:ufc:600061400:main:extra", "", 400),
        ("event:ufc:600061400", "?part=r2", 400),
        ("event:ufc:600061400", "?part=nonsense", 400),
        ("event:ufc:600061400:main", "?part=prelims", 400),
        ("event:ufc:600061400", "?unknown=1", 400),
    ] {
        let (s, v, body) = copies(&e, id, query).await;
        assert_eq!(s, status, "{id}{query}: {body}");
        assert!(
            v["error"].is_object() || v["code"].is_string() || v.is_object(),
            "{body}"
        );
    }
    // A part in the id is the same as one in the query.
    let (s, v, _) = copies(&e, "ufc:600061400:main", "").await;
    assert_eq!(s, 200);
    assert_eq!(v["event"]["id"], "event:ufc:600061400");
}

#[tokio::test]
async fn it_needs_the_token_and_sport_can_be_off() {
    let (e, _) = ufc_setup().await;
    let r = e
        .client
        .get(format!(
            "{}/api/v1/sport/event/event:ufc:600061400/copies",
            e.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    let off = start_exec(ExecOptions {
        sports: false,
        ..Default::default()
    })
    .await;
    let (s, v, _) = copies(&off, "event:ufc:600061400", "").await;
    assert_eq!(s, 503);
    assert_eq!(v["error"]["code"], "provider_not_configured", "{v}");
    // The schema lists it.
    let (_, doc, _) = e.json(e.get("/api/v1/openapi.json")).await;
    assert!(doc["paths"]["/api/v1/sport/event/{id}/copies"]["get"].is_object());
}

#[tokio::test]
async fn the_event_json_says_when_arr_reckons_it_over() {
    let (e, start) = ufc_setup().await;
    let (_, v, _) = copies(&e, "event:ufc:600061400", "").await;
    let end =
        OffsetDateTime::parse(v["event"]["expected_end"].as_str().unwrap(), &Rfc3339).unwrap();
    assert_eq!(end - start, time::Duration::hours(8));
}

// ------------------------------------------------------------------ review round 1

/// What the store holds when the league's schedule and team list were read just now.
async fn league_known(e: &Exec, league: &str, teams: &[(&str, &str)], read_days_ago: i64) {
    let league = league.to_string();
    let teams: Vec<(String, String)> = teams
        .iter()
        .map(|(i, n)| (i.to_string(), n.to_string()))
        .collect();
    let at = rfc(OffsetDateTime::now_utc() - time::Duration::days(read_days_ago));
    e.state
        .store
        .with(move |c| {
            for (id, name) in &teams {
                c.execute(
                    "INSERT INTO sport_teams (league, team_id, name, short_name, abbreviation) VALUES (?1, ?2, ?3, NULL, NULL)",
                    rusqlite::params![league, id, name],
                )?;
            }
            c.execute(
                "INSERT OR REPLACE INTO sport_refresh (league, refreshed_utc, teams_utc, failures) VALUES (?1, ?2, ?2, 0)",
                rusqlite::params![league, at],
            )?;
            // What the reads recorded: the list was whole, and the schedule was judged in full
            // for a stretch around now (both as of the same time).
            c.execute(
                "INSERT OR REPLACE INTO sport_teams_read (league, read_utc, complete) VALUES (?1, ?2, 1)",
                rusqlite::params![league, at],
            )?;
            let now = OffsetDateTime::now_utc();
            c.execute(
                "INSERT OR REPLACE INTO sport_ranges (league, from_utc, to_utc, read_utc) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    league,
                    rfc(now - time::Duration::days(90)),
                    rfc(now + time::Duration::days(90)),
                    at
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

fn search_queries(e: &Exec) -> usize {
    e.mock
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|l| l.starts_with("t=search&q="))
        .count()
}

fn gate(e: &Exec) -> &arr::sport::gate::CopiesGate {
    &e.state.sports.as_ref().unwrap().copies
}

#[tokio::test]
async fn a_partial_team_name_is_taken_only_from_a_list_that_is_whole_and_recent() {
    for (days_ago_read, want) in [(0, "sure"), (30, "none")] {
        let e = start_exec(ExecOptions::default()).await;
        let start = days_ago(2).replace_hour(8).unwrap();
        insert_event(
            &e,
            "event:a-league-men:401",
            "Melbourne City v Western Sydney Wanderers",
            start,
            "played",
            Some(("1", "Melbourne City")),
            Some(("2", "Western Sydney Wanderers")),
        )
        .await;
        league_known(
            &e,
            "a-league-men",
            &[
                ("1", "Melbourne City"),
                ("2", "Western Sydney Wanderers"),
                ("3", "Sydney FC"),
            ],
            days_ago_read,
        )
        .await;
        let d = start.date();
        let ymd = format!("{:04} {:02} {:02}", d.year(), u8::from(d.month()), d.day());
        stock(
            &e,
            &format!("A-League {ymd}"),
            vec![rel(
                &format!("A-League {ymd} Melbourne City Vs Wanderers 1080p HDTV H264-DARKSPORT"),
                start + time::Duration::hours(10),
                20,
            )],
        );
        let (s, v, body) = copies(&e, "event:a-league-men:401", "").await;
        assert_eq!(s, 200, "{body}");
        let list = v["copies"].as_array().unwrap();
        match want {
            "sure" => {
                assert_eq!(list.len(), 1, "{body}");
                assert!(list[0]["caution"].is_null());
            }
            // The list is a month old: "Wanderers" alone is not told from another club's name.
            _ => assert!(list.is_empty(), "{body}"),
        }
    }
}

#[tokio::test]
async fn asking_again_and_again_asks_the_indexers_once() {
    let (e, start) = ufc_setup().await;
    stock(
        &e,
        "UFC 334",
        vec![rel(
            "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-nVa",
            start + time::Duration::hours(30),
            40,
        )],
    );
    let (_, first, _) = copies(&e, "event:ufc:600061400", "").await;
    assert_eq!(first["searched"]["cached"], false);
    let sent = search_queries(&e);
    assert!((1..=3).contains(&sent), "{sent}");
    // Fresh, fresh again, with a part, and failing: every one is the first answer with its age.
    e.mock.lock().unwrap().failing_queries = vec!["UFC 334".into()];
    for query in [
        "?fresh=true",
        "?fresh=true",
        "?fresh=true&part=main",
        "?part=prelims",
        "",
    ] {
        let (s, v, body) = copies(&e, "event:ufc:600061400", query).await;
        assert_eq!(s, 200, "{body}");
        assert_eq!(v["searched"]["cached"], true, "{query}: {body}");
        assert!(v["searched"]["age_secs"].is_u64(), "{body}");
    }
    assert_eq!(search_queries(&e), sent, "no query was sent again");
    // A part narrows the cached answer; it does not search for it.
    let (_, v, _) = copies(&e, "event:ufc:600061400", "?part=prelims").await;
    assert_eq!(v["copies"], json!([]));
    let (_, v, _) = copies(&e, "event:ufc:600061400", "?part=main").await;
    assert_eq!(v["copies"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_search_that_failed_is_not_repeated_within_the_cooldown_and_is_after_it() {
    let (e, _) = ufc_setup().await;
    e.mock.lock().unwrap().failing_queries =
        vec!["UFC 334".into(), "UFC gane hokit".into(), "UFC gane".into()];
    let (_, v, _) = copies(&e, "event:ufc:600061400", "").await;
    assert_eq!(v["searched"]["complete"], false);
    let sent = search_queries(&e);
    for _ in 0..4 {
        let (_, v, _) = copies(&e, "event:ufc:600061400", "?fresh=true").await;
        assert_eq!(v["searched"]["cached"], true);
    }
    assert_eq!(search_queries(&e), sent);
    gate(&e).set_limits(Duration::ZERO, Duration::ZERO, 100);
    let (_, v, _) = copies(&e, "event:ufc:600061400", "").await;
    assert_eq!(v["searched"]["cached"], false);
    assert!(search_queries(&e) > sent);
}

#[tokio::test]
async fn simultaneous_requests_for_one_event_send_one_search() {
    let (e, start) = ufc_setup().await;
    stock(
        &e,
        "UFC 334",
        vec![rel(
            "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-nVa",
            start + time::Duration::hours(30),
            40,
        )],
    );
    let e = std::sync::Arc::new(e);
    let mut tasks = vec![];
    for _ in 0..8 {
        let e = e.clone();
        tasks.push(tokio::spawn(async move {
            copies(&e, "event:ufc:600061400", "?fresh=true").await
        }));
    }
    let mut cached = 0;
    for t in tasks {
        let (s, v, body) = t.await.unwrap();
        assert_eq!(s, 200, "{body}");
        assert_eq!(v["copies"].as_array().unwrap().len(), 1);
        if v["searched"]["cached"] == true {
            cached += 1;
        }
    }
    assert!(search_queries(&e) <= 3, "{}", search_queries(&e));
    assert_eq!(cached, 7, "one searched, the others were answered by it");
}

#[tokio::test]
async fn walking_every_event_cannot_drive_unbounded_traffic() {
    let e = start_exec(ExecOptions::default()).await;
    gate(&e).set_limits(Duration::from_secs(120), Duration::from_secs(900), 4);
    for i in 0..30 {
        insert_event(
            &e,
            &format!("event:ufc:{}", 700 + i),
            &format!("UFC {}: Alpha{i} vs. Beta{i}", 400 + i),
            days_ago(2 + i64::from(i % 5)),
            "played",
            None,
            None,
        )
        .await;
    }
    let (mut searched, mut busy) = (0, 0);
    for i in 0..30 {
        let (s, v, body) = copies(&e, &format!("event:ufc:{}", 700 + i), "?fresh=true").await;
        assert_eq!(s, 200, "{body}");
        if v["note"] == "busy" {
            busy += 1;
            assert_eq!(v["copies"], json!([]));
            assert!(v["searched"]["retry_after_secs"].as_u64().unwrap() <= 61);
        } else {
            searched += 1;
        }
    }
    assert_eq!((searched, busy), (4, 26));
    // Four events' searches: at most three queries each, to each indexer.
    assert!(search_queries(&e) <= 4 * 3, "{}", search_queries(&e));
}

#[tokio::test]
async fn a_giant_title_and_a_long_result_list_are_answered_in_bounds() {
    let (e, start) = ufc_setup().await;
    let t = start + time::Duration::hours(30);
    let mut list = vec![rel(
        "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-nVa",
        t,
        40,
    )];
    // (The indexer's own answer is capped at a megabyte, so this is as much as can arrive.)
    for i in 0..100 {
        list.push(rel(&format!("{} {i}", "UFC ".repeat(1_250)), t, 3));
        list.push(rel(
            &format!("UFC 334 Zebra{i} {}", "Quokka ".repeat(500)),
            t,
            3,
        ));
    }
    stock(&e, "UFC 334", list);
    let started = Instant::now();
    let (s, v, body) = copies(&e, "event:ufc:600061400", "").await;
    assert_eq!(s, 200, "{}", &body[..body.len().min(300)]);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(v["copies"].as_array().unwrap().len(), 1);
    assert!(
        body.len() < 20_000,
        "the answer carries no giant title: {}",
        body.len()
    );
}

// ------------------------------------------------------------------ review round 2

#[tokio::test]
async fn a_corrected_schedule_changes_what_a_kept_search_is_judged_to_be() {
    let e = start_exec(ExecOptions::default()).await;
    let start = days_ago(2).replace_hour(8).unwrap();
    insert_event(
        &e,
        "event:a-league-men:401",
        "Melbourne City v Western Sydney Wanderers",
        start,
        "played",
        Some(("1", "Melbourne City")),
        Some(("2", "Western Sydney Wanderers")),
    )
    .await;
    league_known(
        &e,
        "a-league-men",
        &[
            ("1", "Melbourne City"),
            ("2", "Western Sydney Wanderers"),
            ("3", "Sydney FC"),
        ],
        0,
    )
    .await;
    let d = start.date();
    let ymd = format!("{:04} {:02} {:02}", d.year(), u8::from(d.month()), d.day());
    stock(
        &e,
        &format!("A-League {ymd}"),
        vec![rel(
            &format!(
                "A-League {ymd} Melbourne City Vs Western Sydney Wanderers 1080p HDTV H264-DARKSPORT"
            ),
            start + time::Duration::hours(10),
            20,
        )],
    );
    let (_, v, body) = copies(&e, "event:a-league-men:401", "").await;
    assert!(v["copies"][0]["caution"].is_null(), "{body}");
    // The schedule is corrected: the opponent is Sydney FC. The next answer is the same search
    // (nothing is asked of the indexers) judged again, and the old recommendation is gone.
    let sent = search_queries(&e);
    e.state
        .store
        .with(|c| {
            c.execute(
                "UPDATE sport_events SET away_id = '3', away_name = 'Sydney FC' WHERE id = 'event:a-league-men:401'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, v, body) = copies(&e, "event:a-league-men:401", "").await;
    assert_eq!(v["searched"]["cached"], true, "{body}");
    assert_eq!(search_queries(&e), sent);
    assert_eq!(v["copies"], json!([]), "{body}");
}

#[tokio::test]
async fn two_cards_of_one_name_a_week_apart_are_seen_by_each_other() {
    let e = start_exec(ExecOptions::default()).await;
    let a = days_ago(10);
    let b = a + time::Duration::days(7);
    insert_event(
        &e,
        "event:ufc:701",
        "UFC: Gane vs. Hokit",
        a,
        "played",
        None,
        None,
    )
    .await;
    insert_event(
        &e,
        "event:ufc:702",
        "UFC: Gane vs. Hokit",
        b,
        "played",
        None,
        None,
    )
    .await;
    league_known(&e, "ufc", &[], 0).await;
    // Published after both: it fits both windows.
    let late = b + time::Duration::hours(30);
    for id in ["event:ufc:701", "event:ufc:702"] {
        stock(
            &e,
            "UFC gane hokit",
            vec![rel(
                "UFC Gane vs Hokit Main Card 1080p WEB-DL-DARKSPORT",
                late,
                40,
            )],
        );
        gate(&e).set_limits(Duration::ZERO, Duration::ZERO, 100);
        let (s, v, body) = copies(&e, id, "").await;
        assert_eq!(s, 200, "{body}");
        let list = v["copies"].as_array().unwrap();
        if list.is_empty() {
            // (the earlier card's window has closed by then)
            assert_eq!(id, "event:ufc:701", "{body}");
            continue;
        }
        assert!(!list[0]["caution"].is_null(), "{id}: {body}");
        assert!(!list[0]["caution"].is_null());
    }
}

#[tokio::test]
async fn the_indexer_shown_is_the_configured_name_never_the_releases_own_text() {
    let (e, start) = ufc_setup().await;
    let t = start + time::Duration::hours(30);
    let mut hostile = rel(
        "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-nVa",
        t,
        40,
    );
    hostile["indexer"] = json!("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567");
    stock(&e, "UFC 334", vec![hostile]);
    let (s, v, body) = copies(&e, "event:ufc:600061400", "").await;
    assert_eq!(s, 200, "{body}");
    assert!(!body.contains("magnet:"), "{body}");
    assert!(!body.contains("btih"), "{body}");
    let list = v["copies"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{body}");
    // The name the person configured for the native indexer, found by its id.
    assert_eq!(list[0]["indexer"], "The Pirate Bay", "{body}");
    assert_ne!(
        list[0]["indexer"].as_str().unwrap_or(""),
        "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"
    );
}
