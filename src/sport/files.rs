//! The files sport has filed (`sport_files`). The movie and series `files` table is never
//! written for an event, and this table is never read by the scan or the catalogue.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// One filed copy: an event's part, where it is (relative to the sport root) and what it is.
#[derive(Debug, Clone)]
pub struct FileRow {
    pub event_id: String,
    pub part: String,
    pub path: String,
    pub size: Option<i64>,
    pub quality: Option<String>,
    pub imported_utc: String,
}

/// Records a filed copy. Called in the transaction that commits the import, so a file and its
/// row appear together. The unique index refuses a second present file for the same part.
#[allow(clippy::too_many_arguments)]
pub fn insert(
    c: &Connection,
    event_id: &str,
    part: &str,
    path: &str,
    size: u64,
    quality: Option<&str>,
    probe: &str,
    grab_id: &str,
    now_s: &str,
) -> Result<()> {
    c.execute(
        "INSERT INTO sport_files (event_id, part, path, size, quality, probe, grab_id, present, imported_utc, observed_utc)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)",
        params![event_id, part, path, size as i64, quality, probe, grab_id, now_s],
    )?;
    Ok(())
}

/// Whether the event has a present file for the part.
pub fn present(c: &Connection, event_id: &str, part: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT 1 FROM sport_files WHERE event_id = ?1 AND part = ?2 AND present = 1",
        params![event_id, part],
        |_| Ok(()),
    )
    .optional()?
    .is_some())
}

/// The grab that filed the present file of an event's part, if there is one (`Some(None)`: a
/// file recorded without a grab).
pub fn filed_by(c: &Connection, event_id: &str, part: &str) -> Result<Option<Option<String>>> {
    Ok(c.query_row(
        "SELECT grab_id FROM sport_files WHERE event_id = ?1 AND part = ?2 AND present = 1",
        params![event_id, part],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()?)
}

/// Every present file of an event, in the order they were filed.
pub fn for_event(c: &Connection, event_id: &str) -> Result<Vec<FileRow>> {
    let mut stmt = c.prepare(
        "SELECT event_id, part, path, size, quality, imported_utc FROM sport_files WHERE event_id = ?1 AND present = 1 ORDER BY id",
    )?;
    Ok(stmt
        .query_map([event_id], |r| {
            Ok(FileRow {
                event_id: r.get(0)?,
                part: r.get(1)?,
                path: r.get(2)?,
                size: r.get(3)?,
                quality: r.get(4)?,
                imported_utc: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}
