//! The bin behind "Delete files": a deleted title's files move (rename only, never copy) into
//! `<root>/.arr-bin/<delete_id>/` on the same drive and stay there for seven days, so a
//! delete can be undone. Every file is checked before it moves: a plain relative path that
//! stays inside the root, a regular file, no symlink anywhere on the way, nothing already in
//! the bin. The bin holds a `.plexignore` so Plex never lists what is in it.

use crate::api::AppState;
use crate::import::rfc3339;
use crate::paths;
use crate::settings::MediaKind;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const BIN: &str = ".arr-bin";
pub const KEEP_DAYS: i64 = 7;

/// Deletes, restores and purges run one at a time.
pub static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Subtitles, metadata and artwork that travel with a video of the same name.
const SIDECARS: &[&str] = &[
    "srt", "ass", "ssa", "sub", "idx", "vtt", "nfo", "jpg", "jpeg", "png", "webp", "tbn", "gif",
];

/// A file a delete takes: its path relative to its root, its size and `video` or `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinFile {
    pub path: String,
    pub bytes: u64,
    pub kind: String,
    /// Modified time in nanoseconds, for the token. Not stored.
    #[serde(skip)]
    pub stamp: u128,
}

#[derive(Debug)]
pub enum Problem {
    /// Not there (any more).
    Missing,
    /// Not safe to touch; the text says which file and why.
    Refused(String),
}

pub fn kind_name(m: MediaKind) -> &'static str {
    match m {
        MediaKind::Movie => "movie",
        MediaKind::Series => "series",
    }
}

fn stamp(m: &fs::Metadata) -> u128 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

fn in_bin(rel: &Path) -> bool {
    rel.components()
        .next()
        .is_some_and(|c| c.as_os_str() == BIN)
}

/// Checks that `rel` names a regular file inside `root` that is safe to move, and returns
/// where it is and its metadata.
pub fn check_file(root: &Path, rel: &str) -> Result<(PathBuf, fs::Metadata), Problem> {
    let refuse = |why: &str| Problem::Refused(format!("{rel}: {why}"));
    let rel_p =
        paths::safe_rel(rel).ok_or_else(|| refuse("not a plain path inside the library"))?;
    if in_bin(&rel_p) {
        return Err(refuse("already in the bin"));
    }
    let root_c = root
        .canonicalize()
        .map_err(|_| Problem::Refused("the library folder cannot be reached".into()))?;
    let abs = root_c.join(&rel_p);
    paths::check_no_symlinks(&root_c, &abs).map_err(|_| refuse("is or sits in a symbolic link"))?;
    let meta = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Problem::Missing),
        Err(_) => return Err(refuse("cannot be read")),
    };
    if !meta.file_type().is_file() {
        return Err(refuse("is not a regular file"));
    }
    match abs.canonicalize() {
        Ok(c) if c.starts_with(&root_c) && c != root_c => Ok((abs, meta)),
        _ => Err(refuse("is outside the library")),
    }
}

/// The files a delete of these videos takes: each video that is there, with the sidecars
/// beside it whose names start with the video's name. Returns them with how many videos
/// were not found. A file that is not safe to touch refuses the whole list.
pub fn list(root: &Path, videos: &[String]) -> Result<(Vec<BinFile>, usize), String> {
    let mut out: BTreeMap<String, BinFile> = BTreeMap::new();
    let mut missing = 0;
    for rel in videos {
        let (abs, meta) = match check_file(root, rel) {
            Ok(f) => f,
            Err(Problem::Missing) => {
                missing += 1;
                continue;
            }
            Err(Problem::Refused(why)) => return Err(why),
        };
        let rel_p = paths::safe_rel(rel).expect("checked");
        let key = paths::rel_string(&rel_p).expect("checked");
        out.entry(key.clone()).or_insert(BinFile {
            path: key,
            bytes: meta.len(),
            kind: "video".into(),
            stamp: stamp(&meta),
        });
        let (Some(dir), Some(name), Some(stem)) = (
            abs.parent(),
            abs.file_name().and_then(|n| n.to_str()),
            abs.file_stem().and_then(|n| n.to_str()),
        ) else {
            continue;
        };
        let Ok(rd) = fs::read_dir(dir) else { continue };
        for entry in rd.flatten() {
            let Some(other) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let ext = other.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
            let sidecar = other != name
                && other
                    .strip_prefix(stem)
                    .is_some_and(|rest| rest.starts_with('.'))
                && ext.is_some_and(|e| SIDECARS.contains(&e.as_str()));
            let Ok(m) = entry.metadata() else { continue };
            // A sidecar that is a link or a folder is left where it is.
            if !sidecar || !m.file_type().is_file() {
                continue;
            }
            let side = rel_p.parent().unwrap_or(Path::new("")).join(&other);
            if let Some(key) = paths::rel_string(&side) {
                out.entry(key.clone()).or_insert(BinFile {
                    path: key,
                    bytes: m.len(),
                    kind: "extra".into(),
                    stamp: stamp(&m),
                });
            }
        }
    }
    Ok((out.into_values().collect(), missing))
}

/// A digest over the exact files with their sizes and modified times. A delete presents the
/// one its preview gave and is refused when anything differs.
pub fn token(media_id: &str, files: &[BinFile]) -> String {
    let mut h = Sha256::new();
    h.update(media_id.as_bytes());
    for f in files {
        h.update(format!(
            "\n{}\0{}\0{}\0{}",
            f.path, f.kind, f.bytes, f.stamp
        ));
    }
    hex::encode(h.finalize())
}

pub enum Fail {
    /// A file differs from the preview.
    Changed(String),
    Refused(String),
    Io(String),
}

/// Removes empty folders from `from`'s folder upwards, never `stop` or anything outside it.
fn prune_empty(stop: &Path, from: &Path) {
    let mut p = from.parent();
    while let Some(d) = p {
        if d == stop || !d.starts_with(stop) || fs::remove_dir(d).is_err() {
            break;
        }
        p = d.parent();
    }
}

/// Moves the files into the bin, one `fs::rename` each. On a failure what already moved
/// goes back and nothing is copied.
pub fn mv(root: &Path, delete_id: &str, files: &[BinFile]) -> Result<(), Fail> {
    let io = |what: &str, e: std::io::Error| Fail::Io(format!("{what}: {}", e.kind()));
    let bin = paths::ensure_dirs(root, Path::new(BIN)).map_err(Fail::Refused)?;
    let ignore = bin.join(".plexignore");
    if fs::symlink_metadata(&ignore).is_err() {
        fs::write(&ignore, "*\n").map_err(|e| io("cannot write the bin", e))?;
    }
    let dir = paths::ensure_dirs(root, &Path::new(BIN).join(delete_id)).map_err(Fail::Refused)?;
    let root_c = root
        .canonicalize()
        .map_err(|e| io("the library folder cannot be reached", e))?;
    let mut moved: Vec<(PathBuf, PathBuf)> = vec![];
    let undo = |moved: &[(PathBuf, PathBuf)]| {
        for (src, dest) in moved.iter().rev() {
            if let Some(p) = src.parent() {
                let _ = fs::create_dir_all(p);
            }
            let _ = fs::rename(dest, src);
        }
        clear_empty(&dir);
    };
    for f in files {
        let step = (|| {
            let (src, meta) = check_file(root, &f.path).map_err(|p| match p {
                Problem::Missing => Fail::Changed(format!("{} is gone", f.path)),
                Problem::Refused(why) => Fail::Refused(why),
            })?;
            if meta.len() != f.bytes || stamp(&meta) != f.stamp {
                return Err(Fail::Changed(format!("{} changed", f.path)));
            }
            let dest = dir.join(&f.path);
            if let Some(p) = dest.parent() {
                fs::create_dir_all(p).map_err(|e| io("cannot make room in the bin", e))?;
            }
            fs::rename(&src, &dest).map_err(|e| io(&format!("cannot move {}", f.path), e))?;
            Ok((src, dest))
        })();
        match step {
            Ok(pair) => moved.push(pair),
            Err(e) => {
                undo(&moved);
                return Err(e);
            }
        }
    }
    for (src, _) in &moved {
        prune_empty(&root_c, src);
    }
    Ok(())
}

/// Removes `dir` and the folders in it that hold no files.
fn clear_empty(dir: &Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                clear_empty(&e.path());
            }
        }
    }
    let _ = fs::remove_dir(dir);
}

/// What a restore did.
#[derive(Debug, Default)]
pub struct Restored {
    pub restored: Vec<BinFile>,
    pub skipped: Vec<(String, &'static str)>,
}

/// Moves files back from the bin to where they were. A file whose place is taken stays in
/// the bin, as does one that cannot move.
pub fn put_back(root: &Path, delete_id: &str, files: &[BinFile]) -> Restored {
    let mut out = Restored::default();
    let Ok(bin) = root.canonicalize().map(|r| r.join(BIN)) else {
        out.skipped = files
            .iter()
            .map(|f| (f.path.clone(), "the library folder cannot be reached"))
            .collect();
        return out;
    };
    let dir = bin.join(delete_id);
    for f in files {
        let mut skip = |why| out.skipped.push((f.path.clone(), why));
        let Some(rel) = paths::safe_rel(&f.path).filter(|r| !in_bin(r)) else {
            skip("not a safe path");
            continue;
        };
        let src = dir.join(&rel);
        if !fs::symlink_metadata(&src).is_ok_and(|m| m.file_type().is_file()) {
            skip("it is no longer in the bin");
            continue;
        }
        let dest = root.join(&rel);
        if fs::symlink_metadata(&dest).is_ok() {
            skip("something is already there");
            continue;
        }
        let parent = rel.parent().unwrap_or(Path::new(""));
        if paths::ensure_dirs(root, parent).is_err() {
            skip("its folder cannot be made");
            continue;
        }
        if fs::rename(&src, &dest).is_err() {
            skip("it could not be moved");
            continue;
        }
        prune_empty(&bin, &src);
        out.restored.push(f.clone());
    }
    out
}

/// Deletes one bin entry's folder, and only that.
pub fn purge_dir(root: &Path, delete_id: &str) -> Result<()> {
    let bin = root.join(BIN);
    let dir = bin.join(delete_id);
    for p in [&bin, &dir] {
        match fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => anyhow::bail!("the bin is not a plain folder"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    fs::remove_dir_all(&dir)?;
    Ok(())
}

// ------------------------------------------------------------------ the entries

#[derive(Debug, Clone)]
pub struct Entry {
    pub delete_id: String,
    pub media_id: String,
    pub title: String,
    pub media: String,
    pub files: Vec<BinFile>,
    pub bytes: u64,
    pub deleted_at: String,
    pub purge_at: String,
}

impl Entry {
    pub fn view(&self) -> Value {
        json!({
            "delete_id": self.delete_id, "media_id": self.media_id, "title": self.title,
            "files": self.files.len(), "bytes": self.bytes,
            "deleted_at": self.deleted_at, "purge_at": self.purge_at,
        })
    }

    pub fn kind(&self) -> MediaKind {
        if self.media == "movie" {
            MediaKind::Movie
        } else {
            MediaKind::Series
        }
    }
}

const COLUMNS: &str = "delete_id, media_id, title, media, files, bytes, deleted_at, purge_at";

fn entry_row(r: &rusqlite::Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        delete_id: r.get(0)?,
        media_id: r.get(1)?,
        title: r.get(2)?,
        media: r.get(3)?,
        files: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
        bytes: r.get::<_, i64>(5)? as u64,
        deleted_at: r.get(6)?,
        purge_at: r.get(7)?,
    })
}

/// The paths (relative to their root) of every file now in the bin.
pub fn binned(c: &Connection) -> Result<std::collections::HashSet<String>> {
    Ok(entries(c)?
        .into_iter()
        .flat_map(|e| e.files)
        .map(|f| f.path)
        .collect())
}

pub fn insert(c: &Connection, e: &Entry) -> Result<()> {
    c.execute(
        &format!("INSERT INTO bin_entries ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"),
        params![
            e.delete_id,
            e.media_id,
            e.title,
            e.media,
            serde_json::to_string(&e.files)?,
            e.bytes as i64,
            e.deleted_at,
            e.purge_at
        ],
    )?;
    Ok(())
}

pub fn entries(c: &Connection) -> Result<Vec<Entry>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {COLUMNS} FROM bin_entries ORDER BY deleted_at DESC, delete_id"
    ))?;
    let rows = stmt.query_map([], entry_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn entry(c: &Connection, delete_id: &str) -> Result<Option<Entry>> {
    Ok(c.query_row(
        &format!("SELECT {COLUMNS} FROM bin_entries WHERE delete_id = ?1"),
        [delete_id],
        entry_row,
    )
    .optional()?)
}

/// Keeps only `files` in the entry (what a partial restore left), or drops it when none are.
pub fn keep(c: &Connection, delete_id: &str, files: &[BinFile]) -> Result<()> {
    if files.is_empty() {
        c.execute("DELETE FROM bin_entries WHERE delete_id = ?1", [delete_id])?;
    } else {
        c.execute(
            "UPDATE bin_entries SET files = ?2, bytes = ?3 WHERE delete_id = ?1",
            params![
                delete_id,
                serde_json::to_string(files)?,
                files.iter().map(|f| f.bytes).sum::<u64>() as i64
            ],
        )?;
    }
    Ok(())
}

pub fn purge_at(now: OffsetDateTime) -> String {
    rfc3339(now + time::Duration::days(KEEP_DAYS))
}

/// Deletes an entry's folder and forgets it. An entry whose folder cannot be reached stays.
pub async fn purge(st: &AppState, e: &Entry) -> Result<()> {
    if let Some(root) = st
        .cfg
        .library
        .as_ref()
        .and_then(|l| crate::ingest::root_for(l, e.kind()))
    {
        let (root, id) = (root.to_path_buf(), e.delete_id.clone());
        tokio::task::spawn_blocking(move || purge_dir(&root, &id)).await??;
    } else {
        anyhow::bail!("no library folder is set for {}", e.media);
    }
    let id = e.delete_id.clone();
    st.store.with(move |c| keep(c, &id, &[])).await
}

/// The periodic task: purges every entry whose seven days are over.
pub async fn purge_due(st: &AppState, now: OffsetDateTime) -> Result<Value> {
    let _guard = LOCK.lock().await;
    let all = st.store.with(|c| entries(c)).await?;
    let (mut purged, mut failed) = (0, 0);
    for e in all {
        if OffsetDateTime::parse(&e.purge_at, &Rfc3339).is_ok_and(|t| t <= now) {
            match purge(st, &e).await {
                Ok(()) => purged += 1,
                Err(err) => {
                    failed += 1;
                    tracing::warn!(error = %format!("{err:#}"), "could not empty a bin entry");
                }
            }
        }
    }
    Ok(json!({ "purged": purged, "failed": failed }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(root: &Path, rel: &str, body: &str) -> BinFile {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
        let m = fs::metadata(&p).unwrap();
        BinFile {
            path: rel.into(),
            bytes: m.len(),
            kind: "video".into(),
            stamp: stamp(&m),
        }
    }

    #[test]
    fn a_failed_move_puts_back_what_already_moved() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let a = file(root, "A (2000)/a.mkv", "a");
        let b = file(root, "B (2001)/b.mkv", "b");
        // Something in the bin where a folder must go: the second rename cannot happen.
        fs::create_dir_all(root.join(".arr-bin/x")).unwrap();
        fs::write(root.join(".arr-bin/x/B (2001)"), "in the way").unwrap();
        let r = mv(root, "x", &[a.clone(), b.clone()]);
        assert!(matches!(r, Err(Fail::Io(_))));
        assert_eq!(fs::read_to_string(root.join(&a.path)).unwrap(), "a");
        assert_eq!(fs::read_to_string(root.join(&b.path)).unwrap(), "b");
        assert!(!root.join(".arr-bin/x/A (2000)").exists());
    }

    #[test]
    fn only_files_that_are_safe_and_named_for_the_video_are_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let v = file(root, "M (1999)/M.1999.mkv", "v");
        file(root, "M (1999)/M.1999.en.srt", "s");
        file(root, "M (1999)/M.1999.nfo", "n");
        file(root, "M (1999)/M.1999 extended.srt", "other");
        file(root, "M (1999)/M.19990.srt", "other");
        file(root, "M (1999)/poster.jpg", "p");
        let (files, gone) = list(root, &[v.path.clone(), "M (1999)/missing.mkv".into()]).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            names,
            [
                "M (1999)/M.1999.en.srt",
                "M (1999)/M.1999.mkv",
                "M (1999)/M.1999.nfo"
            ]
        );
        assert_eq!(gone, 1);
        for bad in [
            "../x.mkv",
            "/etc/passwd",
            ".arr-bin/x/y.mkv",
            "a//b.mkv",
            "",
        ] {
            assert!(
                matches!(check_file(root, bad), Err(Problem::Refused(_))),
                "{bad}"
            );
        }
        assert!(matches!(
            check_file(root, "M (1999)"),
            Err(Problem::Refused(_))
        ));
    }
}
