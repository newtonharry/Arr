//! "Delete files": a movie, show, season or episode leaves the library for the bin, where it
//! can be put back for seven days. A preview says exactly what would go and gives a token;
//! the delete must present it and is refused when anything changed since. Downloads that
//! only seed those files go with them (their data is what keeps a hardlinked copy's space).

use super::titles::within;
use super::*;
use crate::scan;
use crate::trash::{self, BinFile, Entry, Fail};
use rusqlite::params;
use std::path::{Path as FsPath, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreviewBody {
    media_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteBody {
    media_id: String,
    token: String,
    #[serde(default)]
    keep_wanted: bool,
}

/// What a delete covers.
struct Scope {
    id: MediaId,
    title: String,
    media: MediaKind,
    root: PathBuf,
    /// The owned files Arr knows the place of, relative to the root.
    videos: Vec<String>,
    /// Owned items whose file Arr does not know the place of.
    unknown: usize,
    /// The `files` rows behind each video, read with the scope (before anything moves).
    rows: std::collections::HashMap<String, Vec<i64>>,
}

impl Scope {
    fn inside(&self, t: MediaId) -> bool {
        inside_of(self.id, t)
    }

    /// The title the scope belongs to (a show for a season or an episode).
    fn owner(&self) -> MediaId {
        plan_media(self.id)
    }
}

fn inside_of(scope: MediaId, t: MediaId) -> bool {
    match scope {
        MediaId::Series(k) => t.series_key() == Some(k),
        scope => within(scope, t),
    }
}

fn refused(message: impl Into<String>) -> ApiError {
    ApiError::conflict(ErrorCode::DeleteRefused, message)
}

#[deny(clippy::wildcard_enum_match_arm)]
async fn scope(st: &AppState, raw: &str) -> Result<Scope, ApiError> {
    let id = parse_media_id(raw)?;
    // Sport files can't be deleted from Arr yet: say so before looking for a folder.
    if let MediaId::Event(_) = id {
        return Err(refused("sport files can't be deleted from Arr yet"));
    }
    let Some(root) = st
        .cfg
        .library
        .as_ref()
        .and_then(|l| crate::ingest::root_for(l, crate::downloads::policy_kind(id)))
        .map(|p| p.to_path_buf())
    else {
        return Err(refused("there is no library folder to delete from"));
    };
    let index = st.index();
    let missing = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotManaged,
            format!("{id} is not something Arr knows"),
        )
    };
    let (id, title, unknown, item_root) = match id {
        MediaId::Movie { tmdb } => {
            let m = index.movie(tmdb).ok_or_else(missing)?;
            let unknown =
                usize::from(m.has_file && m.file.as_ref().is_none_or(|f| f.path.is_none()));
            (id, m.title.clone(), unknown, m.root_folder.clone())
        }
        MediaId::Series(k) | MediaId::Season(k, _) | MediaId::Episode(k, ..) => {
            let s = index.series(k).ok_or_else(missing)?;
            // Stored ids use the series key the catalog knows the show by.
            let key = s.id.series_key().unwrap_or(k);
            let id = match id {
                MediaId::Season(_, n) => MediaId::Season(key, n),
                MediaId::Episode(_, n, e) => MediaId::Episode(key, n, e),
                MediaId::Movie { .. } | MediaId::Series(_) | MediaId::Event(_) => s.id,
            };
            let unknown = s
                .episodes
                .iter()
                .flatten()
                .filter(|e| {
                    e.has_file
                        && e.file.as_ref().is_none_or(|f| f.path.is_none())
                        && inside_of(id, library::episode_id(s.id, e.season, e.number))
                })
                .count();
            (id, s.title.clone(), unknown, s.root_folder.clone())
        }
        MediaId::Event(_) => return Err(refused("sport files can't be deleted from Arr yet")),
    };
    // The same root check import and scan make: evidence from another root must not be
    // resolved against this one.
    let media = crate::downloads::policy_kind(id);
    if !crate::ingest::root_matches(st, media, item_root.as_deref()) {
        return Err(refused(
            "this title's folder is not the library folder Arr is set up to use",
        ));
    }
    // Every present file row of the title, whichever copy the catalog shows.
    let owner = plan_media(id).to_string();
    let rows: Vec<(i64, String, String)> = st
        .store
        .with(move |c| {
            let mut stmt = c.prepare(
                "SELECT f.id, f.path, t.target FROM files f JOIN file_targets t ON t.file_id = f.id
                 WHERE f.present = 1 AND f.media_id = ?1 AND f.path IS NOT NULL ORDER BY f.id",
            )?;
            let rows = stmt.query_map([owner], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .map_err(internal("delete"))?;
    let (mut videos, mut kept) = (vec![], vec![]);
    let mut ids: std::collections::HashMap<String, Vec<i64>> = Default::default();
    for (row, path, target) in rows {
        let Ok(t) = target.parse::<MediaId>() else {
            continue;
        };
        if inside_of(id, t) {
            let list = ids.entry(path.clone()).or_default();
            if !list.contains(&row) {
                list.push(row);
            }
            if !videos.contains(&path) {
                videos.push(path);
            }
        } else {
            kept.push(path);
        }
    }
    // One file can hold several episodes; it cannot go while one of them stays.
    if let Some(p) = videos.iter().find(|p| kept.contains(p)) {
        return Err(refused(format!(
            "{p} also holds episodes that are staying, so it cannot go on its own"
        )));
    }
    Ok(Scope {
        id,
        title,
        media: crate::downloads::policy_kind(id),
        root,
        videos,
        unknown,
        rows: ids,
    })
}

/// The files that would go, as the disk has them now.
async fn files_of(sc: &Scope) -> Result<Vec<BinFile>, ApiError> {
    let (root, videos) = (sc.root.clone(), sc.videos.clone());
    let (files, _gone) = tokio::task::spawn_blocking(move || trash::list(&root, &videos))
        .await
        .map_err(|e| ApiError::internal("delete", e))?
        .map_err(refused)?;
    Ok(files)
}

/// The title's downloads a delete meets: ones still going and ones finished but still seeding.
async fn downloads_of(st: &AppState, sc: &Scope) -> Result<Vec<downloads::Grab>, ApiError> {
    let pid = sc.owner().to_string();
    st.store
        .with(move |c| {
            downloads::select(
                c,
                "media_id = ?1 AND (active = 1 OR (state = 'imported' AND cleanup NOT IN ('removing', 'cleaned', 'client_removed')))",
                &[&pid],
            )
        })
        .await
        .map_err(internal("downloads"))
}

fn covers(g: &downloads::Grab) -> Vec<MediaId> {
    if g.targets.is_empty() {
        vec![g.target]
    } else {
        g.targets.iter().map(|t| t.target).collect()
    }
}

/// What a delete does with one of the title's downloads.
struct DownloadPlan {
    grab: downloads::Grab,
    removed: bool,
    reason: Option<&'static str>,
    /// Everything it holds is inside the delete, but it is being added to Plex.
    finishing: bool,
}

/// Several grabs can be one torrent in the client, so a torrent goes only when every grab
/// that uses it (going or finished) lies wholly inside the delete and none is being added.
async fn plan_downloads(st: &AppState, sc: &Scope) -> Result<Vec<DownloadPlan>, ApiError> {
    let mut out = vec![];
    for grab in downloads_of(st, sc).await? {
        let hash = grab.info_hash.clone();
        let group = if hash.is_empty() {
            vec![grab.clone()]
        } else {
            st.store
                .with(move |c| {
                    downloads::select(
                        c,
                        "info_hash = ?1 AND (active = 1 OR (state = 'imported' AND cleanup NOT IN ('removing', 'cleaned', 'client_removed')))",
                        &[&hash],
                    )
                })
                .await
                .map_err(internal("downloads"))?
        };
        let inside = group
            .iter()
            .all(|x| covers(x).into_iter().all(|t| sc.inside(t)));
        let adding = group.iter().any(|x| {
            x.active && !matches!(x.state.as_str(), "queued" | "unconfirmed" | "downloading")
        });
        let (removed, reason, finishing) = if !inside {
            (
                false,
                Some("It also holds episodes that are staying, so it stays too."),
                false,
            )
        } else if adding {
            (
                false,
                Some("It is being added to Plex right now. Try again in a minute."),
                true,
            )
        } else {
            (true, None, false)
        };
        out.push(DownloadPlan {
            grab,
            removed,
            reason,
            finishing,
        });
    }
    Ok(out)
}

/// A whole show cannot leave Arr while one of its downloads is going: when that download
/// must stay because another title shares its torrent, deleting the show (unless a new copy
/// is wanted) is refused up front, before any file moves.
fn whole_show_blocked(sc: &Scope, plans: &[DownloadPlan]) -> Option<&'static str> {
    (matches!(sc.id, MediaId::Series(_)) && plans.iter().any(|p| !p.removed && p.grab.active))
        .then_some(
            "A download of this show is shared with another title and has not finished, so the show cannot be removed from Arr yet. Try again when it is done, or choose to get a new copy.",
        )
}

/// What a delete presents: the files' token and, for each download the delete meets, whether
/// it would go (in a stable order). A download that appears or changes between the preview and
/// the delete changes the token, so it is never cancelled under a token that did not show it.
fn delete_token(media_id: &str, files: &[trash::BinFile], plans: &[DownloadPlan]) -> String {
    use sha2::{Digest, Sha256};
    let mut grabs: Vec<String> = plans
        .iter()
        .map(|p| format!("{}\0{}", p.grab.id, p.removed))
        .collect();
    grabs.sort();
    let mut h = Sha256::new();
    h.update(trash::token(media_id, files).as_bytes());
    for g in grabs {
        h.update(format!("\n{g}"));
    }
    hex::encode(h.finalize())
}

/// `POST /delete/preview`: what a delete of this movie, show, season or episode would take.
pub(super) async fn preview(State(st): St, ApiJson(body): ApiJson<PreviewBody>) -> ApiResult {
    st.loaded()?;
    let sc = scope(&st, &body.media_id).await?;
    let files = files_of(&sc).await?;
    let mut shown = vec![];
    let plans = plan_downloads(&st, &sc).await?;
    let blocked = whole_show_blocked(&sc, &plans);
    let token = delete_token(&sc.id.to_string(), &files, &plans);
    for p in plans {
        let g = p.grab;
        let mut row =
            json!({ "grab_id": g.id, "title": g.title, "bytes": g.size, "removed": p.removed });
        if let Some(r) = p.reason {
            row["reason"] = json!(r);
        }
        shown.push(row);
    }
    ok(json!({
        "media_id": sc.id, "title": sc.title, "files": files,
        "library_bytes": files.iter().map(|f| f.bytes).sum::<u64>(),
        "downloads": shown, "unknown": sc.unknown, "blocked": blocked,
        "token": token,
    }))
}

/// `POST /delete`: moves the files into the bin and stops wanting them. A request that is
/// dropped (the server deadline, a client that gives up) must not split the move from its
/// record, so the delete runs as its own task, which holds the bin's lock, and the request
/// waits for it.
pub(super) async fn delete(State(st): St, ApiJson(body): ApiJson<DeleteBody>) -> ApiResult {
    st.loaded()?;
    tokio::spawn(delete_unit(st, body))
        .await
        .map_err(|e| ApiError::internal("delete", e))?
}

async fn delete_unit(st: Arc<AppState>, body: DeleteBody) -> ApiResult {
    let _guard = trash::LOCK.lock().await;
    let now = OffsetDateTime::now_utc();
    let sc = scope(&st, &body.media_id).await?;
    let mut files = files_of(&sc).await?;
    let id_s = sc.id.to_string();
    if files.is_empty() {
        return Err(refused("There are no files Arr knows of to delete."));
    }
    for f in files.iter_mut().filter(|f| f.kind == "video") {
        f.rows = sc.rows.get(&f.path).cloned().unwrap_or_default();
    }
    // Downloads whose torrent is wanted by nothing outside this delete end, with their data,
    // before the files move (removing one checks that the library copy it came from is still
    // there). A download being added to Plex is left to finish and the delete waits. One that
    // cannot be removed stays and keeps seeding. Decided and done under the grabs lock.
    let mut seeding = 0;
    let mut removed: Vec<String> = vec![];
    {
        let _grabs = st.worker.grabs.lock().await;
        let plans = plan_downloads(&st, &sc).await?;
        // The files and the downloads must be what the preview showed, before anything is
        // cancelled or moved.
        if delete_token(&id_s, &files, &plans) != body.token {
            return Err(ApiError::conflict(
                ErrorCode::Changed,
                "The files changed since you looked. Look again before deleting.",
            ));
        }
        if plans.iter().any(|p| p.finishing) {
            return Err(ApiError::conflict(
                ErrorCode::ActiveDownloads,
                "A download is nearly done and is being added to Plex. Try again in a minute.",
            ));
        }
        if !body.keep_wanted
            && let Some(why) = whole_show_blocked(&sc, &plans)
        {
            return Err(refused(why));
        }
        let going = plans.iter().filter(|p| p.removed && p.grab.active);
        let done = plans.iter().filter(|p| p.removed && !p.grab.active);
        for p in going.chain(done) {
            let r = downloads::cancel_locked(
                &st,
                &p.grab.id,
                CancelOptions {
                    blocklist: false,
                    removal: Removal::TorrentAndData,
                    ignore_seed_criteria: true,
                    research: false,
                    only_unfinished: p.grab.active,
                },
            )
            .await;
            match r {
                Ok(_) => removed.push(p.grab.title.clone()),
                Err(downloads::ActionError::Conflict("finishing", _)) => {
                    return Err(ApiError::conflict(
                        ErrorCode::ActiveDownloads,
                        "A download is nearly done and is being added to Plex. Try again in a minute.",
                    ));
                }
                Err(e) if p.grab.active => {
                    return Err(action_error(
                        e,
                        st.cfg.download_backend().unwrap_or("download_client"),
                    ));
                }
                Err(_) => seeding += 1,
            }
        }
    }
    // The files move, or none do.
    let delete_id = crate::fsutil::random_hex(8).map_err(internal("delete"))?;
    let bytes: u64 = files.iter().map(|f| f.bytes).sum();
    let entry = Entry {
        delete_id: delete_id.clone(),
        media_id: id_s.clone(),
        title: sc.title.clone(),
        media: trash::kind_name(sc.media).into(),
        files: files.clone(),
        bytes,
        deleted_at: rfc3339(now),
        purge_at: trash::purge_at(now),
    };
    // The entry is written before anything moves, so a crash once the files are in the bin
    // still leaves a way back. (After a crash before the move, its restore finds nothing in
    // the bin and its purge drops it.)
    let pending = entry.clone();
    st.store
        .with(move |c| trash::insert(c, &pending))
        .await
        .map_err(internal("delete"))?;
    let (root, did, moving) = (sc.root.clone(), delete_id.clone(), files.clone());
    let moved = tokio::task::spawn_blocking(move || trash::mv(&root, &did, &moving))
        .await
        .map_err(|e| ApiError::internal("delete", e))?;
    if let Err(f) = moved {
        // A file the move could not put back is still in the bin: the entry stays for it.
        if !sc.root.join(trash::BIN).join(&delete_id).exists() {
            let did = delete_id.clone();
            let _ = st.store.with(move |c| trash::keep(c, &did, &[])).await;
        }
        // Downloads that already went with their data are named: they are not coming back.
        let said = |why: String, none: &str| {
            if removed.is_empty() {
                format!("{why}{none}")
            } else {
                format!(
                    "{why}. No file was moved, but these downloads were already removed with their data: {}.",
                    removed.join(", ")
                )
            }
        };
        return Err(match f {
            Fail::Changed(why) => ApiError::conflict(ErrorCode::Changed, said(why, "")),
            Fail::Refused(why) => refused(said(why, "")),
            Fail::Io(why) => {
                ApiError::conflict(ErrorCode::MoveFailed, said(why, ". Nothing was deleted."))
            }
        });
    }
    if trash::test_point(&sc.root, "crash_after_move") {
        return Err(ApiError::internal("delete", "stopped for a crash test"));
    }
    let owner = sc.owner().to_string();
    let plex = st.plex.is_some();
    let (mut entry2, owner2, id2) = (entry.clone(), owner.clone(), id_s.clone());
    let root2 = sc.root.clone();
    let recorded = st
        .store
        .with(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let at = &entry2.deleted_at;
            // The files are gone from the library: no longer owned, and the item counts as
            // scanned so an older snapshot's flag cannot bring the ownership back.
            // The rows retired (read before the files moved) and the exact imports marked deleted are kept with the
            // entry, so a restore brings back those and no others.
            for f in entry2.files.iter_mut().filter(|f| f.kind == "video") {
                for id in &f.rows {
                    tx.execute("UPDATE files SET present = 0 WHERE id = ?1", [id])?;
                }
                let mut stmt = tx.prepare(
                    "SELECT id, root FROM import_journal WHERE dest = ?1 AND deleted_at IS NULL AND state IN ('committed', 'backup_removed')",
                )?;
                let imports: Vec<(i64, String)> = stmt
                    .query_map([&f.path], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                drop(stmt);
                for (id, root) in imports {
                    if crate::trash::same_dir(FsPath::new(&root), &root2) {
                        tx.execute(
                            "UPDATE import_journal SET deleted_at = ?2 WHERE id = ?1",
                            params![id, at],
                        )?;
                        f.journal.push(id);
                    }
                }
            }
            tx.execute(
                "UPDATE media SET scanned_at = ?2 WHERE id = ?1",
                params![owner2, at],
            )?;
            trash::keep(&tx, &entry2.delete_id, &entry2.files)?;
            library::event(
                &tx,
                at,
                "deleted",
                Some(&owner2),
                Some(&id2),
                None,
                &format!("{}: {} file(s) moved to the bin", entry2.title, entry2.files.len()),
                Some(&json!({ "delete_id": entry2.delete_id, "files": entry2.files.len(), "bytes": entry2.bytes })),
            )?;
            if plex {
                worker::queue_command(&tx, "plex_refresh", id2.parse().ok(), at)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await;
    if let Err(e) = recorded {
        // Without a record there would be no way back: put the files where they were.
        let (root, did, back) = (sc.root.clone(), delete_id.clone(), files.clone());
        let done = tokio::task::spawn_blocking(move || trash::put_back(&root, &did, &back)).await;
        let left: Vec<BinFile> = files
            .iter()
            .filter(|f| {
                !done
                    .as_ref()
                    .is_ok_and(|d| d.restored.iter().any(|r| r.path == f.path))
            })
            .cloned()
            .collect();
        let did = delete_id.clone();
        let _ = st.store.with(move |c| trash::keep(c, &did, &left)).await;
        return Err(internal("delete")(e));
    }
    st.reload_index().await.map_err(internal("index"))?;
    st.worker.notify.notify_one();
    // Stop wanting it, unless a new copy is wanted.
    if !body.keep_wanted {
        let r = match sc.id {
            MediaId::Series(_) => super::titles::remove_title(&st, &id_s, false).await,
            _ => super::titles::stop_scope(&st, &id_s, false).await,
        };
        // Not tracked any more is the end state wanted.
        if let Err(e) = r
            && e.code != ErrorCode::NotManaged
        {
            return Err(e);
        }
    }
    ok(json!({
        "delete_id": delete_id, "moved": files.len(), "bytes": bytes,
        "purge_at": entry.purge_at, "downloads_kept": seeding,
    }))
}

/// `GET /bin`: what a delete left, newest first.
pub(super) async fn bin(State(st): St) -> ApiResult {
    st.loaded()?;
    let all = st
        .store
        .with(|c| trash::entries(c))
        .await
        .map_err(internal("bin"))?;
    ok(json!({ "entries": all.iter().map(Entry::view).collect::<Vec<_>>() }))
}

async fn find(st: &AppState, raw: &str) -> Result<Entry, ApiError> {
    let id = raw.to_string();
    st.store
        .with(move |c| trash::entry(c, &id))
        .await
        .map_err(internal("bin"))?
        .ok_or_else(|| ApiError::not_found("nothing in the bin has that id"))
}

fn root_of(st: &AppState, e: &Entry) -> Result<PathBuf, ApiError> {
    st.cfg
        .library
        .as_ref()
        .and_then(|l| crate::ingest::root_for(l, e.kind()))
        .map(|p| p.to_path_buf())
        .ok_or_else(|| refused("there is no library folder to restore into"))
}

/// `POST /bin/{delete_id}/restore`: puts the files back where they were. A file whose place
/// is taken stays in the bin and is reported. Nothing is wanted again.
pub(super) async fn restore(State(st): St, Path(delete_id): Path<String>) -> ApiResult {
    st.loaded()?;
    let _guard = trash::LOCK.lock().await;
    let now = OffsetDateTime::now_utc();
    let e = find(&st, &delete_id).await?;
    let root = root_of(&st, &e)?;
    let (r2, id2, files) = (root, e.delete_id.clone(), e.files.clone());
    let done = tokio::task::spawn_blocking(move || trash::put_back(&r2, &id2, &files))
        .await
        .map_err(|e| ApiError::internal("restore", e))?;
    let left: Vec<BinFile> = e
        .files
        .iter()
        .filter(|f| !done.restored.iter().any(|r| r.path == f.path))
        .cloned()
        .collect();
    let mid: Option<MediaId> = e.media_id.parse().ok();
    let owner = mid.map(plan_media);
    let (did, owner_s, n) = (
        e.delete_id.clone(),
        owner.map(|o| o.to_string()),
        done.restored.len(),
    );
    let back = done.restored.clone();
    let (left2, plex, title, bytes) = (
        left,
        st.plex.is_some(),
        e.title.clone(),
        done.restored.iter().map(|f| f.bytes).sum::<u64>(),
    );
    st.store
        .with(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            trash::keep(&tx, &did, &left2)?;
            // Own them again as they were: a scan cannot find a file whose name says nothing
            // about what it is. The very rows the delete retired come back.
            if let Some(o) = &owner_s {
                for f in back.iter().filter(|f| f.kind == "video") {
                    for id in &f.rows {
                        tx.execute(
                            "UPDATE files SET present = 1 WHERE id = ?1
                             AND NOT EXISTS (SELECT 1 FROM files WHERE media_id = ?2 AND path = ?3 AND present = 1)",
                            params![id, o, f.path],
                        )?;
                    }
                    for id in &f.journal {
                        tx.execute(
                            "UPDATE import_journal SET deleted_at = NULL WHERE id = ?1",
                            [id],
                        )?;
                    }
                }
            }
            if n > 0 {
                let at = rfc3339(now);
                library::event(
                    &tx,
                    &at,
                    "restored",
                    owner_s.as_deref(),
                    owner_s.as_deref(),
                    None,
                    &format!("{title}: {n} file(s) put back from the bin"),
                    Some(&json!({ "delete_id": did, "files": n, "bytes": bytes })),
                )?;
                if plex {
                    worker::queue_command(&tx, "plex_refresh", owner, &at)?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(internal("restore"))?;
    // The files are owned again: look at the title's folder.
    let ownership = match owner {
        Some(o) if n > 0 => scan::scan_item(&st, o, now)
            .await
            .map_err(internal("restore"))?,
        _ => json!({ "status": "not_scanned" }),
    };
    st.worker.notify.notify_one();
    ok(json!({
        "delete_id": e.delete_id, "media_id": e.media_id, "restored": n,
        "skipped": done.skipped.iter().map(|(p, why)| json!({ "path": p, "reason": why })).collect::<Vec<_>>(),
        "ownership": ownership["status"],
    }))
}

/// `DELETE /bin/{delete_id}`: empties one entry now. Its space comes back.
pub(super) async fn purge(State(st): St, Path(delete_id): Path<String>) -> ApiResult {
    st.loaded()?;
    let _guard = trash::LOCK.lock().await;
    let e = find(&st, &delete_id).await?;
    trash::purge(&st, &e).await.map_err(internal("bin"))?;
    ok(json!({ "delete_id": e.delete_id, "purged": true, "bytes": e.bytes }))
}
