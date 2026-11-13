//! Sport, phase D, review round 2: what a payload's own names claim, hostile text from a client
//! or engine, the Plex section rules, and where the sport root really is.
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

async fn setup_on(stack: Stack) -> (Exec, OffsetDateTime) {
    let e = start_exec(ExecOptions {
        stack,
        ..Default::default()
    })
    .await;
    let start = days_ago(2);
    insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start).await;
    (e, start)
}

fn sport_root(e: &Exec) -> PathBuf {
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

fn complete(e: &Exec, hash: &str) {
    match e.stack {
        Stack::Managed => e.complete(hash),
        Stack::Legacy => qb::complete(&e.qb, hash),
    }
}

/// A release listed as the Main card whose magnet carries no display name: nothing about the
/// payload is known until it is downloaded. `name` is the torrent's own name (for a payload of
/// several files).
async fn list_bare(
    e: &Exec,
    start: OffsetDateTime,
    name: &str,
    files: Vec<(String, Vec<u8>)>,
) -> (String, String) {
    let (hash, mut r) = e.publish(CLEAN, name, files, 1.5, false);
    r["publishDate"] = json!(rfc(start + time::Duration::hours(30)));
    r["downloadUrl"] = Value::Null;
    r["magnetUrl"] = json!(format!("magnet:?xt=urn:btih:{hash}"));
    e.search_results("UFC 334", vec![r]);
    e.search_results("UFC gane hokit", vec![]);
    let (s, v, body) = e
        .json(e.get(&format!("/api/v1/sport/event/{EVENT}/copies?fresh=true")))
        .await;
    assert_eq!(s, 200, "{body}");
    let id = v["copies"][0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no copy: {body}"))
        .to_string();
    (hash, id)
}

async fn pick_and_download(e: &Exec, hash: &str, id: &str) -> String {
    let (s, v, body) = e
        .json(
            e.post(&format!("/api/v1/sport/event/{EVENT}/get"))
                .json(&json!({ "id": id, "confirm": true })),
        )
        .await;
    assert_eq!(s, 201, "{body}");
    let grab = v["grab"]["id"].as_str().unwrap().to_string();
    e.tick().await;
    complete(e, hash);
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

/// What happens to a payload of `files` (torrent name `name`) picked as the Main card.
async fn outcome(
    name: &str,
    files: Vec<(&str, Vec<u8>)>,
) -> (String, Option<String>, Option<String>, i64) {
    let (e, start) = setup_on(Stack::Managed).await;
    insert_event(
        &e,
        "event:ufc:600061401",
        "UFC 335: Oliveira vs. Lopes",
        start + time::Duration::days(7),
    )
    .await;
    let files: Vec<(String, Vec<u8>)> =
        files.into_iter().map(|(n, b)| (n.to_string(), b)).collect();
    let (hash, id) = list_bare(&e, start, name, files).await;
    let grab = pick_and_download(&e, &hash, &id).await;
    let g = e.grab_row(&grab);
    (
        g.state.clone(),
        g.failure.clone(),
        g.last_error.clone(),
        sport_files(&e).await,
    )
}

fn nfo() -> Vec<u8> {
    b"info".to_vec()
}

// ------------------------------------------------------------------ D4, D5, D6, D12

#[tokio::test]
async fn a_correctly_named_torrent_with_the_wrong_day_in_its_file_is_not_filed() {
    // (the name carries the right event; the file says it is January)
    let (state, failure, why, filed) =
        outcome(CLEAN, vec![("2026-01-01.mkv", mkv(3)), ("info.nfo", nfo())]).await;
    assert_eq!(
        (state.as_str(), failure.as_deref()),
        ("failed", Some("wrong_payload")),
        "{why:?}"
    );
    assert_eq!(filed, 0);
    // The right day is fine.
    let day = days_ago(2).date();
    let right = format!(
        "{:04}-{:02}-{:02}.mkv",
        day.year(),
        u8::from(day.month()),
        day.day()
    );
    let (state, _, why, filed) =
        outcome(CLEAN, vec![(right.as_str(), mkv(3)), ("info.nfo", nfo())]).await;
    assert_eq!((state.as_str(), filed), ("imported", 1), "{why:?}");
}

#[tokio::test]
async fn the_part_a_payload_says_must_be_the_part_that_was_picked() {
    for (what, name, file, want_failed) in [
        // a full-event copy picked as the main card
        (
            "full event name",
            "UFC.334.Gane.vs.Hokit.Full.Event.1080p.WEB-DL",
            "video.mkv",
            true,
        ),
        // a correctly named torrent whose file is the prelims
        ("prelims file", CLEAN, "Prelims.mkv", true),
        // right part in the file, terse
        ("right part only", CLEAN, "Main.Card.mkv", false),
        // terse
        ("terse", CLEAN, "video.mkv", false),
    ] {
        let (state, failure, why, filed) =
            outcome(name, vec![(file, mkv(3)), ("info.nfo", nfo())]).await;
        if want_failed {
            assert_eq!(
                (state.as_str(), failure.as_deref(), filed),
                ("failed", Some("wrong_payload"), 0),
                "{what}: {why:?}"
            );
        } else {
            assert_eq!((state.as_str(), filed), ("imported", 1), "{what}: {why:?}");
        }
    }
}

#[tokio::test]
async fn a_folder_that_says_prelims_over_a_file_that_says_main_card_is_refused() {
    let (state, failure, why, filed) = outcome(
        CLEAN,
        vec![("Prelims/Main.Card.mkv", mkv(3)), ("info.nfo", nfo())],
    )
    .await;
    assert_eq!(
        (state.as_str(), failure.as_deref(), filed),
        ("failed", Some("wrong_payload"), 0),
        "{why:?}"
    );
}

#[tokio::test]
async fn a_show_about_the_event_inside_a_correctly_named_torrent_is_not_filed() {
    for file in [
        "Countdown.mkv",
        "UFC.334.Embedded.Vlog.Series.Episode.2.mkv",
        "UFC.334.Gane.vs.Hokit.Main.Event.mkv",
    ] {
        let (state, failure, why, filed) =
            outcome(CLEAN, vec![(file, mkv(3)), ("info.nfo", nfo())]).await;
        assert_eq!(
            (state.as_str(), failure.as_deref(), filed),
            ("failed", Some("wrong_payload"), 0),
            "{file}: {why:?}"
        );
    }
}

#[tokio::test]
async fn a_file_that_carries_another_cards_number_is_not_filed() {
    let (state, failure, why, filed) = outcome(
        CLEAN,
        vec![
            ("UFC.333.Oliveira.vs.Lopes.mkv", mkv(3)),
            ("info.nfo", nfo()),
        ],
    )
    .await;
    assert_eq!(
        (state.as_str(), failure.as_deref(), filed),
        ("failed", Some("wrong_payload"), 0),
        "{why:?}"
    );
}

#[tokio::test]
async fn a_second_video_anywhere_in_the_payload_refuses_the_cut() {
    for (what, extra) in [
        (
            "a hidden folder",
            ".hidden/UFC.333.Oliveira.vs.Lopes.Main.Card.mkv",
        ),
        ("a hidden folder, neutral", ".hidden/video2.mkv"),
        ("a sample folder", "Sample/video2.mkv"),
        ("an extras folder", "Extras/video2.mkv"),
        ("a nested folder", "a/b/c/video2.mkv"),
    ] {
        let (state, failure, why, filed) =
            outcome(CLEAN, vec![("video.mkv", mkv(3)), (extra, mkv(4))]).await;
        // (a hidden file naming another card is a contradiction; the others are a second video)
        assert!(
            matches!(
                (state.as_str(), failure.as_deref()),
                ("failed", Some("wrong_payload")) | ("import_blocked", _)
            ),
            "{what}: {state} {failure:?} {why:?}"
        );
        assert_eq!(filed, 0, "{what}");
    }
    // The quoted case: the hidden file's name is judged although it is never filed.
    let (_, failure, _, _) = outcome(
        CLEAN,
        vec![
            ("video.mkv", mkv(3)),
            (".hidden/UFC.333.Oliveira.vs.Lopes.Main.Card.mkv", mkv(4)),
        ],
    )
    .await;
    assert_eq!(failure.as_deref(), Some("wrong_payload"));
}

#[tokio::test]
async fn the_claims_of_a_name_are_checked_against_the_event_and_the_part() {
    use arr::sport::Part;
    use arr::sport::get::{PayloadCheck, judge_payload, payload_claims};
    let (e, _) = setup_on(Stack::Managed).await;
    let ev = e
        .state
        .store
        .with(|c| arr::sport::data::event(c, EVENT))
        .await
        .unwrap()
        .unwrap();
    let claims = |n: &str, p: Part| payload_claims(&ev, &Default::default(), n, p);
    // Nothing claimed: tolerated.
    assert!(claims("video", Part::Main).is_none());
    assert!(claims("1080p WEB-DL", Part::Main).is_none());
    // Whole and a part, in both directions.
    assert!(claims("UFC 334 Full Event", Part::Main).is_some());
    assert!(claims("UFC 334 Main Card", Part::Whole).is_some());
    assert!(claims("UFC 334 Main Card", Part::Main).is_none());
    assert!(claims("Prelims", Part::Prelims).is_none());
    assert!(claims("Prelims", Part::Main).is_some());
    // Other card, other league, a show, one fight.
    assert!(claims("UFC 333", Part::Main).is_some());
    assert!(claims("NBA 2026 Lakers", Part::Main).is_some());
    assert!(claims("Countdown", Part::Main).is_some());
    assert!(claims("UFC 334 Main Event", Part::Main).is_some());
    // The same, over a whole payload: the torrent's name, folders and every video.
    let files: Vec<String> = vec!["a/video.mkv".into()];
    assert_eq!(
        judge_payload(
            &ev,
            &Default::default(),
            Some("UFC 334"),
            &files,
            Part::Main
        ),
        PayloadCheck::Fine
    );
    assert!(matches!(
        judge_payload(
            &ev,
            &Default::default(),
            Some("UFC 334"),
            &["Prelims/video.mkv".to_string()],
            Part::Main
        ),
        PayloadCheck::Contradicts(_)
    ));
    assert_eq!(
        judge_payload(
            &ev,
            &Default::default(),
            None,
            &["x.mkv".into(), "y.mkv".into()],
            Part::Main
        ),
        PayloadCheck::SeveralVideos
    );
    // Every refusal code is classified (the match is exhaustive); non-video files are not counted.
    assert_eq!(
        judge_payload(
            &ev,
            &Default::default(),
            None,
            &["x.mkv".into(), "x.nfo".into(), "x.srt".into()],
            Part::Main
        ),
        PayloadCheck::Fine
    );
}

// ------------------------------------------------------------------------------------- D3

const HOSTILE: [&str; 4] = ["Gane-wins", "wins-by-KO", "Gane.wins", "../Gane"];

async fn everything_a_client_reads(e: &Exec, grab: &str) -> String {
    let mut all = String::new();
    for path in [
        "/api/v1/history".to_string(),
        format!("/api/v1/queue/{grab}"),
        "/api/v1/queue".to_string(),
        "/api/v1/attention".to_string(),
        "/api/v1/feed".to_string(),
        "/api/v1/blocklist".to_string(),
    ] {
        let (_, _, body) = e.json(e.get(&path)).await;
        all.push_str(&body);
    }
    let (_, feed, _) = e.json(e.get("/api/v1/feed")).await;
    for i in arr::push::items_from_feed(&feed) {
        all.push_str(&i.body);
    }
    all
}

fn assert_clean(view: &str, what: &str) {
    for h in HOSTILE {
        assert!(!view.contains(h), "{what}: {h} reached a client");
    }
}

#[tokio::test]
async fn text_from_a_client_or_engine_never_names_the_payload_for_a_sport_grab() {
    // The engine refuses the torrent and says its name (the real embedded engine does for a name
    // like `../Gane-wins-by-KO`); the add fails as invalid.
    let (e, start) = setup_on(Stack::Managed).await;
    let (hash, id) = list_bare(&e, start, CLEAN, vec![("video.mkv".into(), mkv(3))]).await;
    e.fake.lock().unwrap().add_invalid_text = Some(
        "unsafe torrent name \"../Gane-wins-by-KO\"; unsafe file path \"Gane.wins/wins-by-KO.mkv\""
            .into(),
    );
    let (s, v, body) = e
        .json(
            e.post(&format!("/api/v1/sport/event/{EVENT}/get"))
                .json(&json!({ "id": id, "confirm": true })),
        )
        .await;
    assert_eq!(s, 201, "{body}");
    let grab = v["grab"]["id"].as_str().unwrap().to_string();
    e.tick().await;
    e.tick().await;
    let g = e.grab_row(&grab);
    assert_eq!(g.state, "failed", "{:?}", g.last_error);
    assert_eq!(
        g.failure.as_deref(),
        Some("invalid_torrent"),
        "the code is kept"
    );
    assert_clean(&everything_a_client_reads(&e, &grab).await, "engine error");
    // The reason is typed: the person reads that the client refused the torrent.
    let (_, _, body) = e.json(e.get(&format!("/api/v1/queue/{grab}"))).await;
    assert!(
        body.contains("the download client refused this torrent"),
        "{body}"
    );
    let rows: String = e
        .state
        .store
        .with(|c| {
            let mut s = c.prepare("SELECT message FROM events")?;
            Ok(s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .join("\n"))
        })
        .await
        .unwrap();
    assert_clean(&rows, "history rows");
    let _ = hash;
    // A movie's message is unchanged: the same engine text still reaches it (nothing here
    // touches the movie path; the sport scrub is by the grab's media id).
}

#[tokio::test]
async fn hostile_file_and_torrent_names_stay_out_of_every_client_text_on_both_clients() {
    for stack in [Stack::Managed, Stack::Legacy] {
        let (e, start) = setup_on(stack).await;
        let files = vec![(
            "Gane.wins.by.KO/Gane-wins-by-KO.mkv".to_string(),
            vec![0x1a, 0x45, 0xdf, 0xa3, 1, 2, 3, 4],
        )];
        let (hash, id) = list_bare(&e, start, "Gane.wins.by.KO", files).await;
        let grab = pick_and_download(&e, &hash, &id).await;
        let g = e.grab_row(&grab);
        assert!(
            matches!(g.state.as_str(), "failed" | "import_blocked"),
            "{}",
            g.state
        );
        assert_clean(
            &everything_a_client_reads(&e, &grab).await,
            "hostile payload",
        );
    }
}

// ------------------------------------------------------------------------------------- D1

#[cfg(unix)]
#[test]
fn the_sport_root_is_compared_by_where_it_really_is() {
    let tmp = tempfile::tempdir().unwrap();
    let (dl, alias) = (tmp.path().join("downloads"), tmp.path().join("alias"));
    std::fs::create_dir(&dl).unwrap();
    std::os::unix::fs::symlink(&dl, &alias).unwrap();
    // The folder does not exist yet: the existing ancestor is resolved.
    let sport = alias.join("Sport");
    assert_eq!(arr::paths::overlap(&sport, &dl), Ok(true));
    assert_eq!(arr::paths::overlap(&dl, &sport), Ok(true));
    assert_eq!(
        arr::paths::resolved(&sport),
        Ok(std::fs::canonicalize(&dl).unwrap().join("Sport"))
    );
    let apart = tmp.path().join("library/sport");
    assert_eq!(arr::paths::overlap(&apart, &dl), Ok(false));
    // Only a genuinely missing component is tolerated: a link that points nowhere, a file where a
    // folder should be, and a folder that cannot be entered all fail closed.
    let dangling = tmp.path().join("dangling");
    std::os::unix::fs::symlink(tmp.path().join("nowhere"), &dangling).unwrap();
    assert!(arr::paths::resolved(&dangling.join("Sport")).is_err());
    assert!(arr::paths::overlap(&dangling.join("Sport"), &dl).is_err());
    let file = tmp.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    assert!(arr::paths::resolved(&file.join("Sport")).is_err());
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = tmp.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::create_dir(locked.join("inner")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = arr::paths::resolved(&locked.join("inner/Sport"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // (running as root can enter it: then there is nothing to refuse)
        if let Err(why) = denied {
            assert!(why.contains("not going on"), "{why}");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_sport_root_reached_through_a_link_into_the_downloads_is_refused_at_import() {
    // The configuration is fine when read; the folder becomes a link into the downloads after
    // (so the import checks again, by where the root really is).
    let (e, start) = setup_on(Stack::Managed).await;
    let root = sport_root(&e);
    let (hash, id) = list_bare(&e, start, CLEAN, vec![("video.mkv".into(), mkv(3))]).await;
    let downloads = e.fake.lock().unwrap().download_dir.clone();
    std::os::unix::fs::symlink(&downloads, &root).unwrap();
    let grab = pick_and_download(&e, &hash, &id).await;
    let g = e.grab_row(&grab);
    assert_eq!(g.state, "import_blocked", "{:?}", g.last_error);
    assert!(arr::sport::reason::render_stored(&g.last_error.unwrap()).contains("Sport folder"));
    assert_eq!(sport_files(&e).await, 0);
    assert!(
        files_under(&downloads)
            .iter()
            .all(|p| !p.to_string_lossy().contains("UFC -"))
    );
}

// ------------------------------------------------------------------------------------ D11, D13

fn plex_cfg(extra: &str) -> ExecOptions {
    ExecOptions {
        plex_extra: extra.to_string(),
        ..Default::default()
    }
}

async fn file_one(e: &Exec, start: OffsetDateTime) {
    let (hash, id) = list_bare(e, start, CLEAN, vec![("video.mkv".into(), mkv(3))]).await;
    let grab = pick_and_download(e, &hash, &id).await;
    assert_eq!(
        e.grab_row(&grab).state,
        "imported",
        "{:?}",
        e.grab_row(&grab).last_error
    );
    e.tick().await;
}

async fn plex_notes(e: &Exec) -> Vec<String> {
    e.state
        .store
        .with(|c| {
            let mut s =
                c.prepare("SELECT COALESCE(result, '') FROM commands WHERE name = 'plex_refresh'")?;
            Ok(s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn browse_verification_accepts_only_the_file_at_the_expected_path() {
    // Plex lists a file of the right name at another folder, and a folder of the right name.
    let e = start_exec(plex_cfg("movies_path = \"/media_library/movies\"\n")).await;
    let start = days_ago(2);
    insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start).await;
    // The fake answers every browse the same way, whatever the path: wrong files only.
    e.mock.lock().unwrap().plex_browse_fixed = Some(json!({ "MediaContainer": {
        "File": [{ "key": "k", "path": format!("/other/sport/UFC/{}/x.mkv", start.year()), "title": "x.mkv" }],
        "Path": [{ "key": "k", "path": "/media_library/sport/UFC/2026/anything", "title": "anything" }]
    }}));
    file_one(&e, start).await;
    assert!(e.mock.lock().unwrap().plex_created.is_empty());
    assert!(
        plex_notes(&e)
            .await
            .iter()
            .any(|n| n.contains("plex.sport_path"))
    );
}

#[tokio::test]
async fn browse_verification_does_not_take_a_directory_or_a_title_for_the_file() {
    let e = start_exec(plex_cfg("movies_path = \"/media_library/movies\"\n")).await;
    let start = days_ago(2);
    insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start).await;
    // The filed video's own name, as a directory entry and as a title with another path.
    let name_dir = format!("/media_library/sport/UFC/{}", start.year());
    e.mock.lock().unwrap().plex_browse_fixed = Some(json!({ "MediaContainer": {
        "Path": [{ "key": "k", "path": format!("{name_dir}/UFC - x.mkv"), "title": "UFC - x.mkv" }],
        "File": [{ "key": "k", "path": "/elsewhere/file.mkv", "title": "UFC - x.mkv" }]
    }}));
    file_one(&e, start).await;
    assert!(e.mock.lock().unwrap().plex_created.is_empty());
}

fn section(key: &str, kind: &str, agent: &str, loc: &str) -> Value {
    json!({ "key": key, "title": format!("S{key}"), "type": kind, "agent": agent, "Location": [{ "path": loc }] })
}

#[tokio::test]
async fn a_stated_sport_section_must_be_a_plain_video_library_that_holds_the_folder() {
    let none = "com.plexapp.agents.none";
    for (what, sections, extra, ok) in [
        // a good one
        (
            "good",
            vec![section("7", "movie", none, "/data/sport")],
            "sport_section = 7\n",
            true,
        ),
        // any library that holds the folder is a deliberate choice
        (
            "another kind",
            vec![section("7", "show", none, "/data/sport")],
            "sport_section = 7\n",
            true,
        ),
        (
            "another agent",
            vec![section("7", "movie", "tv.plex.agents.movie", "/data/sport")],
            "sport_section = 7\n",
            true,
        ),
        (
            "wrong folder",
            vec![section("7", "movie", none, "/data/movies")],
            "sport_section = 7\n",
            false,
        ),
        (
            "absent",
            vec![section("8", "movie", none, "/data/sport")],
            "sport_section = 7\n",
            false,
        ),
    ] {
        let e = start_exec(plex_cfg(&format!("sport_path = \"/data/sport\"\n{extra}"))).await;
        let start = days_ago(2);
        insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start).await;
        e.mock.lock().unwrap().plex_sections = sections;
        file_one(&e, start).await;
        let scans = e.mock.lock().unwrap().plex_scans.clone();
        let notes = plex_notes(&e).await;
        if ok {
            assert!(
                scans.iter().any(|s| s.starts_with("7?")),
                "{what}: {scans:?} {notes:?}"
            );
        } else {
            assert!(scans.is_empty(), "{what}: {scans:?}");
            assert!(
                notes.iter().any(|n| n.contains("plex.sport_section (7)")),
                "{what}: {notes:?}"
            );
            // (nothing falls back to making or choosing another library)
            assert!(e.mock.lock().unwrap().plex_created.is_empty(), "{what}");
        }
    }
}

#[tokio::test]
async fn the_sport_section_is_never_the_movie_or_tv_one() {
    // A section of the configured movie library (key 1 in the harness) that holds the Sport
    // folder is not used, and since a library already holds the folder nothing is made over it.
    let e = start_exec(plex_cfg("sport_path = \"/data/sport\"\n")).await;
    e.mock.lock().unwrap().plex_sections = vec![section(
        "1",
        "movie",
        "com.plexapp.agents.none",
        "/data/sport",
    )];
    let start = days_ago(2);
    insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start).await;
    file_one(&e, start).await;
    let (created, scans) = {
        let m = e.mock.lock().unwrap();
        (m.plex_created.len(), m.plex_scans.clone())
    };
    assert_eq!(
        created, 0,
        "a second library over the same folder: {scans:?}"
    );
    assert!(!scans.iter().any(|s| s.starts_with("1?")), "{scans:?}");
    assert!(
        plex_notes(&e)
            .await
            .iter()
            .any(|n| n.contains("not a plain video library"))
    );
}

#[tokio::test]
async fn an_existing_library_over_the_folder_decides_whether_one_is_made() {
    let none = "com.plexapp.agents.none";
    let two = |a: &str, b: &str| json!({ "key": "9", "title": "Mixed", "type": "movie", "agent": none, "Location": [{ "path": a }, { "path": b }] });
    for (what, root, sections, makes, scanned, note) in [
        // a verified plain library over exactly the folder: used
        (
            "plain",
            "/data/sport",
            vec![section("9", "movie", none, "/data/sport")],
            false,
            Some("9?"),
            None,
        ),
        (
            "plain, trailing slash",
            "/data/sport",
            vec![section("9", "movie", none, "/data/sport/")],
            false,
            Some("9?"),
            None,
        ),
        // another agent (a real movie-metadata library): left alone, said so
        (
            "movie agent",
            "/data/sport",
            vec![section("9", "movie", "tv.plex.agents.movie", "/data/sport")],
            false,
            None,
            Some("plex.sport_section = 9"),
        ),
        (
            "a show library",
            "/data/sport",
            vec![section("9", "show", none, "/data/sport")],
            false,
            None,
            Some("plex.sport_section = 9"),
        ),
        (
            "also holds other folders",
            "/data/sport",
            vec![two("/data/sport", "/data/other")],
            false,
            None,
            Some("plex.sport_section = 9"),
        ),
        // a library whose folder is above the Sport folder holds it as part of a larger one
        (
            "a library rooted above",
            "/media/Sport",
            vec![section("9", "movie", none, "/media")],
            false,
            None,
            Some("holds it"),
        ),
        // a library with a folder inside it
        (
            "a folder inside",
            "/data/sport",
            vec![section("9", "movie", none, "/data/sport/UFC")],
            false,
            None,
            Some("inside"),
        ),
        // Windows paths compare with both separators, any case, any trailing separator
        (
            "windows, backslashes",
            "D:/Media/Sport",
            vec![section("9", "movie", none, "D:\\Media\\Sport")],
            false,
            Some("9?"),
            None,
        ),
        (
            "windows, case",
            "D:/Media/Sport",
            vec![section("9", "movie", none, "d:\\media\\SPORT\\")],
            false,
            Some("9?"),
            None,
        ),
        (
            "windows, above",
            "D:/Media/Sport",
            vec![section("9", "movie", none, "D:\\Media")],
            false,
            None,
            Some("holds it"),
        ),
        // a library at the filesystem root holds everything
        (
            "a library at the root",
            "/media/Sport",
            vec![section("9", "movie", none, "/")],
            false,
            None,
            Some("holds it"),
        ),
        // paths that differ only in case: Arr cannot tell, so it fails closed (a macOS volume)
        (
            "case differs",
            "/Volumes/media/sport",
            vec![section("9", "movie", none, "/Volumes/Media/Sport")],
            false,
            None,
            Some("holds it"),
        ),
        (
            "case differs above",
            "/Volumes/media/sport",
            vec![section("9", "movie", none, "/Volumes/Media")],
            false,
            None,
            Some("holds it"),
        ),
        // another folder with a similar name is not the folder
        (
            "a sibling with the same prefix",
            "/data/sport",
            vec![section("9", "movie", none, "/data/sport2")],
            true,
            Some("11?"),
            None,
        ),
        // nothing holds it: one is made
        (
            "none holds it",
            "/data/sport",
            vec![section("9", "movie", none, "/data/elsewhere")],
            true,
            Some("11?"),
            None,
        ),
    ] {
        let e = start_exec(plex_cfg(&format!("sport_path = \"{root}\"\n"))).await;
        e.mock.lock().unwrap().plex_sections = sections;
        let start = days_ago(2);
        insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start).await;
        file_one(&e, start).await;
        let (created, scans) = {
            let m = e.mock.lock().unwrap();
            (m.plex_created.len(), m.plex_scans.clone())
        };
        let notes = plex_notes(&e).await;
        assert_eq!(
            created == 1,
            makes,
            "{what}: created {created} {scans:?} {notes:?}"
        );
        match scanned {
            Some(p) => assert!(scans.iter().any(|s| s.starts_with(p)), "{what}: {scans:?}"),
            None => assert!(scans.is_empty(), "{what}: {scans:?}"),
        }
        if let Some(n) = note {
            assert!(
                notes.iter().any(|x| x.contains(n)
                    && x.contains("Mixed")
                        == (n == "plex.sport_section = 9" && what.starts_with("also"))
                    || x.contains(n)),
                "{what}: {notes:?}"
            );
        }
    }
}

// ------------------------------------------------------------------------ round 3

#[cfg(unix)]
#[tokio::test]
async fn an_ffprobe_message_about_a_copy_never_reaches_a_client_or_the_journal_route() {
    use std::os::unix::fs::PermissionsExt;
    // An ffprobe that passes the downloaded file and fails the staged copy, saying a file name.
    let tmp = tempfile::tempdir().unwrap();
    let real = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v ffprobe")
        .output()
        .unwrap();
    let real = String::from_utf8_lossy(&real.stdout).trim().to_string();
    let script = tmp.path().join("ffprobe");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \"$*\" in *.um-import-*) echo 'Gane-wins-by-KO.mkv: corrupt input' >&2; exit 1;; esac\nexec {real} \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let e = start_exec(ExecOptions {
        library_extra: format!("ffprobe = '{}'\n", script.display()),
        tv_extra: "copy_using_hardlinks = false\n".into(),
        ..Default::default()
    })
    .await;
    let start = days_ago(2);
    insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start).await;
    let (hash, id) = list_bare(&e, start, CLEAN, vec![("video.mkv".into(), mkv(3))]).await;
    let grab = pick_and_download(&e, &hash, &id).await;
    let g = e.grab_row(&grab);
    assert_eq!(g.state, "import_blocked", "{:?}", g.last_error);
    // The journal's own row (what the sink keeps) and the route that returns it.
    let stored: Vec<String> = e
        .state
        .store
        .with(|c| {
            let mut s = c.prepare("SELECT COALESCE(error, '') FROM import_journal")?;
            Ok(s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert!(!stored.is_empty());
    assert!(
        stored
            .iter()
            .all(|t| !t.contains("Gane-wins") && !t.contains("corrupt")),
        "{stored:?}"
    );
    let (_, _, body) = e.json(e.get(&format!("/api/v1/queue/{grab}"))).await;
    assert_clean(&body, "queue item");
    assert!(!body.contains("corrupt"), "{body}");
    // The read side alone is enough: a row that holds foreign text is returned as the generic
    // sentence.
    e.state
        .store
        .with(|c| {
            c.execute("UPDATE import_journal SET error = 'Gane-wins-by-KO.mkv: bad'", [])?;
            c.execute("UPDATE grabs SET last_error = 'Gane-wins-by-KO.mkv: bad', cleanup_detail = 'Gane-wins-by-KO.mkv'", [])?;
            c.execute("UPDATE events SET message = 'Gane-wins-by-KO.mkv: bad' WHERE grab_id IS NOT NULL", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, _, body) = e.json(e.get(&format!("/api/v1/queue/{grab}"))).await;
    assert_clean(&body, "read side");
    let (_, _, q) = e.json(e.get("/api/v1/queue")).await;
    assert_clean(&q, "queue list");
}

#[test]
fn what_a_payload_name_claims_is_found_anywhere_in_it() {
    use arr::sport::Part;
    let ev =
        |name: &str, starts: &str, ends: Option<&str>, league: &str| arr::sport::data::EventRow {
            id: format!("event:{league}:1"),
            league: league.into(),
            season: None,
            starts_at: starts.into(),
            ends_at: ends.map(str::to_string),
            name: name.into(),
            short_name: None,
            home_id: None,
            home_name: None,
            away_id: None,
            away_name: None,
            round: None,
            venue: None,
            state: "played".into(),
        };
    let ufc = ev(
        "UFC 334: Gane vs. Hokit",
        "2026-05-09T22:00:00Z",
        None,
        "ufc",
    );
    let known = arr::sport::get::Known {
        teams: vec![],
        others: vec![vec!["oliveira".into(), "lopes".into()]],
    };
    let claims = |n: &str, p: Part| arr::sport::get::payload_claims(&ufc, &known, n, p);
    // Contradictions: each of the quoted cases, in dotted names.
    for (what, name, part) in [
        ("another day, compact", "UFC.334.20260101.1080p", Part::Main),
        (
            "another day, compact day first",
            "UFC.334.01012026.1080p",
            Part::Main,
        ),
        ("another day, plain", "2026-01-01", Part::Main),
        ("another day, named month", "1.Jan.2026.1080p", Part::Main),
        (
            "a pair after technical words",
            "1080p.Oliveira.vs.Lopes",
            Part::Main,
        ),
        (
            "two parts in one name",
            "Main.Card.1080p.Prelims",
            Part::Main,
        ),
        (
            "full event for the main card",
            "UFC.334.Full.Event.1080p",
            Part::Main,
        ),
        ("prelims for the main card", "Prelims", Part::Main),
        ("main card for the whole event", "Main.Card", Part::Whole),
        ("season and episode, glued", "UFC.S01E02.1080p", Part::Main),
        (
            "season and episode, dotted",
            "UFC.S01.E02.1080p",
            Part::Main,
        ),
        (
            "season and episode, spaced",
            "UFC S01 E02 1080p",
            Part::Main,
        ),
        ("season and episode, x", "UFC 1x02 1080p", Part::Main),
        ("episode word", "UFC Episode 2", Part::Main),
        ("ep word", "UFC Ep 2", Part::Main),
        ("another card", "UFC.333.1080p", Part::Main),
        ("a show", "Countdown", Part::Main),
        ("one fight", "Main.Event", Part::Main),
        ("another competition", "NBA.Finals", Part::Main),
        ("another sport", "Basketball.1080p", Part::Main),
    ] {
        assert!(claims(name, part).is_some(), "{what}: {name}");
    }
    // Nothing claimed, or claims that hold.
    for (name, part) in [
        ("video", Part::Main),
        ("1080p", Part::Main),
        ("1080p.WEB-DL.H264-DARKSPORT", Part::Main),
        (
            "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-DARKSPORT",
            Part::Main,
        ),
        ("UFC.334.Prelims.1080p.WEB-DL.H264.Fight-BB", Part::Prelims),
        (
            "UFC.334.Early.Prelims.720p.WEB-DL.H264.Fight-BB",
            Part::EarlyPrelims,
        ),
        ("UFC 334 Full Event 2026 05 09", Part::Whole),
        ("2026-05-09", Part::Main),
        ("20260509", Part::Main),
        ("Gane.vs.Hokit", Part::Main),
        ("Hokit.v.Gane.1080p", Part::Main),
        ("Fight.Night.1080p", Part::Main),
    ] {
        assert!(
            claims(name, part).is_none(),
            "{name}: {:?}",
            claims(name, part)
        );
    }
    // Golf: a neutral name claims nothing; a different major does; a round must be the one picked.
    let golf = ev(
        "KPMG Women's PGA Championship",
        "2026-06-25T12:00:00Z",
        Some("2026-06-28T21:00:00Z"),
        "lpga-tour",
    );
    let g = |n: &str, p: Part| arr::sport::get::payload_claims(&golf, &Default::default(), n, p);
    assert!(g("video", Part::Round(3)).is_none());
    assert!(g("1080p", Part::Whole).is_none());
    assert!(g("Round.3.1080p", Part::Round(3)).is_none());
    assert!(g("Round.2.1080p", Part::Round(3)).is_some());
    assert!(g("The.Masters.Round.3", Part::Round(3)).is_some());
    assert!(
        g(
            "KPMG.Womens.PGA.Championship.Round.3.20260627",
            Part::Round(3)
        )
        .is_none()
    );
    assert!(g("20260110", Part::Round(3)).is_some());
}

#[test]
fn an_ordinary_payload_is_accepted_and_a_second_video_of_any_kind_is_not() {
    use arr::sport::Part;
    use arr::sport::get::{PayloadCheck, judge_payload};
    let ev = arr::sport::data::EventRow {
        id: "event:ufc:600061400".into(),
        league: "ufc".into(),
        season: None,
        starts_at: "2026-05-09T22:00:00Z".into(),
        ends_at: None,
        name: "UFC 334: Gane vs. Hokit".into(),
        short_name: None,
        home_id: None,
        home_name: None,
        away_id: None,
        away_name: None,
        round: None,
        venue: None,
        state: "played".into(),
    };
    let ok = |name: Option<&str>, files: &[&str], part: Part| {
        let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
        judge_payload(&ev, &Default::default(), name, &files, part)
    };
    let main = "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264-DARKSPORT";
    // A typical scene folder, its file and its extras.
    assert_eq!(
        ok(
            Some(main),
            &[
                &format!("{main}/{main}.mkv"),
                &format!("{main}/{main}.nfo"),
                &format!("{main}/{main}.sfv"),
                &format!("{main}/{main}.srt"),
                &format!("{main}/Proof/proof.jpg"),
                &format!("{main}/notes.txt"),
            ],
            Part::Main
        ),
        PayloadCheck::Fine
    );
    // Prelims and early prelims.
    for (n, p) in [
        ("UFC.334.Prelims.1080p.WEB-DL.H264.Fight-BB", Part::Prelims),
        (
            "UFC.334.Early.Prelims.720p.WEB-DL.H264.Fight-BB",
            Part::EarlyPrelims,
        ),
    ] {
        assert_eq!(
            ok(Some(n), &[&format!("{n}/{n}.mkv")], p),
            PayloadCheck::Fine,
            "{n}"
        );
    }
    // A file named only with resolution and group; the torrent name repeated in the file name.
    assert_eq!(
        ok(Some(main), &["1080p-DARKSPORT.mkv", "x.nfo"], Part::Main),
        PayloadCheck::Fine
    );
    assert_eq!(
        ok(Some(main), &[&format!("{main}.mkv")], Part::Main),
        PayloadCheck::Fine
    );
    // A second video, of any kind the code knows.
    for extra in [
        "bonus.flv",
        "Sample/sample.mkv",
        "extras/other.mp4",
        ".hidden/x.mkv",
        "disc.iso",
        "a/b/c/d.avi",
        "clip.divx",
    ] {
        assert_eq!(
            ok(Some(main), &["video.mkv", extra], Part::Main),
            PayloadCheck::SeveralVideos,
            "{extra}"
        );
    }
    // Pictures and text are not videos.
    assert_eq!(
        ok(
            Some(main),
            &[
                "video.mkv",
                "a.jpg",
                "b.png",
                "c.nfo",
                "d.txt",
                "e.srt",
                "f.sfv"
            ],
            Part::Main
        ),
        PayloadCheck::Fine
    );
}

#[tokio::test]
async fn the_quoted_payloads_are_not_filed_and_a_neutral_golf_round_is() {
    // D12: a second video of another container.
    let (state, failure, _, filed) =
        outcome(CLEAN, vec![("video.mkv", mkv(3)), ("bonus.flv", mkv(4))]).await;
    assert_eq!(
        (state.as_str(), failure.as_deref(), filed),
        ("import_blocked", None, 0)
    );
    // D4, D5, D6 as whole payloads.
    for (what, file) in [
        ("pair after technical words", "1080p.Oliveira.vs.Lopes.mkv"),
        ("two parts", "Main.Card.1080p.Prelims.mkv"),
        ("an episode", "UFC.S01.E02.1080p.mkv"),
    ] {
        let (state, failure, why, filed) =
            outcome(CLEAN, vec![(file, mkv(3)), ("info.nfo", nfo())]).await;
        assert_eq!(
            (state.as_str(), failure.as_deref(), filed),
            ("failed", Some("wrong_payload"), 0),
            "{what}: {why:?}"
        );
    }
}

// ------------------------------------------------------------------------ round 6

const PLANTED: &str = "Gane-wins-by-KO";

/// Hostile values for a stored field: free text, and records of every shape the decoder must
/// refuse (a hostile value in each position that used to be a string, an unknown variant, extra
/// fields, huge or negative numbers, truncated, nested or repeated prefixes).
fn hostile_values() -> Vec<String> {
    let (r, h) = ("\u{1}sport:", "\u{1}sportev:");
    let p = PLANTED;
    let mut v = vec![
        p.to_string(),
        format!("/srv/dl/{p}.mkv"),
        format!("{r}{{\"k\":\"names_not_event\",\"what\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"names_not_event\",\"claim\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"not_playable\",\"name\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"cannot_use\",\"name\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"wrong_quality\",\"name\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"wrong_part\",\"said\":\"{p}\",\"expected\":\"main\"}}"),
        format!("{r}{{\"k\":\"unreadable\",\"name\":\"{p}\",\"kind\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"unsafe_file\",\"ext\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"file_op\",\"what\":\"{p}\",\"kind\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"cannot_create_folder\",\"path\":\"{p}\",\"kind\":\"other\"}}"),
        format!("{r}{{\"k\":\"sport_folder_unusable\",\"detail\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"taken_back\",\"landed\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"publish_failed\",\"kind\":\"{p}\"}}"),
        format!(
            "{r}{{\"k\":\"implausible\",\"name\":\"{p}\",\"why\":{{\"k\":\"too_small\",\"mib\":1}}}}"
        ),
        format!("{r}{{\"k\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"already_have\",\"extra\":\"{p}\"}}"),
        format!("{r}{{\"k\":\"unsafe_file\",\"ext\":99999999999999999999}}"),
        format!("{r}{{\"k\":\"unsafe_file\",\"ext\":-1}}"),
        format!(
            "{r}{{\"k\":\"no_space\",\"free_mib\":18446744073709551615,\"needed_mib\":1,\"min_mib\":1}}"
        ),
        format!("{r}{{\"k\":\"already_have\""),
        format!("{r}{r}{{\"k\":\"already_have\"}}"),
        format!("{r}{{\"k\":\"names_not_event\",\"claim\":\"{r}\"}}"),
        format!("{p}{r}{{\"k\":\"already_have\"}}"),
        format!("{r}{{\"k\":\"already_have\"}}{p}"),
        format!("{r}{{\"k\":\"already_have\"}}{}", " ".repeat(800)),
        format!(
            "{h}{{\"k\":\"failed\",\"reason\":{{\"k\":\"names_not_event\",\"what\":\"{p}\"}}}}"
        ),
        format!("{h}{{\"k\":\"failed\",\"reason\":\"{p}\"}}"),
        format!("{h}{{\"k\":\"picked\",\"note\":\"{p}\"}}"),
        format!("{h}{{\"k\":\"{p}\"}}"),
        format!("{h}{h}{{\"k\":\"picked\"}}"),
        format!("{h}{{\"k\":\"picked\""),
        format!("{h}{{\"k\":\"failed\",\"reason\":{{\"k\":\"unsafe_file\",\"ext\":250}}}}"),
    ];
    // (a well-formed reason carried in a history record's place, and the reverse)
    v.push(format!("{r}{{\"k\":\"picked\"}}"));
    v.push(format!("{h}{{\"k\":\"already_have\"}}"));
    v
}

#[tokio::test]
async fn a_hostile_row_planted_in_any_table_is_returned_by_no_route() {
    let (e, start) = setup_on(Stack::Managed).await;
    let (hash, id) = list_bare(&e, start, CLEAN, vec![("video.mkv".into(), mkv(3))]).await;
    let grab = pick_and_download(&e, &hash, &id).await;
    assert_eq!(e.grab_row(&grab).state, "imported");
    // A plex_refresh command for the event with a result that names the person's library.
    e.state
        .store
        .with(|c| {
            c.execute(
                "INSERT INTO commands (id, name, media_id, state, created_at, result, attempts) VALUES ('cmd_abcdef0123456789abcdef01', 'plex_refresh', 'event:ufc:600061400:main', 'completed', '2026-01-01T00:00:00Z', json_object('skipped', 'Gane-wins-by-KO library', 'note', 'Gane-wins-by-KO'), 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    e.state
        .store
        .with(|c| {
            c.execute(
                "INSERT INTO commands (id, name, media_id, state, created_at, result, attempts) VALUES ('cmd_abcdef0123456789abcdef02', 'plex_refresh', 'event:ufc:600061400:main', 'queued', '2026-01-02T00:00:00Z', json_object('note', 'Gane-wins-by-KO'), 0)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    for planted in hostile_values() {
        // A hostile value in every table that can hold one: the grab (error, detail, title,
        // client name, and a state that lists it among the things needing attention), the
        // journal, the history (message and data) and the blocklist (title and reason).
        let (g, p) = (grab.clone(), planted.clone());
        e.state
            .store
            .with(move |c| {
                c.execute(
                    "UPDATE grabs SET last_error = ?2, cleanup_detail = ?2, title = ?2, client_name = ?2, state = 'import_blocked', active = 1 WHERE id = ?1",
                    rusqlite::params![g, p],
                )?;
                c.execute("UPDATE import_journal SET error = ?1", [&p])?;
                c.execute("DELETE FROM events WHERE grab_id IS NULL OR message LIKE 'x%'", [])?;
                c.execute(
                    "UPDATE events SET message = ?1, data = json_object('note', ?1) WHERE grab_id IS NOT NULL",
                    [&p],
                )?;
                c.execute(
                    "INSERT INTO events (at, kind, media_id, target, grab_id, message, data) VALUES ('2026-01-01T00:00:00Z', 'download_failed', 'event:ufc:600061400:main', 'event:ufc:600061400:main', NULL, ?1, json_object('note', ?1))",
                    [&p],
                )?;
                c.execute("DELETE FROM blocklist", [])?;
                c.execute(
                    "INSERT INTO blocklist (media_id, release_ref, info_hash, title, reason, grab_id, created_at) VALUES ('event:ufc:600061400:main', 'rel_aaaaaaaaaaaaaaaaaaaaaaaa', NULL, ?1, ?1, NULL, '2026-01-01T00:00:00Z')",
                    [&p],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let mut seen = vec![];
        for path in [
            "/api/v1/history".to_string(),
            "/api/v1/history?media_id=event:ufc:600061400:main".to_string(),
            "/api/v1/queue?all=true".to_string(),
            format!("/api/v1/queue/{grab}"),
            "/api/v1/attention".to_string(),
            "/api/v1/feed".to_string(),
            "/api/v1/blocklist".to_string(),
            "/api/v1/commands".to_string(),
            "/api/v1/commands/cmd_abcdef0123456789abcdef01".to_string(),
            format!("/api/v1/sport/event/{EVENT}/copies"),
        ] {
            let (_, _, body) = e.json(e.get(&path)).await;
            seen.push((path, body));
        }
        // The action responses: each returns the grab, and the pick again says why not.
        for action in ["pause", "resume", "retry", "cancel"] {
            let (_, _, body) = e
                .json(
                    e.post(&format!("/api/v1/queue/{grab}/{action}"))
                        .json(&json!({})),
                )
                .await;
            seen.push((action.to_string(), body));
        }
        // A pending command for the event holding hostile text in its result: the POST that would
        // reuse it (it is refused for an event id) and the GETs that return it.
        let (_, _, body) = e
            .json(
                e.post("/api/v1/commands").json(
                    &json!({ "name": "plex_refresh", "media_id": "event:ufc:600061400:main" }),
                ),
            )
            .await;
        seen.push(("POST /commands (event)".into(), body));
        let (_, _, body) = e
            .json(e.get("/api/v1/commands/cmd_abcdef0123456789abcdef02"))
            .await;
        seen.push(("GET pending command".into(), body));
        let (_, _, body) = e
            .json(e.post("/api/v1/grabs").json(&json!({ "media_id": "event:ufc:600061400:main", "release_ref": "rel_aaaaaaaaaaaaaaaaaaaaaaaa" })))
            .await;
        seen.push(("POST /grabs".into(), body));
        let (_, _, body) = e
            .json(
                e.post(&format!("/api/v1/sport/event/{EVENT}/get"))
                    .json(&json!({ "id": id, "confirm": true })),
            )
            .await;
        seen.push(("event get".into(), body));
        for (path, body) in &seen {
            assert!(!body.contains(PLANTED), "{path} with {planted:?}: {body}");
            assert!(
                !body.contains('\u{1}') && !body.contains("\\u0001"),
                "{path} with {planted:?}: {body}"
            );
        }
        // The push bodies are built from the feed.
        let (_, feed, _) = e.json(e.get("/api/v1/feed")).await;
        for i in arr::push::items_from_feed(&feed) {
            assert!(!i.body.contains(PLANTED), "{}", i.body);
        }
        // Everything an event's history says begins with the stored event's own name.
        let (_, h, _) = e.json(e.get("/api/v1/history")).await;
        for i in h["items"].as_array().unwrap() {
            if i["media_id"]
                .as_str()
                .is_some_and(|m| m.starts_with("event:"))
            {
                assert!(
                    i["message"]
                        .as_str()
                        .unwrap()
                        .starts_with("UFC 334: Gane vs. Hokit"),
                    "{i}"
                );
            }
        }
        // The grab may have been cancelled by the action calls: set it back for the next value.
        let g = grab.clone();
        e.state
            .store
            .with(move |c| {
                c.execute(
                    "UPDATE grabs SET state = 'import_blocked', active = 1 WHERE id = ?1",
                    [&g],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    // The Plex step's result: the only values returned are fixed ones.
    let (_, c, _) = e
        .json(e.get("/api/v1/commands/cmd_abcdef0123456789abcdef01"))
        .await;
    assert_eq!(c["command"]["result"]["refreshed"], false, "{c}");
}

#[tokio::test]
async fn the_history_of_a_sport_download_is_typed_records_rendered_from_the_event() {
    let (e, start) = setup_on(Stack::Managed).await;
    let (hash, id) = list_bare(&e, start, CLEAN, vec![("video.mkv".into(), mkv(3))]).await;
    let grab = pick_and_download(&e, &hash, &id).await;
    let (_, body, _) = e.json(e.get("/api/v1/history")).await;
    let msgs: Vec<String> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["grab_id"] == grab.as_str())
        .map(|i| i["message"].as_str().unwrap().to_string())
        .collect();
    assert!(
        msgs.iter()
            .any(|m| m == "UFC 334: Gane vs. Hokit - Main card: a copy was picked"),
        "{msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m.ends_with("filed in the Sport folder")),
        "{msgs:?}"
    );
    // What is stored is the record, not text.
    let stored: Vec<String> = e
        .state
        .store
        .with(|c| {
            let mut s = c.prepare("SELECT message FROM events WHERE media_id LIKE 'event:%'")?;
            Ok(s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert!(
        !stored.is_empty()
            && stored
                .iter()
                .all(|m| arr::sport::reason::HistoryEvent::decode(m).is_some()),
        "{stored:?}"
    );
}

#[tokio::test]
async fn an_import_time_client_failure_keeps_its_typed_reason() {
    let (e, start) = setup_on(Stack::Managed).await;
    let (hash, id) = list_bare(&e, start, CLEAN, vec![("video.mkv".into(), mkv(3))]).await;
    let (s, v, body) = e
        .json(
            e.post(&format!("/api/v1/sport/event/{EVENT}/get"))
                .json(&json!({ "id": id, "confirm": true })),
        )
        .await;
    assert_eq!(s, 201, "{body}");
    let grab = v["grab"]["id"].as_str().unwrap().to_string();
    e.tick().await;
    complete(&e, &hash);
    e.tick().await;
    // The client goes away just as the import looks at it.
    e.fake.lock().unwrap().down = true;
    e.tick().await;
    e.tick().await;
    let (_, body, _) = e.json(e.get(&format!("/api/v1/queue/{grab}"))).await;
    let text = body["grab"]["last_error"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(
        text.contains("download client") || body["grab"]["state"] == "imported",
        "{text}: {body}"
    );
    assert!(!text.contains("Hit a problem") || text.is_empty(), "{text}");
}

#[tokio::test]
async fn a_stored_command_cannot_be_serialised_for_a_client_without_the_reduction() {
    // `worker::Command` has no `Serialize`; the only serialisable form is `PublicCommand`, which
    // `public()` makes: an event command's result is reduced to fixed values.
    let (e, _) = setup_on(Stack::Managed).await;
    e.state
        .store
        .with(|c| {
            c.execute(
                "INSERT INTO commands (id, name, media_id, state, created_at, result, attempts) VALUES ('cmd_abcdef0123456789abcdef03', 'plex_refresh', 'event:ufc:600061400:main', 'queued', '2026-01-02T00:00:00Z', json_object('note', 'Gane-wins-by-KO', 'refreshed', json('true'), 'section', 4, 'scope', 'Gane-wins-by-KO'), 0)",
                [],
            )?;
            let c1 = arr::worker::get_command(c, "cmd_abcdef0123456789abcdef03")?.unwrap();
            let v = serde_json::to_string(&c1.public())?;
            assert!(!v.contains(PLANTED), "{v}");
            assert!(v.contains("\"refreshed\":true") && v.contains("\"scope\":\"section\""), "{v}");
            Ok(())
        })
        .await
        .unwrap();
}
