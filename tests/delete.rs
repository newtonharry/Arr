//! "Delete files": a preview and a token, files moved into the bin with a rename, the
//! downloads that only seed them removed, a way back for seven days, and every refusal that
//! keeps real media safe.
#![recursion_limit = "256"]

mod common;

use common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::OffsetDateTime;

const MATRIX: &str = "The.Matrix.1999.1080p.WEB-DL.x264-UM";
const RELOADED: &str =
    "The Matrix Reloaded (2003)/The.Matrix.Reloaded.2003.1080p.WEB-DL.x264-OLD.mkv";
const SEASON: &str = "season:tvdb:372264:1";
const SHOW: &str = "series:tvdb:372264";

async fn call(e: &Exec, rb: reqwest::RequestBuilder) -> (u16, Value) {
    let (s, v, _) = e.json(rb).await;
    (s, v)
}

async fn preview(e: &Exec, id: &str) -> (u16, Value) {
    call(
        e,
        e.post("/api/v1/delete/preview")
            .json(&json!({ "media_id": id })),
    )
    .await
}

async fn delete(e: &Exec, id: &str, token: &str, keep_wanted: bool) -> (u16, Value) {
    call(
        e,
        e.post("/api/v1/delete")
            .json(&json!({ "media_id": id, "token": token, "keep_wanted": keep_wanted })),
    )
    .await
}

/// Previews and deletes in one go; the delete's answer.
async fn remove(e: &Exec, id: &str, keep_wanted: bool) -> Value {
    let (s, p) = preview(e, id).await;
    assert_eq!(s, 200, "{p}");
    let (s, v) = delete(e, id, p["token"].as_str().unwrap(), keep_wanted).await;
    assert_eq!(s, 200, "{v}");
    v
}

fn bin_file(root: &Path, delete_id: &str, rel: &str) -> PathBuf {
    root.join(format!(".arr-bin/{delete_id}/{rel}"))
}

async fn movie(e: &Exec, q: &str) -> Value {
    let (_, v) = call(e, e.get(&format!("/api/v1/library/movies?q={q}"))).await;
    v["items"][0].clone()
}

/// The Matrix, downloaded and imported through the real pipeline; the grab id and hash.
async fn imported_matrix(e: &Exec) -> (String, String) {
    let files = vec![
        (format!("{MATRIX}.mkv"), mkv(2)),
        ("Sample/sample.mkv".to_string(), mkv(1)),
    ];
    let (hash, r) = e.publish(MATRIX, MATRIX, files, 8.0, false);
    e.search_results("The Matrix 1999", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": "movie:tmdb:603", "execute": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    e.complete(&hash);
    e.tick().await;
    assert_eq!(e.grab_row(&id).state, "imported");
    (id, hash)
}

/// The Slow Horses season pack, imported for the episodes that were missing.
async fn imported_pack(e: &Exec) -> (String, String) {
    let name = "Slow.Horses.S01.1080p.WEB-DL.x264-PACK";
    let files: Vec<(String, Vec<u8>)> = (1..=6)
        .map(|n| {
            (
                format!("Slow.Horses.S01E{n:02}.1080p.WEB-DL.x264-PACK.mkv"),
                mkv(1),
            )
        })
        .collect();
    let (hash, r) = e.publish(name, name, files, 12.0, false);
    e.search_results("Slow Horses S01", vec![r]);
    let (s, v, text) = e
        .json(
            e.post("/api/v1/plans")
                .json(&json!({ "media_id": SEASON, "execute": true, "replan": true })),
        )
        .await;
    assert_eq!(s, 201, "{text}");
    let id = v["plan"]["execution"]["grabs"][0]
        .as_str()
        .unwrap()
        .to_string();
    e.tick().await;
    e.complete(&hash);
    e.tick().await;
    assert_eq!(
        e.grab_row(&id).state,
        "imported",
        "{:?}",
        e.grab_row(&id).last_error
    );
    (id, hash)
}

#[tokio::test]
async fn deleting_a_movie_moves_its_files_to_the_bin_and_removes_what_seeds_them() {
    let e = start_exec(ExecOptions::default()).await;
    let (grab, hash) = imported_matrix(&e).await;
    let folder = e.movies.join("The Matrix (1999)");
    let video = folder.join(format!("{MATRIX}.mkv"));
    let source = e.downloads.join(format!("{MATRIX}/{MATRIX}.mkv"));
    assert!(video.is_file() && source.is_file());
    // Beside the video: its subtitle goes too, a poster and another title's file do not.
    put(
        &e.movies,
        &format!("The Matrix (1999)/{MATRIX}.en.srt"),
        b"1",
    );
    put(&e.movies, "The Matrix (1999)/poster.jpg", b"p");
    let (s, p) = preview(&e, "movie:tmdb:603").await;
    assert_eq!(s, 200, "{p}");
    let paths: Vec<&str> = p["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        [
            format!("The Matrix (1999)/{MATRIX}.en.srt"),
            format!("The Matrix (1999)/{MATRIX}.mkv")
        ]
    );
    assert_eq!(p["files"][0]["kind"], "extra");
    assert_eq!(p["files"][1]["kind"], "video");
    assert_eq!(p["unknown"], 0);
    assert_eq!(p["downloads"][0]["grab_id"], json!(grab));
    assert_eq!(p["downloads"][0]["removed"], true);
    assert!(
        !p.to_string().contains(e.movies.to_str().unwrap()),
        "relative paths only"
    );
    let total = p["library_bytes"].as_u64().unwrap();
    assert!(total > 0);

    let before = OffsetDateTime::now_utc();
    let v = remove(&e, "movie:tmdb:603", false).await;
    let id = v["delete_id"].as_str().unwrap().to_string();
    assert_eq!(
        (v["moved"].clone(), v["bytes"].clone()),
        (json!(2), json!(total))
    );
    let purge_at = OffsetDateTime::parse(
        v["purge_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert!(
        (purge_at - before) > time::Duration::days(6)
            && (purge_at - before) < time::Duration::days(8)
    );

    // The video and its subtitle are in the bin; the rest stayed where it was.
    assert!(!video.exists());
    assert!(bin_file(&e.movies, &id, &format!("The Matrix (1999)/{MATRIX}.mkv")).is_file());
    assert!(
        bin_file(
            &e.movies,
            &id,
            &format!("The Matrix (1999)/{MATRIX}.en.srt")
        )
        .is_file()
    );
    assert_eq!(
        std::fs::read_to_string(e.movies.join(".arr-bin/.plexignore"))
            .unwrap()
            .trim(),
        "*"
    );
    assert!(
        folder.join("poster.jpg").is_file(),
        "only what shares the name goes"
    );
    // The seeding download went with its data: nothing is left to hold the space.
    e.tick().await;
    assert!(!e.fake.lock().unwrap().torrents.contains_key(&hash));
    assert!(!source.exists(), "the download's data is removed");
    // Arr no longer owns it, stopped wanting it, and says so in the history.
    let m = movie(&e, "matrix").await;
    assert_eq!(m["has_file"], false, "{m}");
    assert_eq!(m["monitored"], false);
    let (_, h) = call(&e, e.get("/api/v1/history?limit=50")).await;
    assert!(h.to_string().contains("\"deleted\""), "{h}");
    // Plex was asked to look again.
    e.tick().await;
    assert!(!e.mock.lock().unwrap().plex.is_empty());
    // The bin lists it.
    let (_, b) = call(&e, e.get("/api/v1/bin")).await;
    assert_eq!(b["entries"][0]["delete_id"], json!(id));
    assert_eq!(b["entries"][0]["files"], 2);
    assert_eq!(b["entries"][0]["media_id"], "movie:tmdb:603");
    assert!(!b.to_string().contains(e.movies.to_str().unwrap()));
}

#[tokio::test]
async fn deleting_an_episode_keeps_a_pack_that_also_holds_others() {
    let e = start_exec(ExecOptions::default()).await;
    let (pack, pack_hash) = imported_pack(&e).await;
    let season_dir = e.tv.join("Slow Horses/Season 1");
    let two = season_dir.join("Slow.Horses.S01E02.1080p.WEB-DL.x264-PACK.mkv");
    assert!(two.is_file());
    // The pack also holds episodes that stay: it is not removed with one episode.
    let (s, p) = preview(&e, "episode:tvdb:372264:1:2").await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["files"].as_array().unwrap().len(), 1, "{p}");
    assert_eq!(p["downloads"][0]["grab_id"], json!(pack));
    assert_eq!(p["downloads"][0]["removed"], false);
    assert!(
        p["downloads"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("staying")
    );
    let v = remove(&e, "episode:tvdb:372264:1:2", false).await;
    assert_eq!(v["moved"], 1);
    e.tick().await;
    assert!(
        e.fake.lock().unwrap().torrents.contains_key(&pack_hash),
        "the pack stays"
    );
    assert!(!two.exists());
    assert!(
        season_dir.is_dir(),
        "other episodes are still in the season folder"
    );
    // Only that episode stopped being wanted.
    let (_, lib) = call(&e, e.get("/api/v1/library/series?q=slow")).await;
    let s1 = lib["items"][0]["seasons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["number"] == 1)
        .unwrap()
        .clone();
    assert!(s1["monitored_episodes"].as_u64().unwrap() >= 5, "{s1}");
}

#[tokio::test]
async fn deleting_a_season_takes_the_pack_that_only_holds_it() {
    let e = start_exec(ExecOptions::default()).await;
    let (pack, pack_hash) = imported_pack(&e).await;
    let season_dir = e.tv.join("Slow Horses/Season 1");
    // The whole season holds everything the pack holds: it goes with it.
    let (s, p) = preview(&e, SEASON).await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["files"].as_array().unwrap().len(), 6, "{p}");
    assert_eq!(p["downloads"][0]["removed"], true, "{p}");
    let v = remove(&e, SEASON, false).await;
    assert_eq!(v["moved"], 6);
    e.tick().await;
    let g = e.grab_row(&pack);
    assert!(
        !e.fake.lock().unwrap().torrents.contains_key(&pack_hash),
        "{v} {:?} {:?} {:?}",
        g.cleanup,
        g.cleanup_detail,
        g.state
    );
    assert!(
        !season_dir.exists(),
        "the emptied season folder is removed: {:?}",
        std::fs::read_dir(&season_dir).map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
    );
    assert!(e.tv.is_dir(), "never the root");
    let (_, lib) = call(&e, e.get("/api/v1/library/series?q=slow")).await;
    assert_eq!(lib["total"], 1, "the show itself stays in Arr: {lib}");
}

#[tokio::test]
async fn deleting_a_show_ends_like_remove_from_arr() {
    let e = start_exec(ExecOptions::default()).await;
    let v = remove(&e, SHOW, false).await;
    assert_eq!(v["moved"], 2);
    let (_, lib) = call(&e, e.get("/api/v1/library/series?q=slow")).await;
    assert_eq!(lib["total"], 0, "{lib}");
    assert!(!e.tv.join("Slow Horses").exists());
    assert!(e.tv.is_dir());
    // The show's other seasons of files, folders of other shows: untouched.
    assert!(e.tv.join("Fargo/Season 4").is_dir());
    // Undo puts the files back; the show is not tracked again.
    let (_, b) = call(&e, e.get("/api/v1/bin")).await;
    let id = b["entries"][0]["delete_id"].as_str().unwrap().to_string();
    let (s, r) = call(&e, e.post(&format!("/api/v1/bin/{id}/restore"))).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["restored"], 2);
    assert!(
        e.tv.join("Slow Horses/Season 1/Slow.Horses.S01E01.1080p.WEB-DL.x264-OLD.mkv")
            .is_file()
    );
    let (_, lib) = call(&e, e.get("/api/v1/library/series?q=slow")).await;
    assert_eq!(lib["total"], 0, "restoring does not monitor anything again");
}

#[tokio::test]
async fn keep_wanted_leaves_the_item_monitored() {
    let e = start_exec(ExecOptions::default()).await;
    remove(&e, "movie:tmdb:604", true).await;
    let m = movie(&e, "reloaded").await;
    assert_eq!(
        (m["has_file"].clone(), m["monitored"].clone()),
        (json!(false), json!(true)),
        "{m}"
    );
    assert!(!e.movies.join(RELOADED).exists());
}

#[tokio::test]
async fn restore_puts_files_back_and_refuses_an_occupied_path() {
    let e = start_exec(ExecOptions::default()).await;
    let original = e.movies.join(RELOADED);
    let bytes = std::fs::read(&original).unwrap();
    let v = remove(&e, "movie:tmdb:604", false).await;
    let id = v["delete_id"].as_str().unwrap().to_string();
    assert!(!original.exists());
    // Something new took the place: the file is not overwritten, it stays in the bin.
    put(&e.movies, RELOADED, b"another file");
    let (s, r) = call(&e, e.post(&format!("/api/v1/bin/{id}/restore"))).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["restored"], 0);
    assert_eq!(r["skipped"][0]["path"], RELOADED);
    assert_eq!(std::fs::read(&original).unwrap(), b"another file");
    assert!(bin_file(&e.movies, &id, RELOADED).is_file());
    let (_, b) = call(&e, e.get("/api/v1/bin")).await;
    assert_eq!(b["entries"].as_array().unwrap().len(), 1, "the entry stays");
    // Cleared away, it goes back whole and is owned again; nothing is monitored again.
    std::fs::remove_file(&original).unwrap();
    let (s, r) = call(&e, e.post(&format!("/api/v1/bin/{id}/restore"))).await;
    assert_eq!((s, r["restored"].clone()), (200, json!(1)), "{r}");
    assert_eq!(std::fs::read(&original).unwrap(), bytes);
    let m = movie(&e, "reloaded").await;
    assert_eq!(
        (m["has_file"].clone(), m["monitored"].clone()),
        (json!(true), json!(false)),
        "{m}"
    );
    let (_, b) = call(&e, e.get("/api/v1/bin")).await;
    assert!(b["entries"].as_array().unwrap().is_empty());
    assert!(!e.movies.join(format!(".arr-bin/{id}")).exists());
    let (s, _) = call(&e, e.post(&format!("/api/v1/bin/{id}/restore"))).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn the_bin_empties_after_seven_days_or_on_request() {
    let e = start_exec(ExecOptions::default()).await;
    let v = remove(&e, "movie:tmdb:604", false).await;
    let id = v["delete_id"].as_str().unwrap().to_string();
    let file = bin_file(&e.movies, &id, RELOADED);
    assert!(file.is_file());
    let now = OffsetDateTime::now_utc();
    arr::worker::run_task(&e.state, "bin", now + Duration::from_secs(6 * 86_400))
        .await
        .unwrap();
    assert!(file.is_file(), "six days in, it is still there");
    arr::worker::run_task(&e.state, "bin", now + Duration::from_secs(8 * 86_400))
        .await
        .unwrap();
    assert!(!file.exists() && !e.movies.join(format!(".arr-bin/{id}")).exists());
    let (_, b) = call(&e, e.get("/api/v1/bin")).await;
    assert!(b["entries"].as_array().unwrap().is_empty());
    assert!(e.movies.is_dir() && e.movies.join(".arr-bin").is_dir());
}

#[tokio::test]
async fn delete_now_empties_one_entry_and_leaves_the_others() {
    let e = start_exec(ExecOptions::default()).await;
    let a = remove(&e, "movie:tmdb:604", false).await["delete_id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = remove(&e, SHOW, false).await["delete_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, r) = call(&e, e.delete(&format!("/api/v1/bin/{a}"))).await;
    assert_eq!((s, r["purged"].clone()), (200, json!(true)), "{r}");
    assert!(!bin_file(&e.movies, &a, RELOADED).exists());
    assert!(
        e.tv.join(format!(".arr-bin/{b}")).is_dir(),
        "the other entry is untouched"
    );
    let (s, _) = call(&e, e.delete(&format!("/api/v1/bin/{a}"))).await;
    assert_eq!(s, 404);
    let (s, _) = call(&e, e.delete("/api/v1/bin/..%2F..")).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn a_changed_file_refuses_the_delete() {
    let e = start_exec(ExecOptions::default()).await;
    let (_, p) = preview(&e, "movie:tmdb:604").await;
    let token = p["token"].as_str().unwrap().to_string();
    let file = e.movies.join(RELOADED);
    let mut grown = std::fs::read(&file).unwrap();
    grown.extend_from_slice(b"more");
    std::fs::write(&file, grown).unwrap();
    let (s, v) = delete(&e, "movie:tmdb:604", &token, false).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("changed")),
        "{v}"
    );
    assert!(file.is_file(), "nothing moved");
    let (_, b) = call(&e, e.get("/api/v1/bin")).await;
    assert!(b["entries"].as_array().unwrap().is_empty());
    // A token for something else does not do either.
    let (_, p2) = preview(&e, "movie:tmdb:604").await;
    let (s, v) = delete(&e, "movie:tmdb:603", p2["token"].as_str().unwrap(), false).await;
    assert_eq!(s, 409, "{v}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_is_refused() {
    let e = start_exec(ExecOptions::default()).await;
    let outside = e.dir.join("outside.mkv");
    std::fs::write(&outside, mkv(1)).unwrap();
    let file = e.movies.join(RELOADED);
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(&outside, &file).unwrap();
    let (s, v) = preview(&e, "movie:tmdb:604").await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("delete_refused")),
        "{v}"
    );
    let (s, v) = delete(&e, "movie:tmdb:604", "anything", false).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("delete_refused")),
        "{v}"
    );
    assert!(
        outside.is_file()
            && std::fs::symlink_metadata(&file)
                .unwrap()
                .file_type()
                .is_symlink()
    );
    // A symlinked folder is refused too, and nothing behind it is touched.
    std::fs::remove_file(&file).unwrap();
    let real = e.dir.join("elsewhere");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(
        real.join("The.Matrix.Reloaded.2003.1080p.WEB-DL.x264-OLD.mkv"),
        mkv(1),
    )
    .unwrap();
    std::fs::remove_dir(e.movies.join("The Matrix Reloaded (2003)")).unwrap();
    std::os::unix::fs::symlink(&real, e.movies.join("The Matrix Reloaded (2003)")).unwrap();
    let (s, v) = preview(&e, "movie:tmdb:604").await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("delete_refused")),
        "{v}"
    );
    assert!(
        real.join("The.Matrix.Reloaded.2003.1080p.WEB-DL.x264-OLD.mkv")
            .is_file()
    );
}

#[tokio::test]
async fn a_path_that_escapes_the_library_is_refused() {
    let e = start_exec(ExecOptions::default()).await;
    let outside = e.dir.join("escape.mkv");
    std::fs::write(&outside, mkv(1)).unwrap();
    let bad = format!("../{}", outside.file_name().unwrap().to_str().unwrap());
    e.state
        .store
        .blocking(move |c| {
            c.execute(
                "UPDATE files SET path = ?1 WHERE media_id = 'movie:tmdb:604' AND present = 1",
                [&bad],
            )?;
            Ok(())
        })
        .unwrap();
    e.state.reload_index().await.unwrap();
    let (s, v) = preview(&e, "movie:tmdb:604").await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("delete_refused")),
        "{v}"
    );
    assert!(outside.is_file());
    // The bin is no place to delete from either.
    e.state
        .store
        .blocking(|c| {
            c.execute(
                "UPDATE files SET path = '.arr-bin/x/y.mkv' WHERE media_id = 'movie:tmdb:604' AND present = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    e.state.reload_index().await.unwrap();
    let (s, v) = preview(&e, "movie:tmdb:604").await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("delete_refused")),
        "{v}"
    );
}

#[tokio::test]
async fn a_download_being_added_to_plex_holds_the_delete() {
    let e = start_exec(ExecOptions::default()).await;
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
    let g = id.clone();
    e.state
        .store
        .blocking(move |c| {
            Ok(c.execute("UPDATE grabs SET state = 'completed' WHERE id = ?1", [&g])?)
        })
        .unwrap();
    let (s, p) = preview(&e, SEASON).await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["downloads"][0]["removed"], false);
    let (s, v) = delete(&e, SEASON, p["token"].as_str().unwrap(), false).await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("active_downloads")),
        "{v}"
    );
    assert_eq!(e.grab_row(&id).state, "completed");
    assert!(e.fake.lock().unwrap().torrents.contains_key(&hash));
    assert!(
        e.tv.join("Slow Horses/Season 1/Slow.Horses.S01E01.1080p.WEB-DL.x264-OLD.mkv")
            .is_file()
    );
    let (_, b) = call(&e, e.get("/api/v1/bin")).await;
    assert!(b["entries"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn it_needs_the_same_sign_in_and_a_known_title() {
    let e = start_exec(ExecOptions::default()).await;
    let r = e
        .client
        .post(format!("{}/api/v1/delete/preview", e.base))
        .json(&json!({ "media_id": "movie:tmdb:604" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let (s, _) = preview(&e, "movie:tmdb:99999").await;
    assert_eq!(s, 404);
    let (s, _) = preview(&e, "nonsense").await;
    assert_eq!(s, 400);
    // A whole show with nothing to delete says so rather than doing nothing.
    let (s, p) = preview(&e, "season:tvdb:269613:9").await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["files"], json!([]));
    let (s, v) = delete(
        &e,
        "season:tvdb:269613:9",
        p["token"].as_str().unwrap(),
        false,
    )
    .await;
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("delete_refused")),
        "{v}"
    );
}
