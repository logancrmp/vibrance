//! Background scanning: walk the tracked folders, compare each file's size and mtime against the
//! database, read tags for new or changed files on a worker pool, and write the results in
//! batches while streaming events to the UI.
//!
//! Threads: one walker, N tag readers, and the scan thread itself, which owns the database
//! connection and is the only writer. The walk is pipelined with tag reading so the first
//! tracks reach the UI before the walk has finished.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use rusqlite::{Connection, ErrorCode, TransactionBehavior};
use walkdir::WalkDir;

use crate::db::{self, FolderId, Known};
use crate::{Result, ScanEvent, Track, TrackId, tags};

const BATCH_SIZE: usize = 250;
const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
/// The first batch goes out sooner, so an import shows tracks well within 100 ms.
const FIRST_FLUSH: Duration = Duration::from_millis(20);

/// One scan at a time per process; a second request waits, then rescans.
static SCAN_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn spawn(db_path: PathBuf, add: Option<PathBuf>) -> Receiver<ScanEvent> {
    let (events, rx) = crossbeam_channel::unbounded();
    let thread_events = events.clone();
    let spawned = thread::Builder::new()
        .name("vibrance-scan".into())
        .spawn(move || {
            lower_priority();
            let _guard = SCAN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            if let Err(e) = run(&db_path, add, &thread_events) {
                log::error!("library scan failed: {e}");
                send(&thread_events, ScanEvent::Error(e.to_string()));
            }
        });
    if let Err(e) = spawned {
        send(
            &events,
            ScanEvent::Error(format!("could not start scan: {e}")),
        );
    }
    rx
}

/// The UI may drop the receiver; the scan still brings the database up to date.
fn send(events: &Sender<ScanEvent>, event: ScanEvent) {
    let _ = events.send(event);
}

/// Scan threads yield to the UI and audio threads (spec 7.1: background work must not affect
/// frame times or cause underruns). On Linux the nice value is per thread and inherited by
/// threads spawned afterwards, so this covers the walker and readers too.
#[cfg(target_os = "linux")]
fn lower_priority() {
    unsafe extern "C" {
        safe fn nice(inc: std::ffi::c_int) -> std::ffi::c_int;
    }
    nice(10);
}

#[cfg(not(target_os = "linux"))]
fn lower_priority() {}

#[derive(Default)]
struct Stats {
    added: usize,
    updated: usize,
    removed: usize,
}

#[derive(Default)]
struct Counters {
    /// Audio files found so far (after de-duplication).
    found: AtomicUsize,
    /// Found files whose size and mtime match the database.
    unchanged: AtomicUsize,
}

struct Root {
    id: FolderId,
    path: PathBuf,
}

/// A new or changed file whose tags need reading.
struct Job {
    path: PathBuf,
    root: usize,
    mtime: i64,
    size: u64,
    existing: bool,
}

struct Parsed {
    track: Track,
    folder_id: FolderId,
    existing: bool,
}

fn run(db_path: &Path, add: Option<PathBuf>, events: &Sender<ScanEvent>) -> Result<()> {
    let start = Instant::now();
    let mut conn = db::open(db_path)?;
    let mut stats = Stats::default();
    send(
        events,
        ScanEvent::Progress {
            scanned: 0,
            total: 0,
        },
    );

    if let Some(dir) = add {
        add_folder(&mut conn, &dir, events, &mut stats)?;
    }
    let roots: Vec<Root> = db::roots(&db::folders(&conn)?)
        .into_iter()
        .map(|(id, path)| Root { id, path })
        .collect();
    let known = db::known_files(&conn)?;
    let counters = Counters::default();
    let workers = thread::available_parallelism().map_or(4, |n| n.get());

    let (job_tx, job_rx) = crossbeam_channel::unbounded::<Job>();
    // Bounded so readers can't run far ahead of the writer and pile up parsed tracks.
    let (parsed_tx, parsed_rx) = crossbeam_channel::bounded::<Parsed>(4 * BATCH_SIZE);
    let (walked, written) = thread::scope(|s| {
        let (roots, counters) = (&roots, &counters);
        let walker = s.spawn(move || walk(roots, known, &job_tx, counters, events));
        let readers: Vec<_> = (0..workers)
            .map(|_| {
                let (jobs, parsed) = (job_rx.clone(), parsed_tx.clone());
                s.spawn(move || {
                    for job in jobs {
                        if parsed.send(read_job(job, roots)).is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();
        drop((job_rx, parsed_tx));
        let written = write_batches(&mut conn, &parsed_rx, events, counters, &mut stats);
        // If writing failed this stops the readers, which in turn stops the walker.
        drop(parsed_rx);
        let readers_ok = readers.into_iter().all(|r| r.join().is_ok());
        (walker.join().ok().filter(|_| readers_ok), written)
    });
    written?;
    let walked = walked.ok_or_else(|| std::io::Error::other("scan worker panicked"))?;

    if walked.complete {
        let gone = walked.gone();
        if !gone.is_empty() {
            db::delete_tracks(&mut conn, &gone)?;
            stats.removed = gone.len();
            send(events, ScanEvent::Removed(gone));
        }
    }
    let total = counters.found.load(Relaxed);
    send(
        events,
        ScanEvent::Progress {
            scanned: total,
            total,
        },
    );
    send(
        events,
        ScanEvent::Done {
            added: stats.added,
            updated: stats.updated,
            removed: stats.removed,
            elapsed: start.elapsed(),
        },
    );
    Ok(())
}

/// Tracks `dir` (canonicalized). If it contains already-tracked folders, their tracks move to it.
fn add_folder(
    conn: &mut Connection,
    dir: &Path,
    events: &Sender<ScanEvent>,
    stats: &mut Stats,
) -> Result<()> {
    let dir = match std::fs::canonicalize(dir) {
        Ok(dir) if dir.is_dir() => dir,
        Ok(_) => {
            send(
                events,
                ScanEvent::Error(format!("{} is not a folder", dir.display())),
            );
            return Ok(());
        }
        Err(e) => {
            send(
                events,
                ScanEvent::Error(format!("cannot add {}: {e}", dir.display())),
            );
            return Ok(());
        }
    };
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (id, inserted) = db::insert_folder(&tx, &dir)?;
    let is_root = || -> Result<bool> {
        Ok(db::roots(&db::folders(&tx)?)
            .iter()
            .any(|(root, _)| *root == id))
    };
    let adopted = if inserted && is_root()? {
        db::adopt_subtree(&tx, id, &dir)?
    } else {
        Vec::new()
    };
    tx.commit()?;
    if !adopted.is_empty() {
        stats.updated += adopted.len();
        send(events, ScanEvent::Upserted(adopted));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Walking

/// A supported audio file found on disk.
struct Candidate {
    path: PathBuf,
    root: usize,
    mtime: i64,
    size: u64,
    inode: (u64, u64),
}

struct Walk<'a> {
    roots: &'a [Root],
    jobs: &'a Sender<Job>,
    counters: &'a Counters,
    /// Stored tracks not yet seen on disk. Whatever is left at the end is gone.
    unseen: HashMap<Vec<u8>, Known>,
    /// (device, inode) of every accepted file, so a file reachable by two paths counts once.
    inodes: HashSet<(u64, u64)>,
    /// Paths that couldn't be read. Tracks under them are kept, not removed, because an
    /// unmounted drive or a permissions problem looks the same as deleted files.
    protected: Vec<PathBuf>,
}

struct WalkOutcome {
    complete: bool,
    unseen: HashMap<Vec<u8>, Known>,
    protected: Vec<PathBuf>,
}

impl WalkOutcome {
    /// Tracks whose files are gone.
    fn gone(self) -> Vec<TrackId> {
        let WalkOutcome {
            unseen, protected, ..
        } = self;
        unseen
            .into_iter()
            .filter(|(path, _)| {
                let path = Path::new(OsStr::from_bytes(path));
                !protected.iter().any(|p| path.starts_with(p))
            })
            .map(|(_, known)| known.id)
            .collect()
    }
}

fn walk(
    roots: &[Root],
    known: HashMap<Vec<u8>, Known>,
    jobs: &Sender<Job>,
    counters: &Counters,
    events: &Sender<ScanEvent>,
) -> WalkOutcome {
    let mut walk = Walk {
        roots,
        jobs,
        counters,
        unseen: known,
        inodes: HashSet::new(),
        protected: Vec::new(),
    };
    let complete = walk.run(events);
    WalkOutcome {
        complete,
        unseen: walk.unseen,
        protected: walk.protected,
    }
}

impl Walk<'_> {
    /// Returns false if the readers went away before the walk finished.
    fn run(&mut self, events: &Sender<ScanEvent>) -> bool {
        // Files reached through a symlink are handled last, so that when the same file is also
        // reachable directly, the direct path is the one stored.
        let mut via_link = Vec::new();
        for (index, root) in self.roots.iter().enumerate() {
            if let Err(e) = std::fs::metadata(&root.path) {
                let msg = format!("tracked folder {} is unavailable: {e}", root.path.display());
                log::warn!("{msg}");
                send(events, ScanEvent::Error(msg));
                self.protected.push(root.path.clone());
                continue;
            }
            // Depths of the symlinked directories enclosing the current entry.
            let mut link_depths: Vec<usize> = Vec::new();
            let entries = WalkDir::new(&root.path)
                .follow_links(true)
                .into_iter()
                .filter_entry(|e| e.depth() == 0 || !is_hidden(e.file_name()));
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        self.note_error(e);
                        continue;
                    }
                };
                let depth = entry.depth();
                while link_depths.last().is_some_and(|&d| d >= depth) {
                    link_depths.pop();
                }
                let linked = entry.path_is_symlink() || !link_depths.is_empty();
                let file_type = entry.file_type();
                if file_type.is_dir() {
                    if entry.path_is_symlink() {
                        link_depths.push(depth);
                    }
                    continue;
                }
                if !file_type.is_file() || !tags::is_supported(entry.path()) {
                    continue;
                }
                let md = match entry.metadata() {
                    Ok(md) => md,
                    Err(e) => {
                        self.note_error(e);
                        continue;
                    }
                };
                let file = Candidate {
                    path: entry.into_path(),
                    root: index,
                    mtime: md
                        .mtime()
                        .saturating_mul(1_000_000_000)
                        .saturating_add(md.mtime_nsec()),
                    size: md.size(),
                    inode: (md.dev(), md.ino()),
                };
                if linked {
                    via_link.push(file);
                } else if !self.dispatch(file) {
                    return false;
                }
            }
        }
        via_link.into_iter().all(|file| self.dispatch(file))
    }

    /// Queues a file for tag reading unless it is unchanged or a duplicate.
    /// Returns false if the readers went away.
    fn dispatch(&mut self, file: Candidate) -> bool {
        if !self.inodes.insert(file.inode) {
            return true;
        }
        self.counters.found.fetch_add(1, Relaxed);
        let known = self.unseen.remove(db::path_bytes(&file.path));
        let folder_id = self.roots[file.root].id;
        if let Some(k) = &known
            && (k.mtime, k.size, k.folder_id) == (file.mtime, file.size, folder_id)
        {
            self.counters.unchanged.fetch_add(1, Relaxed);
            return true;
        }
        let job = Job {
            path: file.path,
            root: file.root,
            mtime: file.mtime,
            size: file.size,
            existing: known.is_some(),
        };
        self.jobs.send(job).is_ok()
    }

    fn note_error(&mut self, e: walkdir::Error) {
        if e.loop_ancestor().is_some() {
            log::debug!("skipping symlink loop: {e}");
            return;
        }
        log::warn!("scan: {e}");
        let Some(path) = e.path() else { return };
        // A track whose file (or symlink target) vanished is simply gone. Anything else may be
        // a directory we couldn't read, so keep what we have under it.
        let vanished_track = e
            .io_error()
            .is_some_and(|io| io.kind() == ErrorKind::NotFound)
            && self.unseen.contains_key(db::path_bytes(path));
        if !vanished_track {
            self.protected.push(path.to_owned());
        }
    }
}

fn is_hidden(name: &OsStr) -> bool {
    name.as_bytes().first() == Some(&b'.')
}

// ---------------------------------------------------------------------------------------------
// Reading and writing

fn read_job(job: Job, roots: &[Root]) -> Parsed {
    let info = tags::read(&job.path);
    let root = &roots[job.root];
    Parsed {
        folder_id: root.id,
        existing: job.existing,
        track: Track {
            id: 0,
            path: job.path,
            folder: root.path.clone(),
            title: info.title,
            artist: info.artist,
            album_artist: info.album_artist,
            album: info.album,
            track_no: info.track_no,
            disc_no: info.disc_no,
            year: info.year,
            genre: info.genre,
            duration_ms: info.duration_ms,
            format: info.format,
            bitrate_kbps: info.bitrate_kbps,
            sample_rate: info.sample_rate,
            mtime: job.mtime,
            size: job.size,
            // Filled in from the database when the track is written.
            added: 0,
            gain_db: info.gain_db,
            peak: info.peak,
        },
    }
}

/// Collects parsed tracks and writes them every `BATCH_SIZE` tracks or `FLUSH_INTERVAL`,
/// whichever comes first, until all readers are done.
fn write_batches(
    conn: &mut Connection,
    parsed: &Receiver<Parsed>,
    events: &Sender<ScanEvent>,
    counters: &Counters,
    stats: &mut Stats,
) -> Result<()> {
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut processed = 0;
    let mut last_progress = None;
    let mut deadline = Instant::now() + FIRST_FLUSH;
    loop {
        let finished = match parsed.recv_deadline(deadline) {
            Ok(item) => {
                batch.push(item);
                if batch.len() < BATCH_SIZE && Instant::now() < deadline {
                    continue;
                }
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        processed += batch.len();
        write_batch(conn, &mut batch, events, stats)?;
        // While the walk is still running, `total` is a lower bound that keeps growing.
        let progress = (
            counters.unchanged.load(Relaxed) + processed,
            counters.found.load(Relaxed),
        );
        if last_progress != Some(progress) && !finished {
            send(
                events,
                ScanEvent::Progress {
                    scanned: progress.0,
                    total: progress.1,
                },
            );
            last_progress = Some(progress);
        }
        if finished {
            return Ok(());
        }
        // Keep the short deadline until the first tracks are out.
        let interval = if processed == 0 {
            FIRST_FLUSH
        } else {
            FLUSH_INTERVAL
        };
        deadline = Instant::now() + interval;
    }
}

fn write_batch(
    conn: &mut Connection,
    batch: &mut Vec<Parsed>,
    events: &Sender<ScanEvent>,
    stats: &mut Stats,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let now = db::unix_now();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut upserted = Vec::with_capacity(batch.len());
    {
        let mut stmt = db::upsert_statement(&tx)?;
        for Parsed {
            mut track,
            folder_id,
            existing,
        } in batch.drain(..)
        {
            match db::upsert_track(&mut stmt, &mut track, folder_id, now) {
                Ok(()) => {
                    if existing {
                        stats.updated += 1;
                    } else {
                        stats.added += 1;
                    }
                    upserted.push(track);
                }
                // Its folder was removed while the scan was running.
                Err(e) if e.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) => {
                    log::debug!("dropping {}: {e}", track.path.display());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    tx.commit()?;
    if !upserted.is_empty() {
        send(events, ScanEvent::Upserted(upserted));
    }
    Ok(())
}
