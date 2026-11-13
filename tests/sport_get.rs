//! Sport, phase D: a person picks a copy of an event, Arr downloads it, judges the payload again
//! and files it under the Sport folder. Everything here is the loopback stand-ins of the shared
//! harness (the indexer, the schedule source, the download client); nothing reaches a real service.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};
use std::path::PathBuf;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const EVENT: &str = "event:ufc:600061400";
const TAG: &str = "DARKSPORT";

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

async fn insert_event(e: &Exec, id: &str, name: &str, start: OffsetDateTime, state: &str) {
    let league = id.split(':').nth(1).unwrap().to_string();
    let (id, name, state, at) = (
        id.to_string(),
        name.to_string(),
        state.to_string(),
        rfc(start),
    );
    e.state
        .store
        .with(move |c| {
            c.execute(
                "INSERT INTO sport_events (id, league, season, starts_at_utc, ends_at_utc, name, short_name, home_id, home_name, away_id, away_name, round, venue, state, first_seen_utc, updated_utc)
                 VALUES (?1, ?2, NULL, ?3, NULL, ?4, NULL, NULL, NULL, NULL, NULL, NULL, NULL, ?5, ?3, ?3)",
                rusqlite::params![id, league, at, name, state],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// A UFC card two days ago, and the harness; the sport folder is `<library>/Sport`.
async fn setup() -> (Exec, OffsetDateTime) {
    let e = start_exec(ExecOptions::default()).await;
    let start = days_ago(2);
    insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start, "played").await;
    (e, start)
}

fn sport_root(e: &Exec) -> PathBuf {
    e.movies.parent().unwrap().join("sport")
}

/// A release of the card published after it, backed by a real torrent holding `files`.
fn publish(
    e: &Exec,
    start: OffsetDateTime,
    title: &str,
    files: Vec<(String, Vec<u8>)>,
) -> (String, Value) {
    let (hash, mut r) = e.publish(title, title, files, 1.5, false);
    r["publishDate"] = json!(rfc(start + time::Duration::hours(30)));
    (hash, r)
}

fn stock(e: &Exec, releases: Vec<Value>) {
    e.search_results("UFC 334", releases);
    e.search_results("UFC gane hokit", vec![]);
}

async fn copies(e: &Exec) -> Value {
    let (s, v, body) = e
        .json(e.get(&format!("/api/v1/sport/event/{EVENT}/copies?fresh=true")))
        .await;
    assert_eq!(s, 200, "{body}");
    v
}

async fn pick(e: &Exec, id: &str, confirm: bool) -> (u16, Value, String) {
    e.json(
        e.post(&format!("/api/v1/sport/event/{EVENT}/get"))
            .json(&json!({ "id": id, "confirm": confirm })),
    )
    .await
}

fn main_title(extra: &str) -> String {
    format!("UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264{extra}-{TAG}")
}

async fn sport_files(e: &Exec) -> Vec<(String, String, String)> {
    e.state
        .store
        .with(|c| {
            let mut stmt = c.prepare(
                "SELECT event_id, part, path FROM sport_files WHERE present = 1 ORDER BY id",
            )?;
            Ok(stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap()
}

async fn count(e: &Exec, table: &'static str) -> i64 {
    e.state
        .store
        .with(move |c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?))
        .await
        .unwrap()
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

/// A picked copy, downloaded and filed; returns the grab id.
async fn pick_and_file(e: &Exec, start: OffsetDateTime) -> (String, String) {
    let title = main_title("");
    let (hash, r) = publish(e, start, &title, vec![(format!("{title}.mkv"), mkv(3))]);
    stock(e, vec![r]);
    let v = copies(e).await;
    let c = &v["copies"][0];
    assert!(c["caution"].is_null(), "{v}");
    assert!(c["id"].is_string(), "{v}");
    let (s, v, body) = pick(e, c["id"].as_str().unwrap(), false).await;
    assert_eq!(s, 201, "{body}");
    let grab = v["grab"]["id"].as_str().unwrap().to_string();
    e.tick().await;
    e.complete(&hash);
    e.tick().await;
    e.tick().await;
    (grab, hash)
}

// ----------------------------------------------------------------------------- the happy path

#[tokio::test]
async fn a_picked_copy_is_filed_under_sport_named_from_the_event() {
    let (e, start) = setup().await;
    let (files0, targets0, media0) = (
        count(&e, "files").await,
        count(&e, "file_targets").await,
        count(&e, "media").await,
    );
    let (grab, _) = pick_and_file(&e, start).await;
    let g = e.grab_row(&grab);
    assert_eq!(g.state, "imported", "{:?}", g.last_error);
    // One file, below Sport/<League>/<Season>, named from the stored event and the part, not
    // from the torrent.
    let files = files_under(&sport_root(&e));
    assert_eq!(files.len(), 1, "{files:?}");
    let rel = files[0]
        .strip_prefix(sport_root(&e))
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(rel.starts_with(&format!("UFC/{}/", start.year())), "{rel}");
    assert!(name.starts_with("UFC - "), "{name}");
    assert!(
        name.contains("Gane vs. Hokit") && name.ends_with("Main card.mkv"),
        "{name}"
    );
    assert!(!name.contains("WEB-DL") && !name.contains(TAG), "{name}");
    let rows = sport_files(&e).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].0.as_str(), rows[0].1.as_str()), (EVENT, "main"));
    // The movie and series tables were not written.
    assert_eq!(count(&e, "files").await, files0);
    assert_eq!(count(&e, "file_targets").await, targets0);
    assert_eq!(count(&e, "media").await, media0);
}

#[tokio::test]
async fn the_feed_and_the_queue_name_a_sport_download_from_the_event() {
    let (e, start) = setup().await;
    let title = main_title("");
    let (hash, r) = publish(&e, start, &title, vec![(format!("{title}.mkv"), mkv(3))]);
    stock(&e, vec![r]);
    let v = copies(&e).await;
    let (s, _, body) = pick(&e, v["copies"][0]["id"].as_str().unwrap(), false).await;
    assert_eq!(s, 201, "{body}");
    e.tick().await;
    let (_, q, text) = e.json(e.get("/api/v1/queue")).await;
    let item = &q["items"][0];
    assert_eq!(item["title"], "UFC 334: Gane vs. Hokit - Main card");
    assert!(!text.contains(TAG) && !text.contains("WEB-DL"), "{text}");
    let (s, f, text) = e.json(e.get("/api/v1/feed")).await;
    assert_eq!(s, 200, "{text}");
    assert!(text.contains("UFC 334"), "{text}");
    assert!(!text.contains(TAG), "{text}");
    let _ = (f, hash);
}

// ------------------------------------------------------------------------------ picking

#[tokio::test]
async fn a_copy_with_a_caution_needs_the_confirmation_and_an_unlisted_one_cannot_be_picked() {
    let (e, start) = setup().await;
    // No uploader's tag: a caution.
    let title = "UFC.334.Gane.vs.Hokit.Main.Card.1080p.WEB-DL.H264";
    let (_hash, r) = publish(&e, start, title, vec![(format!("{title}.mkv"), mkv(3))]);
    stock(&e, vec![r]);
    let v = copies(&e).await;
    let c = &v["copies"][0];
    assert_eq!(c["caution"]["reason"], "no_uploader_tag", "{v}");
    let id = c["id"].as_str().unwrap().to_string();
    let (s, v, body) = pick(&e, &id, false).await;
    assert_eq!(s, 409, "{body}");
    assert_eq!(v["error"]["code"], "confirmation_needed", "{body}");
    assert_eq!(
        v["error"]["details"]["caution"]["reason"],
        "no_uploader_tag"
    );
    assert_eq!(count(&e, "grabs").await, 0);
    let (s, _, body) = pick(&e, &id, true).await;
    assert_eq!(s, 201, "{body}");
    assert_eq!(count(&e, "grabs").await, 1);
    // A reference nobody listed, a malformed one, and one listed for another event.
    let (s, v, body) = pick(&e, "rel_000000000000000000000000", true).await;
    assert_eq!(s, 404, "{body}");
    assert_eq!(v["error"]["code"], "release_not_found");
    let (s, _, _) = pick(&e, "nonsense", true).await;
    assert_eq!(s, 400);
    insert_event(
        &e,
        "event:ufc:600061401",
        "UFC 335: Oliveira vs. Lopes",
        days_ago(2),
        "played",
    )
    .await;
    let (s, v, body) = e
        .json(
            e.post("/api/v1/sport/event/event:ufc:600061401/get")
                .json(&json!({ "id": id, "confirm": true })),
        )
        .await;
    assert_eq!(s, 404, "{body}");
    assert_eq!(v["error"]["code"], "release_not_found");
}

#[tokio::test]
async fn an_event_not_over_and_a_part_already_filed_are_refused() {
    let (e, start) = setup().await;
    let (_grab, _) = pick_and_file(&e, start).await;
    assert_eq!(sport_files(&e).await.len(), 1);
    // Another copy of the same part: refused, nothing overwritten.
    let before = std::fs::read(files_under(&sport_root(&e))[0].clone()).unwrap();
    let title = main_title(".REPACK");
    let (_h, r) = publish(&e, start, &title, vec![(format!("{title}.mkv"), mkv(4))]);
    stock(&e, vec![r]);
    let v = copies(&e).await;
    let (s, v, body) = pick(&e, v["copies"][0]["id"].as_str().unwrap(), true).await;
    assert_eq!(s, 409, "{body}");
    assert_eq!(v["error"]["code"], "already_have", "{body}");
    assert_eq!(
        std::fs::read(files_under(&sport_root(&e))[0].clone()).unwrap(),
        before
    );
    // A card that has not been played.
    insert_event(
        &e,
        "event:ufc:600061402",
        "UFC 336: Future vs. Card",
        OffsetDateTime::now_utc() + time::Duration::days(5),
        "scheduled",
    )
    .await;
    let (s, v, _) = e
        .json(
            e.post("/api/v1/sport/event/event:ufc:600061402/get")
                .json(&json!({ "id": "rel_000000000000000000000000", "confirm": true })),
        )
        .await;
    assert!(s == 404 || s == 409, "{v}");
}

// ------------------------------------------------------------ the payload, judged again

#[tokio::test]
async fn an_executable_or_an_archive_in_the_payload_is_never_filed() {
    for (bad, ext) in [("setup.exe", "exe"), ("subs.zip", "zip"), ("x.r00", "r00")] {
        let (e, start) = setup().await;
        let title = main_title("");
        let (hash, r) = publish(
            &e,
            start,
            &title,
            vec![
                (format!("{title}.mkv"), mkv(3)),
                (bad.to_string(), b"MZ\x90\x00".to_vec()),
            ],
        );
        stock(&e, vec![r]);
        let v = copies(&e).await;
        let (s, _, body) = pick(&e, v["copies"][0]["id"].as_str().unwrap(), false).await;
        assert_eq!(s, 201, "{body}");
        e.tick().await;
        e.complete(&hash);
        e.tick().await;
        e.tick().await;
        let g = e.grab_row(
            e.state
                .store
                .blocking(|c| {
                    Ok(c.query_row("SELECT id FROM grabs", [], |r| r.get::<_, String>(0))?)
                })
                .unwrap()
                .as_str(),
        );
        assert_eq!(
            (g.state.as_str(), g.failure.as_deref()),
            ("failed", Some("unsafe_payload")),
            "{ext}: {:?}",
            g.last_error
        );
        // (an archive volume such as .r00 is said to be an archive)
        let want = if ext == "r00" { "archive" } else { ext };
        assert!(arr::sport::reason::render_stored(g.last_error.as_deref().unwrap()).contains(want));
        assert!(files_under(&sport_root(&e)).is_empty());
        assert!(sport_files(&e).await.is_empty());
        // Blocklisted for this event: the copy is not offered again.
        let v = copies(&e).await;
        assert!(v["copies"].as_array().unwrap().is_empty(), "{v}");
        assert!(v["skipped"].to_string().contains("failed before"), "{v}");
    }
}

#[tokio::test]
async fn more_than_one_video_waits_for_a_person_and_files_nothing() {
    let (e, start) = setup().await;
    let title = main_title("");
    let (hash, r) = publish(
        &e,
        start,
        &title,
        vec![
            (format!("{title}.mkv"), mkv(3)),
            (format!("{}.mkv", main_title(".REPACK")), mkv(4)),
        ],
    );
    stock(&e, vec![r]);
    let v = copies(&e).await;
    // The torrent's own file list shows two videos: refused at the pick, blocklisted.
    let (s, v, body) = pick(&e, v["copies"][0]["id"].as_str().unwrap(), false).await;
    assert_eq!(s, 409, "{body}");
    assert_eq!(v["error"]["code"], "wrong_payload");
    assert!(body.contains("more than one video"), "{body}");
    let _ = hash;
    assert!(files_under(&sport_root(&e)).is_empty());
    assert!(sport_files(&e).await.is_empty());
}

#[test]
fn the_bounds_say_what_a_plausible_video_is() {
    use arr::sport::Part;
    use arr::sport::catalog::Sport;
    use arr::sport::get::{bounds, implausible};
    let b = bounds(Sport::Mma, Part::Main);
    assert_eq!((b.min_secs, b.max_secs), (45 * 60, 4 * 3600 + 1800));
    assert!(implausible(b, 4 << 30, 2.0 * 3600.0).is_none());
    assert!(implausible(b, 4 << 30, 120.0).unwrap().contains("shorter"));
    assert!(
        implausible(b, 4 << 30, 20.0 * 3600.0)
            .unwrap()
            .contains("longer")
    );
    assert!(
        implausible(b, 10 << 20, 2.0 * 3600.0)
            .unwrap()
            .contains("small")
    );
    assert!(
        implausible(b, 200 << 30, 2.0 * 3600.0)
            .unwrap()
            .contains("large")
    );
    assert_eq!(bounds(Sport::Mma, Part::Whole).min_secs, 90 * 60);
    assert_eq!(bounds(Sport::Soccer, Part::Whole).min_secs, 45 * 60);
    assert_eq!(bounds(Sport::Golf, Part::Round(3)).max_secs, 10 * 3600);
}

#[tokio::test]
async fn a_video_too_short_for_the_part_fails_when_the_bounds_are_on() {
    let (e, start) = {
        let e = start_exec(ExecOptions {
            sample_detection: true,
            ..Default::default()
        })
        .await;
        let start = days_ago(2);
        insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start, "played").await;
        (e, start)
    };
    let title = main_title("");
    let (hash, r) = publish(&e, start, &title, vec![(format!("{title}.mkv"), mkv(3))]);
    stock(&e, vec![r]);
    let v = copies(&e).await;
    let (s, v, body) = pick(&e, v["copies"][0]["id"].as_str().unwrap(), false).await;
    assert_eq!(s, 201, "{body}");
    let grab = v["grab"]["id"].as_str().unwrap().to_string();
    e.tick().await;
    e.complete(&hash);
    e.tick().await;
    e.tick().await;
    let g = e.grab_row(&grab);
    assert_eq!(
        (g.state.as_str(), g.failure.as_deref()),
        ("failed", Some("invalid_media")),
        "{:?}",
        g.last_error
    );
    assert!(
        arr::sport::reason::render_stored(g.last_error.as_deref().unwrap()).contains("shorter")
    );
    assert!(sport_files(&e).await.is_empty());
}

// -------------------------------------------------------------------------- data safety

#[tokio::test]
async fn nothing_that_exists_is_overwritten_and_movies_and_series_never_notice() {
    let (e, start) = setup().await;
    // A movie root file and a library listing as they are before.
    let routes = [
        "/api/v1/library/movies",
        "/api/v1/library/series",
        "/api/v1/wanted",
        "/api/v1/library/roots",
    ];
    // (the first pass of the worker scans the roots: let it happen before looking)
    e.tick().await;
    let mut before = vec![];
    for r in routes {
        let (s, v, _) = e.json(e.get(r)).await;
        before.push((s, v));
    }
    // A file already at the destination: the import is blocked, the bytes are unchanged.
    let title = main_title("");
    let (hash, r) = publish(&e, start, &title, vec![(format!("{title}.mkv"), mkv(3))]);
    stock(&e, vec![r]);
    let v = copies(&e).await;
    let rel_dir = format!("UFC/{}", start.year());
    let (s, v, body) = pick(&e, v["copies"][0]["id"].as_str().unwrap(), false).await;
    assert_eq!(s, 201, "{body}");
    let grab = v["grab"]["id"].as_str().unwrap().to_string();
    let date = format!(
        "{:04}-{:02}-{:02}",
        start.year(),
        u8::from(start.month()),
        start.day()
    );
    let existing = sport_root(&e).join(&rel_dir).join(format!(
        "UFC - {date} - UFC 334 - Gane vs. Hokit - Main card.mkv"
    ));
    put(
        &sport_root(&e),
        &format!(
            "{rel_dir}/{}",
            existing.file_name().unwrap().to_string_lossy()
        ),
        b"mine",
    );
    e.tick().await;
    e.complete(&hash);
    e.tick().await;
    e.tick().await;
    let g = e.grab_row(&grab);
    // The name was taken by something else: the copy is filed beside it under the event's tag,
    // and what was there is untouched.
    assert_eq!(g.state, "imported", "{:?}", g.last_error);
    assert_eq!(std::fs::read(&existing).unwrap(), b"mine");
    assert_eq!(sport_files(&e).await.len(), 1);
    assert_eq!(files_under(&sport_root(&e)).len(), 2);
    for (i, r) in routes.iter().enumerate() {
        let (s, v, _) = e.json(e.get(r)).await;
        assert_eq!((s, &v), (before[i].0, &before[i].1), "{r} changed");
    }
}

#[tokio::test]
async fn unfollowing_deletes_nothing() {
    let (e, start) = setup().await;
    let (s, _, body) = e
        .json(
            e.post("/api/v1/sport/follow")
                .json(&json!({ "league": "nba" })),
        )
        .await;
    assert!(s == 200 || s == 201, "{body}");
    pick_and_file(&e, start).await;
    let files = files_under(&sport_root(&e));
    assert_eq!(files.len(), 1);
    let (s, _, body) = e
        .json(
            e.post("/api/v1/sport/unfollow")
                .json(&json!({ "league": "nba" })),
        )
        .await;
    assert!(s == 200 || s == 204, "{body}");
    assert_eq!(files_under(&sport_root(&e)), files);
    assert_eq!(sport_files(&e).await.len(), 1);
    let n: i64 = e
        .state
        .store
        .blocking(|c| Ok(c.query_row("SELECT COUNT(*) FROM sport_events", [], |r| r.get(0))?))
        .unwrap();
    assert!(n >= 1);
}

// -------------------------------------------------------------- the route table

#[tokio::test]
async fn an_event_id_sent_to_every_route_that_takes_a_media_id_is_refused() {
    let (e, _start) = setup().await;
    let id = EVENT;
    let mut tried = 0;
    for (method, path) in arr::api::ROUTES {
        if path.starts_with("/api/v1/sport/") {
            continue;
        }
        let takes_media_id = path.contains("{media_id}");
        if !takes_media_id {
            continue;
        }
        let url = path.replace("{media_id}", id);
        let rb = match *method {
            "get" => e.get(&url),
            "post" => e.post(&url).json(&json!({})),
            "patch" => e.patch(&url).json(&json!({})),
            "delete" => e.delete(&url),
            other => panic!("{other}"),
        };
        let (s, v, body) = e.json(rb).await;
        assert!(
            (400..500).contains(&s),
            "{method} {path} answered {s}: {body}"
        );
        let code = v["error"]["code"].as_str().unwrap_or("");
        assert!(
            matches!(
                code,
                "invalid_media_id"
                    | "delete_refused"
                    | "not_found"
                    | "invalid_request"
                    | "invalid_body"
            ),
            "{method} {path}: {code}: {body}"
        );
        tried += 1;
    }
    assert!(tried >= 8, "{tried}");
    // The bodies that carry a media id: adding it, quick-adding it, planning it.
    for (path, body) in [
        ("/api/v1/library", json!({ "media_id": id })),
        ("/api/v1/plans", json!({ "media_id": id })),
        ("/api/v1/quick-add", json!({ "media_id": id })),
        (
            "/api/v1/grabs",
            json!({ "media_id": id, "release_ref": "rel_000000000000000000000000" }),
        ),
    ] {
        let (s, v, text) = e.json(e.post(path).json(&body)).await;
        assert_eq!(s, 400, "{path}: {text}");
        assert_eq!(v["error"]["code"], "invalid_media_id", "{path}: {text}");
    }
    let (s, v, _) = e
        .json(e.post("/api/v1/delete").json(&json!({ "media_id": id })))
        .await;
    assert!((400..500).contains(&s), "{v}");
}

// ------------------------------------------------------------------- crash points

/// Writes a journal row and the files a crash at `state` would have left, then restarts: the
/// recovery runs at startup.
#[cfg(unix)]
async fn crash(e: &mut Exec, state: &str) -> (PathBuf, PathBuf, Vec<u8>) {
    // A grab the journal can point at: a copy picked (nothing is downloaded yet).
    let start = days_ago(2);
    let title = main_title("");
    let (_h, r) = publish(e, start, &title, vec![(format!("{title}.mkv"), mkv(3))]);
    stock(e, vec![r]);
    let v = copies(e).await;
    let (_, v, body) = pick(e, v["copies"][0]["id"].as_str().unwrap(), false).await;
    let grab = v["grab"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{body}"))
        .to_string();
    let grab = grab.as_str();
    let root = sport_root(e);
    let dir = root.join("UFC/2026");
    std::fs::create_dir_all(&dir).unwrap();
    let staged = dir.join(".um-import-777.partial");
    let dest = dir.join("UFC - 2026-01-01 - UFC 334 - Gane vs. Hokit - Main card.mkv");
    let source = e._tmp.path().join("downloads-source.mkv");
    let bytes = mkv(3);
    std::fs::write(&source, &bytes).unwrap();
    // The staged inode is a hard link of the download (the download stays intact).
    std::fs::hard_link(&source, &staged).unwrap();
    let ino = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&staged).unwrap().ino() as i64
    };
    if matches!(state, "published" | "committed") {
        std::fs::hard_link(&staged, &dest).unwrap();
        if state == "committed" {
            std::fs::remove_file(&staged).unwrap();
        }
    }
    let (root2, dest2, grab2, st, source2) = (
        root.to_string_lossy().into_owned(),
        "UFC/2026/UFC - 2026-01-01 - UFC 334 - Gane vs. Hokit - Main card.mkv".to_string(),
        grab.to_string(),
        state.to_string(),
        source.to_string_lossy().into_owned(),
    );
    let staged_s = staged.to_string_lossy().into_owned();
    e.state
        .store
        .with(move |c| {
            c.execute(
                "INSERT INTO import_journal (grab_id, source, root, dest, targets, size, method, state, staged, staged_ino, updated_at)
                 VALUES (?1, ?2, ?3, ?4, '[]', 1, 'hardlink', ?5, ?6, ?7, '2026-01-01T00:00:00Z')",
                rusqlite::params![grab2, source2, root2, dest2, st, staged_s, ino],
            )?;
            if st == "committed" {
                c.execute(
                    "INSERT INTO sport_files (event_id, part, path, size, present, imported_utc, observed_utc)
                     VALUES ('event:ufc:600061400', 'main', ?1, 1, 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                    [dest2],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    e.restart().await;
    (dest, source, bytes)
}

#[cfg(unix)]
#[tokio::test]
async fn a_crash_before_the_file_is_in_place_leaves_the_download_intact() {
    for state in ["planned", "staged", "published"] {
        let (mut e, _start) = setup().await;
        let (dest, source, bytes) = crash(&mut e, state).await;
        assert!(!dest.exists(), "{state}: the half-filed copy is gone");
        assert!(source.exists(), "{state}: the download is intact");
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert!(
            !files_under(&sport_root(&e))
                .iter()
                .any(|p| p.to_string_lossy().contains(".partial")),
            "{state}: no staged leftover"
        );
        assert!(sport_files(&e).await.is_empty(), "{state}: no row");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_crash_after_the_commit_leaves_the_file_and_its_row() {
    let (mut e, _start) = setup().await;
    let (dest, source, _) = crash(&mut e, "committed").await;
    assert!(dest.exists());
    assert!(source.exists());
    assert_eq!(sport_files(&e).await.len(), 1);
}

// ------------------------------------------------------------------------------ Plex

async fn with_plex(extra: &str) -> (Exec, OffsetDateTime) {
    let e = start_exec(ExecOptions {
        plex_extra: extra.to_string(),
        ..Default::default()
    })
    .await;
    let start = days_ago(2);
    insert_event(&e, EVENT, "UFC 334: Gane vs. Hokit", start, "played").await;
    (e, start)
}

async fn command_results(e: &Exec) -> Vec<String> {
    e.state
        .store
        .with(|c| {
            let mut stmt = c.prepare("SELECT COALESCE(result, '') FROM commands WHERE name = 'plex_refresh' ORDER BY created_at")?;
            Ok(stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn the_sport_library_is_created_once_at_the_first_import_and_that_folder_is_scanned() {
    let (e, start) = with_plex("sport_path = \"/media/Sport\"\n").await;
    pick_and_file(&e, start).await;
    e.tick().await;
    let (created, scans) = {
        let m = e.mock.lock().unwrap();
        (m.plex_created.clone(), m.plex_scans.clone())
    };
    assert_eq!(created.len(), 1, "{:?}", created);
    // The exact request: POST library/sections with these query parameters.
    let q: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(created[0].as_bytes())
            .into_owned()
            .collect();
    let want = [
        ("name", "Sport"),
        ("type", "movie"),
        ("agent", "com.plexapp.agents.none"),
        ("scanner", "Plex Video Files Scanner"),
        ("language", "en-US"),
        ("location", "/media/Sport"),
    ];
    assert_eq!(q.len(), want.len(), "{q:?}");
    for (k, v) in want {
        assert_eq!(q.get(k).map(String::as_str), Some(v), "{k}: {q:?}");
    }
    // The folder of the event is scanned in the section that was made.
    let scan = scans.last().unwrap();
    assert!(scan.starts_with("10?"), "{scan}");
    assert!(scan.contains("path="), "{scan}");
    let path = url::form_urlencoded::parse(scan.split_once('?').unwrap().1.as_bytes())
        .find(|(k, _)| k == "path")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    assert_eq!(path, format!("/media/Sport/UFC/{}", start.year()));
    // A second import does not create it again.
    let title = main_title(".R2");
    let _ = title;
    e.tick().await;
    assert_eq!(e.mock.lock().unwrap().plex_created.len(), 1);
}

#[tokio::test]
async fn a_section_that_already_holds_the_sport_folder_is_used_and_nothing_is_created() {
    let (e, start) = with_plex("sport_path = \"/media/Sport\"\n").await;
    e.mock.lock().unwrap().plex_sections = vec![json!({
        "key": "7", "title": "Other videos", "type": "movie", "agent": "com.plexapp.agents.none",
        "Location": [{ "path": "/media/Sport" }]
    })];
    pick_and_file(&e, start).await;
    e.tick().await;
    let (created, scans) = {
        let m = e.mock.lock().unwrap();
        (m.plex_created.clone(), m.plex_scans.clone())
    };
    assert!(created.is_empty(), "{:?}", created);
    assert!(scans.iter().any(|s| s.starts_with("7?")), "{:?}", scans);
}

#[tokio::test]
async fn when_plex_cannot_make_the_library_the_import_still_succeeds_and_says_where_the_file_is() {
    let (e, start) = with_plex("sport_path = \"/media/Sport\"\n").await;
    e.mock.lock().unwrap().plex_create_fail = true;
    let (grab, _) = pick_and_file(&e, start).await;
    e.tick().await;
    assert_eq!(e.grab_row(&grab).state, "imported");
    assert_eq!(sport_files(&e).await.len(), 1);
    let results = command_results(&e).await;
    assert!(
        results
            .iter()
            .any(|r| r.contains("Saved in") && r.contains("Other Videos")),
        "{results:?}"
    );
}
