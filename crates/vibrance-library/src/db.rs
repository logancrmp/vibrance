//! SQLite storage: schema migrations and the queries shared by `Library` and the scanner.
//!
//! Paths are stored as BLOBs of their raw bytes so non-UTF-8 file names round-trip exactly.
//! BLOBs compare with memcmp, which lets "everything under a directory" be an index range scan.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{
    CachedStatement, Connection, OptionalExtension, Row, Statement, TransactionBehavior, params,
};

use crate::{Result, Track, TrackId};

pub(crate) type FolderId = i64;

/// Each entry upgrades the schema by one `user_version`.
const MIGRATIONS: &[&str] = &[
    // v1
    "CREATE TABLE folders (
        id   INTEGER PRIMARY KEY,
        path BLOB NOT NULL UNIQUE
    );
    -- AUTOINCREMENT so a deleted track's id is never handed to another file:
    -- the UI persists queues by TrackId.
    CREATE TABLE tracks (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        path         BLOB NOT NULL UNIQUE,
        folder_id    INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
        title        TEXT NOT NULL,
        artist       TEXT NOT NULL,
        album_artist TEXT NOT NULL,
        album        TEXT NOT NULL,
        track_no     INTEGER,
        disc_no      INTEGER,
        year         INTEGER,
        genre        TEXT,
        duration_ms  INTEGER NOT NULL,
        format       TEXT NOT NULL,
        bitrate_kbps INTEGER,
        sample_rate  INTEGER,
        mtime        INTEGER NOT NULL,
        size         INTEGER NOT NULL,
        added_at     INTEGER NOT NULL
    );
    CREATE INDEX tracks_folder ON tracks(folder_id);
    CREATE TABLE kv (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    ) WITHOUT ROWID;",
    // v2: track gain. `gain_source` says whether the values came from tags or were measured.
    "ALTER TABLE tracks ADD COLUMN gain_db REAL;
    ALTER TABLE tracks ADD COLUMN peak REAL;
    ALTER TABLE tracks ADD COLUMN gain_source TEXT CHECK (gain_source IN ('tag', 'measured'));
    -- v1 never read gain tags: make the next scan re-read every file once.
    UPDATE tracks SET mtime = -1;",
];

pub(crate) fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut conn = Connection::open(path)?;
    // The UI's connection and a scan's connection write concurrently.
    conn.busy_timeout(Duration::from_secs(10))?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        log::warn!("library database is not in WAL mode ({mode})");
    }
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn schema_version(conn: &Connection) -> Result<usize> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let version = usize::try_from(version).unwrap_or(0);
    if version > MIGRATIONS.len() {
        return Err(std::io::Error::other(format!(
            "library database schema v{version} is newer than this build supports (v{})",
            MIGRATIONS.len()
        ))
        .into());
    }
    Ok(version)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    // Checked outside a write transaction first, so opening never waits on a running scan.
    if schema_version(conn)? == MIGRATIONS.len() {
        return Ok(());
    }
    // IMMEDIATE, then re-checked, so two processes opening a fresh database don't both migrate.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version = schema_version(&tx)?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// Byte range `[lo, hi)` covering every path strictly inside `dir`.
fn subtree_range(dir: &Path) -> (Vec<u8>, Vec<u8>) {
    let mut lo = path_bytes(dir).to_vec();
    if lo.last() != Some(&b'/') {
        lo.push(b'/');
    }
    let mut hi = lo.clone();
    *hi.last_mut().expect("non-empty") = b'/' + 1;
    (lo, hi)
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

// ---------------------------------------------------------------------------------------------
// Folders

/// Tracked folders, sorted by path.
pub(crate) fn folders(conn: &Connection) -> Result<Vec<(FolderId, PathBuf)>> {
    let mut stmt = conn.prepare_cached("SELECT id, path FROM folders ORDER BY path")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get(0)?, bytes_to_path(r.get_ref(1)?.as_blob()?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Inserts a folder if it isn't tracked yet. Returns its id and whether it was new.
pub(crate) fn insert_folder(conn: &Connection, path: &Path) -> Result<(FolderId, bool)> {
    let inserted = conn.execute(
        "INSERT INTO folders (path) VALUES (?1) ON CONFLICT(path) DO NOTHING",
        [path_bytes(path)],
    )? > 0;
    let id = conn.query_row(
        "SELECT id FROM folders WHERE path = ?1",
        [path_bytes(path)],
        |r| r.get(0),
    )?;
    Ok((id, inserted))
}

/// Folders that aren't inside another tracked folder. Only these are walked, and every
/// track belongs to the root it lives under, so nested folders can't produce duplicates.
pub(crate) fn roots(folders: &[(FolderId, PathBuf)]) -> Vec<(FolderId, PathBuf)> {
    folders
        .iter()
        .filter(|(id, path)| {
            !folders
                .iter()
                .any(|(other_id, other)| other_id != id && path.starts_with(other))
        })
        .cloned()
        .collect()
}

/// Moves every track under `dir` into folder `id` and returns the moved tracks.
/// Used when a newly added folder contains previously tracked ones.
pub(crate) fn adopt_subtree(conn: &Connection, id: FolderId, dir: &Path) -> Result<Vec<Track>> {
    let (lo, hi) = subtree_range(dir);
    let moved = conn.execute(
        "UPDATE tracks SET folder_id = ?1 WHERE path >= ?2 AND path < ?3 AND folder_id <> ?1",
        params![id, lo, hi],
    )?;
    if moved == 0 {
        return Ok(Vec::new());
    }
    let folder_paths = folder_map(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRACK_COLUMNS} FROM tracks WHERE path >= ?1 AND path < ?2"
    ))?;
    let rows = stmt.query_map(params![lo, hi], |r| track_from_row(r, &folder_paths))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Stops tracking `folder`. Its tracks move to any still-tracked folder that contains them;
/// the rest are deleted (by `ON DELETE CASCADE`).
pub(crate) fn remove_folder(conn: &mut Connection, folder: &Path) -> Result<()> {
    let canonical = std::fs::canonicalize(folder).ok();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let all = folders(&tx)?;
    let Some((id, path)) = all
        .iter()
        .find(|(_, p)| p == folder || Some(p) == canonical.as_ref())
        .cloned()
    else {
        return Ok(());
    };
    let remaining: Vec<_> = all.into_iter().filter(|(fid, _)| *fid != id).collect();
    for (new_id, new_root) in roots(&remaining) {
        if new_root.starts_with(&path) {
            let (lo, hi) = subtree_range(&new_root);
            tx.execute(
                "UPDATE tracks SET folder_id = ?1 WHERE folder_id = ?2 AND path >= ?3 AND path < ?4",
                params![new_id, id, lo, hi],
            )?;
        }
    }
    tx.execute("DELETE FROM folders WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(())
}

fn folder_map(conn: &Connection) -> Result<HashMap<FolderId, PathBuf>> {
    Ok(folders(conn)?.into_iter().collect())
}

// ---------------------------------------------------------------------------------------------
// Tracks

const TRACK_COLUMNS: &str = "id, path, folder_id, title, artist, album_artist, album, track_no, \
     disc_no, year, genre, duration_ms, format, bitrate_kbps, sample_rate, mtime, size, added_at, \
     gain_db, peak";

fn track_from_row(r: &Row<'_>, folders: &HashMap<FolderId, PathBuf>) -> rusqlite::Result<Track> {
    let folder_id: FolderId = r.get(2)?;
    Ok(Track {
        id: r.get(0)?,
        path: bytes_to_path(r.get_ref(1)?.as_blob()?),
        folder: folders.get(&folder_id).cloned().unwrap_or_default(),
        title: r.get(3)?,
        artist: r.get(4)?,
        album_artist: r.get(5)?,
        album: r.get(6)?,
        track_no: r.get(7)?,
        disc_no: r.get(8)?,
        year: r.get(9)?,
        genre: r.get(10)?,
        duration_ms: r.get::<_, i64>(11)? as u64,
        format: r.get(12)?,
        bitrate_kbps: r.get(13)?,
        sample_rate: r.get(14)?,
        mtime: r.get(15)?,
        size: r.get::<_, i64>(16)? as u64,
        added: r.get(17)?,
        gain_db: r.get(18)?,
        peak: r.get(19)?,
    })
}

pub(crate) fn load_tracks(conn: &Connection) -> Result<Vec<Track>> {
    let folder_paths = folder_map(conn)?;
    // Capacity hint; unlike count(*) this doesn't scan anything.
    let count: i64 = conn.query_row(
        "SELECT coalesce(max(id) - min(id) + 1, 0) FROM tracks",
        [],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare_cached(&format!("SELECT {TRACK_COLUMNS} FROM tracks"))?;
    let mut tracks = Vec::with_capacity(count as usize);
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        tracks.push(track_from_row(row, &folder_paths)?);
    }
    Ok(tracks)
}

/// What the scanner needs to decide whether a file changed.
pub(crate) struct Known {
    pub id: TrackId,
    pub folder_id: FolderId,
    pub mtime: i64,
    pub size: u64,
}

/// Every stored track keyed by its raw path bytes.
pub(crate) fn known_files(conn: &Connection) -> Result<HashMap<Vec<u8>, Known>> {
    let mut stmt = conn.prepare("SELECT path, id, folder_id, mtime, size FROM tracks")?;
    let mut rows = stmt.query([])?;
    let mut known = HashMap::new();
    while let Some(r) = rows.next()? {
        known.insert(
            r.get(0)?,
            Known {
                id: r.get(1)?,
                folder_id: r.get(2)?,
                mtime: r.get(3)?,
                size: r.get::<_, i64>(4)? as u64,
            },
        );
    }
    Ok(known)
}

/// On update, `added_at` is kept, and gain from tags replaces whatever was stored. Without gain
/// tags, measured gain is kept and stale tag gain is cleared. (SET expressions see the old row.)
const UPSERT: &str = "INSERT INTO tracks (path, folder_id, title, artist, album_artist, album, \
     track_no, disc_no, year, genre, duration_ms, format, bitrate_kbps, sample_rate, mtime, size, \
     added_at, gain_db, peak, gain_source) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, \
     ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20) \
     ON CONFLICT(path) DO UPDATE SET folder_id = excluded.folder_id, title = excluded.title, \
     artist = excluded.artist, album_artist = excluded.album_artist, album = excluded.album, \
     track_no = excluded.track_no, disc_no = excluded.disc_no, year = excluded.year, \
     genre = excluded.genre, duration_ms = excluded.duration_ms, format = excluded.format, \
     bitrate_kbps = excluded.bitrate_kbps, sample_rate = excluded.sample_rate, \
     mtime = excluded.mtime, size = excluded.size, \
     gain_db = CASE WHEN excluded.gain_source IS NOT NULL THEN excluded.gain_db \
         WHEN gain_source = 'measured' THEN gain_db END, \
     peak = CASE WHEN excluded.gain_source IS NOT NULL THEN excluded.peak \
         WHEN gain_source = 'measured' THEN peak END, \
     gain_source = CASE WHEN excluded.gain_source IS NOT NULL THEN excluded.gain_source \
         WHEN gain_source = 'measured' THEN gain_source END \
     RETURNING id, added_at, gain_db, peak";

pub(crate) fn upsert_statement(conn: &Connection) -> rusqlite::Result<CachedStatement<'_>> {
    conn.prepare_cached(UPSERT)
}

/// Inserts or updates a track by path. `track.gain_db`/`peak` are the values from its tags;
/// afterwards `track` holds what was stored: its id, `added`, and the effective gain.
pub(crate) fn upsert_track(
    stmt: &mut Statement<'_>,
    track: &mut Track,
    folder_id: FolderId,
    now: i64,
) -> rusqlite::Result<()> {
    let source = track.gain_db.map(|_| "tag");
    (track.id, track.added, track.gain_db, track.peak) = stmt.query_row(
        params![
            path_bytes(&track.path),
            folder_id,
            track.title,
            track.artist,
            track.album_artist,
            track.album,
            track.track_no,
            track.disc_no,
            track.year,
            track.genre,
            track.duration_ms as i64,
            track.format,
            track.bitrate_kbps,
            track.sample_rate,
            track.mtime,
            track.size as i64,
            now,
            track.gain_db,
            track.peak,
            source,
        ],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok(())
}

pub(crate) fn delete_tracks(conn: &mut Connection, ids: &[TrackId]) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    {
        let mut stmt = tx.prepare_cached("DELETE FROM tracks WHERE id = ?1")?;
        for id in ids {
            stmt.execute([id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Gain

/// Stores measured gain unless the track's gain came from tags (tags always win).
pub(crate) fn set_measured_gain(
    conn: &Connection,
    id: TrackId,
    gain_db: f32,
    peak: f32,
) -> Result<()> {
    if !gain_db.is_finite() || !peak.is_finite() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("measured gain must be finite (gain {gain_db}, peak {peak})"),
        )
        .into());
    }
    conn.prepare_cached(
        "UPDATE tracks SET gain_db = ?1, peak = ?2, gain_source = 'measured' \
         WHERE id = ?3 AND gain_source IS NOT 'tag'",
    )?
    .execute(params![gain_db, peak, id])?;
    Ok(())
}

pub(crate) fn tracks_without_gain(conn: &Connection) -> Result<Vec<(TrackId, PathBuf)>> {
    let mut stmt =
        conn.prepare_cached("SELECT id, path FROM tracks WHERE gain_db IS NULL ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get(0)?, bytes_to_path(r.get_ref(1)?.as_blob()?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ---------------------------------------------------------------------------------------------
// Key/value state

pub(crate) fn get_state(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT value FROM kv WHERE key = ?1")?
        .query_row([key], |r| r.get(0))
        .optional()?)
}

pub(crate) fn set_state(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO kv (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )?
    .execute([key, value])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtree_range_bounds() {
        let (lo, hi) = subtree_range(Path::new("/music/rock"));
        assert_eq!(lo, b"/music/rock/");
        assert_eq!(hi, b"/music/rock0");
        let inside = b"/music/rock/a.mp3".as_slice();
        let sibling = b"/music/rock2/a.mp3".as_slice();
        assert!(inside >= lo.as_slice() && inside < hi.as_slice());
        assert!(!(sibling >= lo.as_slice() && sibling < hi.as_slice()));

        let (lo, hi) = subtree_range(Path::new("/"));
        assert_eq!(
            (lo.as_slice(), hi.as_slice()),
            (b"/".as_slice(), b"0".as_slice())
        );
    }

    #[test]
    fn roots_skip_nested() {
        let folders = vec![
            (1, PathBuf::from("/a")),
            (2, PathBuf::from("/a/b")),
            (3, PathBuf::from("/ab")),
        ];
        let ids: Vec<_> = roots(&folders).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, [1, 3]);
    }
}
