//! Sport, phase D, review round 1: the library roots, the link tables, what a payload may say,
//! recovery from a crash, file names, and the Plex section. Loopback stand-ins only.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};
use std::path::PathBuf;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const EVENT: &str = "event:ufc:600061400";
const CLEAN: &str = "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-DARKSPORT";

fn rfc(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap()
}

fn days_ago(n: i64) -> OffsetDateTime {
    (OffsetDateTime::now_utc() - time::Duration::days(n))
        .replace_minute(0)
        .unwrap()
        .replace_second(0)
        .unwrap()
        .replace_nanosecond(0)
        .unwrap()
}

async fn insert_event(e: &Exec, id: &str, name: &str, start: OffsetDateTime) {
    let league = id.split(':').nth(1).unwrap().to_string();
    let (id, name, at) = (id.to_string(), name.to_string(), rfc(start));
    e.state
        .store
        .with(move |c| {
            c.execute(
                "INSERT INTO sport_events (id, league, season, starts_at_utc, ends_at_utc, name, short_name, home_id, home_name, away_id, away_name, round, venue, state, first_seen_utc, updated_utc)
                 VALUES (?1, ?2, NULL, ?3, NULL, ?4, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'played', ?3, ?3)",
                rusqlite::params![id, league, at, name],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn setup_with(o: ExecOptions) -> (Exec, OffsetDateTime) {
    let e = start_exec(o).await;
    let start = days_ago(2);
    insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start).await;
    (e, start)
}

async fn setup() -> (Exec, OffsetDateTime) {
    setup_with(ExecOptions::default()).await
}

fn sport_root(e: &Exec) -> PathBuf {
    // (the harness's movies folder is lower case, so the default sport folder is too)
    e.movies.parent().unwrap().join("sport")
}

fn files_under(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for ent in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = ent.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

struct Listed {
    hash: String,
    id: String,
}

/// Lists a release (its indexer title `title`) backed by a torrent holding `files`, with the
/// torrent's `name`; with `bare_magnet` the release carries only `magnet:?xt=...` (no display
/// name), so nothing about the payload is known until it is downloaded.
async fn list(
    e: &Exec,
    start: OffsetDateTime,
    title: &str,
    name: &str,
    files: Vec<(String, Vec<u8>)>,
    bare_magnet: bool,
) -> Listed {
    let (hash, mut r) = e.publish(title, name, files, 1.5, false);
    r["publishDate"] = json!(rfc(start + time::Duration::hours(30)));
    if bare_magnet {
        r["downloadUrl"] = Value::Null;
        r["magnetUrl"] = json!(format!("magnet:?xt=urn:btih:{hash}"));
    }
    e.search_results("UFC 334", vec![r]);
    e.search_results("UFC gane hokit", vec![]);
    let (s, v, body) = e
        .json(e.get(&format!("/api/v1/sport/event/{EVENT}/copies?fresh=true")))
        .await;
    assert_eq!(s, 200, "{body}");
    let c = v["copies"]
        .as_array()
        .and_then(|a| a.first())
        .unwrap_or_else(|| panic!("no copy listed: {body}"));
    Listed {
        hash,
        id: c["id"].as_str().unwrap().to_string(),
    }
}

async fn pick(e: &Exec, id: &str) -> (u16, Value, String) {
    e.json(
        e.post(&format!("/api/v1/sport/event/{EVENT}/get"))
            .json(&json!({ "id": id, "confirm": true })),
    )
    .await
}

/// Picks, downloads, imports. Returns the grab id.
async fn run(e: &Exec, l: &Listed) -> String {
    let (s, v, body) = pick(e, &l.id).await;
    assert_eq!(s, 201, "{body}");
    let grab = v["grab"]["id"].as_str().unwrap().to_string();
    e.tick().await;
    e.complete(&l.hash);
    e.tick().await;
    e.tick().await;
    grab
}

async fn sport_files(e: &Exec) -> i64 {
    e.state
        .store
        .with(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM sport_files WHERE present = 1",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
}

// ------------------------------------------------------------------------------------- D2

fn link(target: &str, media: &str, ref_: &str) -> arr::downloads::ReleaseLink {
    arr::downloads::ReleaseLink {
        release_ref: ref_.into(),
        target: target.parse().unwrap(),
        media_id: media.parse().unwrap(),
        info_hash: Some("0123456789abcdef0123456789abcdef01234567".into()),
        magnet: None,
        download_url: Some("http://x.invalid/dl".into()),
        title: "t".into(),
        size: Some(1),
        indexer_id: Some(1),
        quality: arr::quality::Quality::Webdl1080,
        revision: Default::default(),
        full_season: false,
        covers: vec![target.parse().unwrap()],
        replaces: vec![],
        supersedes: vec![],
        interactive: true,
    }
}

#[tokio::test]
async fn an_event_link_never_answers_for_a_title_and_a_title_link_never_for_an_event() {
    let (e, _start) = setup().await;
    let r = "rel_aaaaaaaaaaaaaaaaaaaaaaaa";
    let now = OffsetDateTime::now_utc();
    let ev = format!("{EVENT}:main");
    e.state
        .store
        .with({
            let ev = ev.clone();
            move |c| arr::downloads::save_links(c, &[link(&ev, &ev, r)], now)
        })
        .await
        .unwrap();
    let load = |target: &'static str| {
        let (st, r) = (e.state.clone(), r.to_string());
        async move {
            st.store
                .with(move |c| {
                    arr::downloads::load_link(c, &r, target.parse::<arr::ids::MediaId>().ok())
                })
                .await
                .unwrap()
        }
    };
    // Only the event link exists: no movie, series or episode asks find it (the fallback to
    // "another target's row" used to).
    for t in [
        "movie:tmdb:603",
        "series:tvdb:999002",
        "episode:tvdb:999002:1:1",
    ] {
        assert!(load(t).await.is_none(), "{t} found an event's link");
    }
    assert!(load("event:ufc:600061400:main").await.is_some());
    // And the other way: a title link is not found by an event's ask.
    let r2 = "rel_bbbbbbbbbbbbbbbbbbbbbbbb";
    e.state
        .store
        .with(move |c| {
            arr::downloads::save_links(c, &[link("movie:tmdb:603", "movie:tmdb:603", r2)], now)
        })
        .await
        .unwrap();
    let st = e.state.clone();
    let found = st
        .store
        .with(move |c| arr::downloads::load_link(c, r2, Some(ev.parse().unwrap())))
        .await
        .unwrap();
    assert!(found.is_none(), "a movie link answered an event ask");
    // The same title's own fallback still works (a season's link found by an episode ask).
    let r3 = "rel_cccccccccccccccccccccccc";
    e.state
        .store
        .with(move |c| {
            arr::downloads::save_links(
                c,
                &[link("series:tvdb:999002", "series:tvdb:999002", r3)],
                now,
            )
        })
        .await
        .unwrap();
    let st = e.state.clone();
    let found = st
        .store
        .with(move |c| {
            arr::downloads::load_link(c, r3, Some("episode:tvdb:999002:1:1".parse().unwrap()))
        })
        .await
        .unwrap();
    assert!(found.is_some());
}

// ------------------------------------------------------------------------------------- D7

#[tokio::test]
async fn a_listing_older_than_a_week_cannot_be_picked_even_before_the_prune() {
    let (e, start) = setup().await;
    let l = list(
        &e,
        start,
        CLEAN,
        CLEAN,
        vec![(format!("{CLEAN}.mkv"), mkv(3))],
        false,
    )
    .await;
    // Seven days and a minute since it was listed (nothing pruned yet).
    let old = rfc(OffsetDateTime::now_utc() - time::Duration::days(7) - time::Duration::minutes(1));
    let o2 = old.clone();
    e.state
        .store
        .with(move |c| {
            c.execute("UPDATE release_links SET seen_at = ?1", [&o2])?;
            c.execute("UPDATE sport_picks SET seen_utc = ?1", [&o2])?;
            Ok(())
        })
        .await
        .unwrap();
    let (s, v, body) = pick(&e, &l.id).await;
    assert_eq!(s, 404, "{body}");
    assert_eq!(v["error"]["code"], "release_not_found");
    // Only the link row is old: still not found. Only the pick row is old: still not found.
    let (fresh, o3) = (rfc(OffsetDateTime::now_utc()), old.clone());
    e.state
        .store
        .with(move |c| {
            c.execute("UPDATE release_links SET seen_at = ?1", [&fresh])?;
            Ok(())
        })
        .await
        .unwrap();
    let (s, _, _) = pick(&e, &l.id).await;
    assert_eq!(s, 404);
    let fresh = rfc(OffsetDateTime::now_utc());
    e.state
        .store
        .with(move |c| {
            c.execute("UPDATE sport_picks SET seen_utc = ?1", [&fresh])?;
            c.execute("UPDATE release_links SET seen_at = ?1", [&o3])?;
            Ok(())
        })
        .await
        .unwrap();
    let (s, _, _) = pick(&e, &l.id).await;
    assert_eq!(s, 404);
    // Listed again: fresh, and it can be picked.
    let l2 = list(
        &e,
        start,
        CLEAN,
        CLEAN,
        vec![(format!("{CLEAN}.mkv"), mkv(3))],
        false,
    )
    .await;
    assert_eq!(l2.id, l.id);
    assert_eq!(pick(&e, &l2.id).await.0, 201);
}

// ------------------------------------------------------------------------------------- D3

const HOSTILE: [&str; 3] = ["Gane-wins", "wins-by-KO", "Gane.wins"];

/// Everything a client can read about the grab and the event, as one string.
async fn client_view(e: &Exec, grab: &str) -> String {
    let mut all = String::new();
    for path in [
        "/api/v1/history".to_string(),
        format!("/api/v1/queue/{grab}"),
        "/api/v1/queue".to_string(),
        "/api/v1/attention".to_string(),
        "/api/v1/feed".to_string(),
        "/api/v1/blocklist".to_string(),
        format!("/api/v1/sport/event/{EVENT}/copies"),
        format!("/api/v1/status/{EVENT}"),
    ] {
        let (_, _, body) = e.json(e.get(&path)).await;
        all.push_str(&body);
    }
    let (_, feed, _) = e.json(e.get("/api/v1/feed")).await;
    for i in arr::push::items_from_feed(&feed) {
        all.push_str(&i.body);
    }
    // The raw rows a view is built from: the history events and the grab's messages.
    let g = e.grab_row(grab);
    all.push_str(&format!("{:?}{:?}", g.last_error, g.cleanup_detail));
    let rows: Vec<String> = e
        .state
        .store
        .with(|c| {
            let mut s = c.prepare("SELECT message FROM events UNION ALL SELECT COALESCE(error,'') FROM import_journal UNION ALL SELECT title FROM blocklist")?;
            Ok(s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    all.push_str(&rows.join("\n"));
    all
}

fn no_hostile(view: &str, what: &str) {
    for h in HOSTILE {
        if let Some(i) = view.find(h) {
            let from = view[..i].char_indices().rev().nth(120).map_or(0, |x| x.0);
            let to = (i + 120).min(view.len());
            let to = (to..=view.len())
                .find(|j| view.is_char_boundary(*j))
                .unwrap_or(view.len());
            panic!("{what}: {h} reached a client, near: {}", &view[from..to]);
        }
    }
    assert!(
        !view.contains("by KO") && !view.contains("wins"),
        "{what}: {view}"
    );
}

#[tokio::test]
async fn a_hostile_file_name_or_torrent_name_reaches_no_client_text() {
    let hostile_mkv = "Gane-wins-by-KO.mkv".to_string();
    // The happy path: a playable video called by the result.
    {
        let (e, start) = setup().await;
        let l = list(
            &e,
            start,
            CLEAN,
            CLEAN,
            vec![(hostile_mkv.clone(), mkv(3))],
            false,
        )
        .await;
        let grab = run(&e, &l).await;
        assert_eq!(e.grab_row(&grab).state, "imported");
        let view = client_view(&e, &grab).await;
        no_hostile(&view, "imported");
        assert!(view.contains("Gane vs. Hokit"), "{view}");
        // The history says what was filed, from the event.
        let (_, h, text) = e.json(e.get("/api/v1/history")).await;
        assert!(text.contains("the video was put in place"), "{h}");
    }
    // Failures and blocks: a folder and a file named by the result, in a torrent so named.
    type Files = Vec<(String, Vec<u8>)>;
    let cases: Vec<(&str, Files)> = vec![
        // not a video the probe can read
        (
            "garbage",
            vec![(
                "Gane.wins.by.KO/Gane-wins-by-KO.mkv".into(),
                vec![0x1a, 0x45, 0xdf, 0xa3, 1, 2, 3, 4],
            )],
        ),
        // an executable beside the video
        (
            "unsafe",
            vec![
                ("Gane.wins.by.KO/Gane-wins-by-KO.mkv".into(), mkv(3)),
                ("Gane.wins.by.KO/Gane-wins-by-KO.exe".into(), b"MZ".to_vec()),
            ],
        ),
        // two videos
        (
            "two",
            vec![
                ("Gane.wins.by.KO/Gane-wins-by-KO.mkv".into(), mkv(3)),
                ("Gane.wins.by.KO/Gane-wins-by-KO.2.mkv".into(), mkv(4)),
            ],
        ),
    ];
    for (what, files) in cases {
        let (e, start) = setup().await;
        let l = list(&e, start, CLEAN, "Gane.wins.by.KO", files, true).await;
        let grab = run(&e, &l).await;
        let g = e.grab_row(&grab);
        assert!(
            matches!(g.state.as_str(), "failed" | "import_blocked"),
            "{what}: {} {:?}",
            g.state,
            g.last_error
        );
        no_hostile(&client_view(&e, &grab).await, what);
    }
    // A file already at the destination, and a failure to probe a copy are blocked texts too.
    {
        let (e, start) = setup().await;
        let l = list(
            &e,
            start,
            CLEAN,
            CLEAN,
            vec![(hostile_mkv.clone(), mkv(3))],
            false,
        )
        .await;
        let date = format!(
            "{:04}-{:02}-{:02}",
            start.year(),
            u8::from(start.month()),
            start.day()
        );
        put(
            &sport_root(&e),
            &format!(
                "UFC/{}/UFC - {date} - UFC 334 - Gane vs. Hokit - Main card.mkv",
                start.year()
            ),
            b"mine",
        );
        let grab = run(&e, &l).await;
        // (filed beside it under the event's tag: nothing is overwritten)
        assert_eq!(e.grab_row(&grab).state, "imported");
        no_hostile(&client_view(&e, &grab).await, "conflict");
    }
}

// --------------------------------------------------------------------------- D4, D5, D6

async fn failed_with(e: &Exec, grab: &str) -> (String, String) {
    let g = e.grab_row(grab);
    (g.state.clone(), g.failure.clone().unwrap_or_default())
}

#[tokio::test]
async fn magnet_metadata_naming_another_event_fails_and_is_blocklisted() {
    // The magnet has no display name, so the search and the enqueue could not tell.
    for (what, file) in [
        (
            "torrent name",
            "UFC.333.Oliveira.vs.Lopes.Main.Card.1080p.WEB-DL.H264-DARKSPORT.mkv",
        ),
        (
            "a path in it",
            "UFC.333.Oliveira.vs.Lopes.Main.Card.1080p.WEB-DL.H264-DARKSPORT/video.mkv",
        ),
    ] {
        let (e, start) = setup().await;
        let l = list(
            &e,
            start,
            CLEAN,
            "UFC.333.Oliveira.vs.Lopes.Main.Card.1080p.WEB-DL.H264-DARKSPORT",
            vec![(file.to_string(), mkv(3))],
            true,
        )
        .await;
        let grab = run(&e, &l).await;
        assert_eq!(
            failed_with(&e, &grab).await,
            ("failed".into(), "wrong_payload".into()),
            "{what}: {:?}",
            e.grab_row(&grab).last_error
        );
        assert_eq!(sport_files(&e).await, 0, "{what}");
        assert!(files_under(&sport_root(&e)).is_empty(), "{what}");
        // Blocklisted for this event: not offered again.
        let (_, v, body) = e
            .json(e.get(&format!("/api/v1/sport/event/{EVENT}/copies?fresh=true")))
            .await;
        assert!(v["copies"].as_array().unwrap().is_empty(), "{what}: {body}");
    }
    // Terse names (no evidence either way) are tolerated and filed.
    let (e, start) = setup().await;
    let l = list(
        &e,
        start,
        CLEAN,
        "video",
        vec![("video.mkv".into(), mkv(3))],
        true,
    )
    .await;
    let grab = run(&e, &l).await;
    assert_eq!(
        e.grab_row(&grab).state,
        "imported",
        "{:?}",
        e.grab_row(&grab).last_error
    );
}

#[tokio::test]
async fn a_payload_naming_another_part_is_not_filed_as_this_one() {
    let (e, start) = setup().await;
    let name = "UFC.334.Gane.vs.Hokit.Prelims.1080p.WEB-DL.H264-DARKSPORT";
    let l = list(
        &e,
        start,
        CLEAN,
        name,
        vec![(format!("{name}.mkv"), mkv(3))],
        true,
    )
    .await;
    let grab = run(&e, &l).await;
    let g = e.grab_row(&grab);
    assert_eq!(
        (g.state.as_str(), g.failure.as_deref()),
        ("failed", Some("wrong_payload")),
        "{:?}",
        g.last_error
    );
    assert!(
        arr::sport::reason::render_stored(&g.last_error.unwrap()).contains("not the main card")
    );
    assert_eq!(sport_files(&e).await, 0);
}

#[tokio::test]
async fn a_payload_that_is_a_show_about_the_event_is_not_filed() {
    for name in [
        "UFC 334 Countdown 1080p WEBRip h264-TJ [TJET]",
        "UFC 334 Gane vs Hokit Main Event 1080p WEB-DL-NX",
        "UFC 334 Embedded Vlog Series Episode 2 1080p WEBRip h264-TJ [TJET]",
    ] {
        let (e, start) = setup().await;
        let l = list(
            &e,
            start,
            CLEAN,
            name,
            vec![(format!("{name}.mkv"), mkv(3))],
            true,
        )
        .await;
        let grab = run(&e, &l).await;
        assert_eq!(
            failed_with(&e, &grab).await,
            ("failed".into(), "wrong_payload".into()),
            "{name}: {:?}",
            e.grab_row(&grab).last_error
        );
        assert_eq!(sport_files(&e).await, 0, "{name}");
    }
}

#[tokio::test]
async fn every_refusal_is_classified_once() {
    use arr::sport::get::{Evidence, evidence_of};
    use arr::sport::matcher::RefusalCode::*;
    // (the match inside `evidence_of` is exhaustive: a new code does not build until it is put on
    // one side; this pins the sides)
    for c in [
        NotAnEvent,
        OtherLeague,
        AmbiguousLeague,
        QualifiedLeague,
        OtherCompetition,
        NotTheEvent,
        WrongDate,
        TeamMismatch,
        RoundMismatch,
        WrongSeason,
        SeriesMismatch,
        NumberMismatch,
        FightersMismatch,
        TournamentMismatch,
        AnotherPart,
        SingleBout,
    ] {
        assert_eq!(evidence_of(c), Evidence::Contradiction, "{c:?}");
    }
    for c in [
        NoLeague,
        DateUnreadable,
        PublishedBeforeEvent,
        NoTeams,
        NoDate,
        AmbiguousTeam,
        NoEvidence,
        OutsideWindow,
        TitleTooLong,
    ] {
        assert_eq!(evidence_of(c), Evidence::Missing, "{c:?}");
    }
}

// ------------------------------------------------------------------------------------- D8

async fn crash_then_recover(point: &'static str) -> (Exec, String, Listed, OffsetDateTime) {
    let (mut e, start) = setup().await;
    let l = list(
        &e,
        start,
        CLEAN,
        CLEAN,
        vec![("video.mkv".to_string(), mkv(3))],
        false,
    )
    .await;
    let (s, v, body) = pick(&e, &l.id).await;
    assert_eq!(s, 201, "{body}");
    let grab = v["grab"]["id"].as_str().unwrap().to_string();
    e.tick().await;
    e.complete(&l.hash);
    // The process "dies" at `point` of the grab's import.
    arr::ingest::CRASH_AT
        .lock()
        .unwrap()
        .push((grab.clone(), point));
    e.tick().await;
    arr::ingest::CRASH_AT
        .lock()
        .unwrap()
        .retain(|(g, _)| *g != grab);
    assert_eq!(
        e.grab_row(&grab).state,
        "importing",
        "{point}: left mid-import"
    );
    e.restart().await;
    (e, grab, l, start)
}

#[tokio::test]
async fn a_crash_at_every_line_of_the_import_recovers_and_the_next_import_finishes() {
    for point in ["after_stage", "after_link", "after_published"] {
        let (e, grab, _l, _) = crash_then_recover(point).await;
        // Rolled back: nothing filed, no row, no staged leftover, the download intact.
        assert_eq!(sport_files(&e).await, 0, "{point}");
        let left: Vec<_> = files_under(&sport_root(&e))
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(left.is_empty(), "{point}: {left:?}");
        assert_eq!(e.grab_row(&grab).state, "completed", "{point}");
        // The next import files it once.
        e.tick().await;
        e.tick().await;
        let g = e.grab_row(&grab);
        assert_eq!(g.state, "imported", "{point}: {:?}", g.last_error);
        assert_eq!(sport_files(&e).await, 1, "{point}");
        assert_eq!(files_under(&sport_root(&e)).len(), 1, "{point}");
    }
}

#[tokio::test]
async fn a_crash_after_the_commit_is_finished_by_the_next_import_without_filing_again() {
    for point in ["after_commit", "before_apply"] {
        let (e, grab, _l, _) = crash_then_recover(point).await;
        // The file and its row are there; the grab was only left `importing`, now `completed`.
        assert_eq!(sport_files(&e).await, 1, "{point}");
        let before = files_under(&sport_root(&e));
        assert_eq!(before.len(), 1, "{point}");
        let bytes = std::fs::read(&before[0]).unwrap();
        assert_eq!(e.grab_row(&grab).state, "completed", "{point}");
        e.tick().await;
        e.tick().await;
        let g = e.grab_row(&grab);
        // Not blocked on its own file: imported, cleaned up and handed to Plex.
        assert_eq!(g.state, "imported", "{point}: {:?}", g.last_error);
        assert_ne!(g.cleanup, "none", "{point}: clean-up never ran");
        assert_eq!(files_under(&sport_root(&e)), before, "{point}");
        assert_eq!(std::fs::read(&before[0]).unwrap(), bytes);
        assert_eq!(sport_files(&e).await, 1);
        let refreshes: i64 = e
            .state
            .store
            .with(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM commands WHERE name = 'plex_refresh'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(refreshes, 1, "{point}: the Plex refresh was queued");
    }
}

#[tokio::test]
async fn a_file_filed_by_another_grab_still_blocks() {
    // (the recognition is of this grab's own file, nothing wider)
    let (e, start) = setup().await;
    let l = list(
        &e,
        start,
        CLEAN,
        CLEAN,
        vec![("video.mkv".to_string(), mkv(3))],
        false,
    )
    .await;
    let grab = run(&e, &l).await;
    assert_eq!(e.grab_row(&grab).state, "imported");
    let l2 = list(
        &e,
        start,
        &format!("{CLEAN}.REPACK"),
        &format!("{CLEAN}.REPACK"),
        vec![("video2.mkv".to_string(), mkv(4))],
        false,
    )
    .await;
    let (s, v, _) = pick(&e, &l2.id).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("already_have"))
    );
}

// ------------------------------------------------------------------------------------- D9

fn ev(id: &str, name: &str, season: Option<&str>) -> arr::sport::data::EventRow {
    arr::sport::data::EventRow {
        id: id.into(),
        league: "ufc".into(),
        season: season.map(str::to_string),
        starts_at: "2026-11-07T22:00:00Z".into(),
        ends_at: None,
        name: name.into(),
        short_name: None,
        home_id: None,
        home_name: None,
        away_id: None,
        away_name: None,
        round: None,
        venue: None,
        state: "played".into(),
    }
}

#[test]
fn file_names_fit_keep_their_parts_apart_and_are_safe_everywhere() {
    use arr::sport::Part;
    use arr::sport::get::{component, destination, destination_among};
    // Many multi-byte letters: 180 characters used to be far over 255 bytes.
    for ch in ["é", "世", "𝔘"] {
        let long = format!("UFC 1: {}", ch.repeat(300));
        let e = ev("event:ufc:1", &long, Some("2026"));
        let mut names = vec![];
        for part in [Part::Main, Part::Prelims, Part::EarlyPrelims, Part::Whole] {
            let (dir, name) = destination(&e, part, "mkv", true);
            assert!(name.len() <= 255, "{ch}: {} bytes", name.len());
            assert!(dir.components().all(|c| c.as_os_str().len() <= 255));
            if part != Part::Whole {
                assert!(name.contains(&part.label()), "{ch}: {name}");
            }
            assert!(name.ends_with(".mkv"));
            names.push(name);
        }
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "parts collided: {names:?}");
        // Stable: the same event, the same name.
        assert_eq!(destination(&e, Part::Main, "mkv", true).1, names[0]);
    }
    // Two different long names that agree on a long prefix get different names.
    let base = "x".repeat(300);
    let a = destination(
        &ev("event:ufc:1", &format!("{base}A"), None),
        Part::Main,
        "mkv",
        true,
    )
    .1;
    let b = destination(
        &ev("event:ufc:2", &format!("{base}B"), None),
        Part::Main,
        "mkv",
        true,
    )
    .1;
    assert_ne!(a, b);
    // Trailing dots and spaces, reserved names, control characters, separators, empty.
    assert_eq!(component("Name. . ", true), "Name");
    assert_eq!(component("CON", true), "_CON");
    assert_eq!(component("aux.txt", true), "_aux.txt");
    assert_eq!(component("LPT1", true), "_LPT1");
    assert_eq!(component("a\u{0}b\u{7}c", true), "abc");
    assert_eq!(component("a/b\\c", true), "a+b+c");
    assert_eq!(component("...", true), "_");
    assert_eq!(component("", true), "_");
    let e = ev("event:ufc:3", "NUL", Some("2026"));
    let (dir, name) = destination(&e, Part::Whole, "mkv", true);
    assert!(!name.to_lowercase().starts_with("nul"), "{name}");
    assert_eq!(dir.to_string_lossy(), "UFC/2026");
    // A name another event holds, ignoring case, is not reused.
    let one = ev("event:ufc:5", "Same Name", Some("2026"));
    let two = ev("event:ufc:6", "SAME NAME", Some("2026"));
    let (_, first) = destination(&one, Part::Main, "mkv", true);
    let (_, second) = destination_among(&two, Part::Main, "mkv", true, &[first.to_lowercase()]);
    assert_ne!(first.to_lowercase(), second.to_lowercase());
    assert!(second.contains('['), "{second}");
    // An event on its own keeps the plain name.
    let (_, alone) = destination_among(&two, Part::Main, "mkv", true, &[]);
    assert!(!alone.contains('['), "{alone}");
}

// ------------------------------------------------------------------------------- D10, D11

/// A server whose Plex sees the whole library parent as `/media_library`, as the real install
/// does; the Sport folder is inferred from `movies_path`.
async fn inferred() -> (Exec, OffsetDateTime) {
    let e = start_exec(ExecOptions {
        plex_extra: "movies_path = \"/media_library/movies\"\n".into(),
        ..Default::default()
    })
    .await;
    let start = days_ago(2);
    insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start).await;
    (e, start)
}

async fn file_one(e: &Exec, start: OffsetDateTime) -> String {
    let l = list(
        e,
        start,
        CLEAN,
        CLEAN,
        vec![("video.mkv".to_string(), mkv(3))],
        false,
    )
    .await;
    let grab = run(e, &l).await;
    assert_eq!(
        e.grab_row(&grab).state,
        "imported",
        "{:?}",
        e.grab_row(&grab).last_error
    );
    e.tick().await;
    grab
}

fn scans(e: &Exec) -> Vec<String> {
    e.mock.lock().unwrap().plex_scans.clone()
}

#[tokio::test]
async fn an_inferred_sport_folder_is_created_only_after_plex_shows_the_filed_video() {
    // Plex sees the library parent: the inferred `/media_library/sport` is right.
    let (e, start) = inferred().await;
    e.mock.lock().unwrap().plex_visible = vec![(
        "/media_library".into(),
        e.movies.parent().unwrap().to_path_buf(),
    )];
    file_one(&e, start).await;
    let m = e.mock.lock().unwrap();
    assert_eq!(m.plex_created.len(), 1, "{:?}", m.plex_created);
    let q: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(m.plex_created[0].as_bytes())
            .into_owned()
            .collect();
    assert_eq!(q["location"], "/media_library/sport", "{q:?}");
    // The browse request: the folder the video is in, base64 of its path, folders and files.
    use base64::Engine;
    let want = format!("/media_library/sport/UFC/{}", days_ago(2).year());
    assert!(
        m.plex_browsed
            .iter()
            .any(|(p, q)| *p == want && q.contains("includeFiles=1")),
        "{:?}",
        m.plex_browsed
    );
    let _ = base64::engine::general_purpose::STANDARD.encode(want.as_bytes());
    drop(m);
    assert!(
        scans(&e).iter().any(|s| s.starts_with("10?")),
        "{:?}",
        scans(&e)
    );
}

#[tokio::test]
async fn a_docker_style_mapping_that_plex_cannot_see_creates_nothing() {
    // Local Movies is `/movies` and Sport is `/sports` in Plex: the inferred `/Sport` (beside
    // `/movies`) is somewhere Plex cannot list.
    let (e, start) = inferred().await;
    e.mock.lock().unwrap().plex_visible = vec![
        ("/movies".into(), e.movies.clone()),
        ("/sports".into(), sport_root(&e)),
    ];
    file_one(&e, start).await;
    let (created, browsed) = {
        let m = e.mock.lock().unwrap();
        (m.plex_created.clone(), m.plex_browsed.len())
    };
    assert!(created.is_empty(), "{created:?}");
    assert!(browsed > 0, "Plex was asked");
    let results: Vec<String> = e
        .state
        .store
        .with(|c| {
            let mut s =
                c.prepare("SELECT COALESCE(result, '') FROM commands WHERE name = 'plex_refresh'")?;
            Ok(s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert!(
        results
            .iter()
            .any(|r| r.contains("Saved in") && r.contains("plex.sport_path")),
        "{results:?}"
    );
    // With the mapping stated, the same setup works.
    let e2 = start_exec(ExecOptions {
        plex_extra: "movies_path = \"/movies\"\nsport_path = \"/sports\"\n".into(),
        ..Default::default()
    })
    .await;
    let start = days_ago(2);
    insert_event(&e2, EVENT, "UFC 334: Gane vs. Hokit", start).await;
    file_one(&e2, start).await;
    let created = e2.mock.lock().unwrap().plex_created.clone();
    assert_eq!(created.len(), 1);
    assert!(created[0].contains("location=%2Fsports"), "{created:?}");
}

#[tokio::test]
async fn the_default_sport_folder_follows_the_case_of_the_movies_folder() {
    use arr::config::sport_folder_name;
    assert_eq!(sport_folder_name("movies"), "sport");
    assert_eq!(sport_folder_name("Movies"), "Sport");
    let (e, _) = setup().await;
    assert_eq!(
        e.state.cfg.library.as_ref().unwrap().sport_root().unwrap(),
        sport_root(&e)
    );
}

#[tokio::test]
async fn a_remembered_section_is_reused_only_for_the_same_server_and_folder() {
    let (e, start) = inferred().await;
    e.mock.lock().unwrap().plex_visible = vec![(
        "/media_library".into(),
        e.movies.parent().unwrap().to_path_buf(),
    )];
    file_one(&e, start).await;
    assert_eq!(e.mock.lock().unwrap().plex_created.len(), 1);
    // Another Plex server where key 10 is the *movie* library: the remembered key must not be
    // trusted, and nothing is scanned in a section that is not the sport folder.
    {
        let mut m = e.mock.lock().unwrap();
        m.plex_machine = "machine-B".into();
        m.plex_sections = vec![json!({
            "key": "10", "title": "Movies", "type": "movie",
            "Location": [{ "path": "/media_library/movies" }]
        })];
        m.plex_scans.clear();
        m.plex_created.clear();
    }
    insert_event(
        &e,
        "event:ufc:600061401",
        "UFC 335: Oliveira vs. Lopes",
        days_ago(3),
    )
    .await;
    queue_refresh(&e, "event:ufc:600061400:main").await;
    e.tick().await;
    {
        let m = e.mock.lock().unwrap();
        assert!(
            !m.plex_scans.iter().any(|s| s.starts_with("10?")),
            "scanned the movie section: {:?}",
            m.plex_scans
        );
        assert_eq!(
            m.plex_created.len(),
            1,
            "a Sport library made on the new server"
        );
    }
    // The same server again, the section's folder changed under the key: not reused.
    {
        let mut m = e.mock.lock().unwrap();
        m.plex_scans.clear();
        m.plex_created.clear();
        for s in m.plex_sections.iter_mut() {
            if s["title"] == "Sport" {
                s["Location"] = json!([{ "path": "/media_library/movies" }]);
            }
        }
    }
    queue_refresh(&e, "event:ufc:600061400:main").await;
    e.tick().await;
    let m = e.mock.lock().unwrap();
    let sport_key = m
        .plex_sections
        .iter()
        .find(|s| s["title"] == "Sport")
        .map(|s| s["key"].as_str().unwrap().to_string())
        .unwrap();
    assert!(
        !m.plex_scans
            .iter()
            .any(|s| s.starts_with(&format!("{sport_key}?"))),
        "scanned a section that no longer holds the sport folder: {:?}",
        m.plex_scans
    );
}

async fn queue_refresh(e: &Exec, media: &str) {
    let (m, now) = (media.to_string(), rfc(OffsetDateTime::now_utc()));
    e.state
        .store
        .with(move |c| {
            arr::worker::queue_command(c, "plex_refresh", Some(m.parse().unwrap()), &now)
        })
        .await
        .unwrap();
}

// ------------------------------------------------------------------------------------- D1

#[tokio::test]
async fn the_sport_root_is_in_the_removal_roots() {
    let (e, _) = setup().await;
    let roots = e.state.cfg.library.as_ref().unwrap().roots();
    assert!(roots.contains(&sport_root(&e)), "{roots:?}");
    assert!(roots.contains(&e.movies) && roots.contains(&e.tv));
}
