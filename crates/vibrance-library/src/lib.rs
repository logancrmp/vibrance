//! Music library: folder scanning, tag reading, the SQLite store, and album art.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::Connection;

mod art;
mod db;
mod scan;
mod tags;
mod watch;

pub use tags::{SUPPORTED_EXTENSIONS, UNKNOWN_ALBUM, UNKNOWN_ARTIST, is_supported};
pub use watch::{WatchEvent, Watcher, watch};

pub type TrackId = i64;

#[derive(Debug, Clone)]
pub struct Track {
    pub id: TrackId,
    pub path: PathBuf,
    /// The tracked folder this file was found under.
    pub folder: PathBuf,
    pub title: String,
    pub artist: String,
    /// Album artist, falling back to the track artist (SPEC 4.1).
    pub album_artist: String,
    pub album: String,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub duration_ms: u64,
    /// Short format name, e.g. "FLAC", "MP3".
    pub format: String,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    /// File modification time in nanoseconds since the Unix epoch.
    pub mtime: i64,
    /// File size in bytes.
    pub size: u64,
    /// When the track first entered the library, in Unix seconds.
    pub added: i64,
    /// Track gain in dB toward the ReplayGain 2.0 reference (-18 LUFS), from tags or measured
    /// with [`Library::set_measured_gain`].
    pub gain_db: Option<f32>,
    /// Track peak as a linear sample value (1.0 = full scale).
    pub peak: Option<f32>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tag error: {0}")]
    Tag(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The library database. Opening it never touches the music folders.
///
/// `Library` is `Send + Sync`; calls are serialized on one connection. Calls do disk I/O, so
/// keep them off the UI thread. A running [`scan`] uses its own connection.
pub struct Library {
    conn: Mutex<Connection>,
}

impl Library {
    /// Opens (creating if needed, along with its parent directory) the database at `db_path`.
    pub fn open(db_path: &Path) -> Result<Library> {
        Ok(Library {
            conn: Mutex::new(db::open(db_path)?),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every track, in no particular order.
    pub fn tracks(&self) -> Result<Vec<Track>> {
        db::load_tracks(&self.conn())
    }

    /// Tracked folders (canonical paths), sorted.
    pub fn folders(&self) -> Result<Vec<PathBuf>> {
        Ok(db::folders(&self.conn())?
            .into_iter()
            .map(|(_, path)| path)
            .collect())
    }

    /// Stop tracking a folder and drop its tracks. Files are never touched.
    ///
    /// Tracks that are also inside another tracked folder are kept and move to that folder.
    /// Unknown folders are ignored.
    pub fn remove_folder(&self, folder: &Path) -> Result<()> {
        db::remove_folder(&mut self.conn(), folder)
    }

    /// Small key/value store for UI state (queue, layout, columns).
    pub fn get_state(&self, key: &str) -> Result<Option<String>> {
        db::get_state(&self.conn(), key)
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        db::set_state(&self.conn(), key, value)
    }

    /// Stores gain measured by the loudness worker for a track without gain tags. Ignored if the
    /// track now has gain tags or no longer exists. A rescan keeps measured gain unless it finds
    /// gain tags, which replace it. Non-finite values are rejected.
    pub fn set_measured_gain(&self, id: TrackId, gain_db: f32, peak: f32) -> Result<()> {
        db::set_measured_gain(&self.conn(), id, gain_db, peak)
    }

    /// Tracks with neither gain tags nor measured gain, in id order.
    pub fn tracks_without_gain(&self) -> Result<Vec<(TrackId, PathBuf)>> {
        db::tracks_without_gain(&self.conn())
    }
}

#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// `scanned` of `total` audio files checked so far. While the folders are still being
    /// walked, `total` is a lower bound that keeps growing.
    Progress { scanned: usize, total: usize },
    /// New or changed tracks, in batches (about every 250 tracks or 100 ms). Ids are final.
    /// Tracks whose tracked folder changed are re-sent here too.
    Upserted(Vec<Track>),
    /// Tracks whose files are gone.
    Removed(Vec<TrackId>),
    /// The scan finished. Always the last event, unless the scan failed.
    Done {
        added: usize,
        updated: usize,
        removed: usize,
        elapsed: Duration,
    },
    /// A problem worth showing the user, e.g. an unavailable tracked folder. The scan carries
    /// on, unless the database itself failed: then this is the last event and no `Done` follows.
    Error(String),
}

/// Scan on a background thread. `add` becomes a tracked folder first; then every
/// tracked folder is walked and compared against the database (size + mtime).
///
/// Only new or changed files have their tags read. Missing files are removed, except under
/// folders that could not be read (e.g. an unmounted drive). Hidden (dot) files and folders
/// are skipped. Scans in one process run one at a time; a second call waits for the first.
pub fn scan(db_path: PathBuf, add: Option<PathBuf>) -> crossbeam_channel::Receiver<ScanEvent> {
    scan::spawn(db_path, add)
}

/// Album art for a track: embedded front cover first, then cover/folder/front
/// images beside the file. Scaled to fit within `max_px`.
///
/// Never upscales; `max_px == 0` returns the original size. Returns `None` when there is no
/// art or none of it decodes. Does blocking I/O and decoding: call it off the UI thread.
pub fn load_art(track_path: &Path, max_px: u32) -> Option<image::RgbaImage> {
    art::load(track_path, max_px)
}
