//! Live folder watching.
//!
//! notify's inotify thread classifies raw events and pings a debounce thread, which emits one
//! `WatchEvent::Changed` once things go quiet. The UI answers that with a rescan, which is cheap
//! enough (milliseconds when little changed) that per-path updates aren't worth it.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::{Result, db, tags};

/// Quiet time after the last relevant event before `Changed` is sent.
const QUIET: Duration = Duration::from_millis(1500);
/// Upper bound on the delay during a sustained burst (e.g. copying a big album), so the
/// library still fills in while the copy runs.
const MAX_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// Supported files or folders changed in a tracked folder: run `scan(db, None)`.
    Changed,
    /// The inotify watch limit (`fs.inotify.max_user_watches`) was hit, so some tracked folders
    /// are not (fully) watched. Counts are tracked folders. Sent at most once per `Watcher`;
    /// changes in unwatched folders are still picked up by rescans.
    LimitReached { watched: usize, failed: usize },
}

/// Watches every tracked folder until dropped. See [`watch`].
pub struct Watcher {
    db_path: PathBuf,
    notify: Mutex<RecommendedWatcher>,
    shared: Arc<Shared>,
    events: Receiver<WatchEvent>,
    stop: Option<Sender<()>>,
    debouncer: Option<JoinHandle<()>>,
}

/// State shared with notify's event thread and the debounce thread.
struct Shared {
    events: Sender<WatchEvent>,
    /// Watched tracked folders and whether each is fully watched.
    roots: Mutex<HashMap<PathBuf, bool>>,
    limit_reported: AtomicBool,
}

/// Starts watching every tracked folder in the database at `db_path`, recursively.
///
/// Setting up the watches walks every directory, so call this off the UI thread. Call
/// [`Watcher::refresh`] after adding or removing a tracked folder.
pub fn watch(db_path: PathBuf) -> Result<Watcher> {
    let (events_tx, events) = crossbeam_channel::unbounded();
    let shared = Arc::new(Shared {
        events: events_tx,
        roots: Mutex::new(HashMap::new()),
        limit_reported: AtomicBool::new(false),
    });
    // A single slot is enough: a ping that is already queued stands for any that follow.
    let (ping_tx, ping_rx) = crossbeam_channel::bounded(1);
    let handler_shared = Arc::clone(&shared);
    let notify = notify::recommended_watcher(move |result: notify::Result<Event>| {
        handle(&handler_shared, &ping_tx, result);
    })
    .map_err(io_error)?;

    let (stop, stop_rx) = crossbeam_channel::bounded(0);
    let debounce_shared = Arc::clone(&shared);
    let debouncer = thread::Builder::new()
        .name("vibrance-watch".into())
        .spawn(move || debounce(&ping_rx, &stop_rx, &debounce_shared.events))?;

    let watcher = Watcher {
        db_path,
        notify: Mutex::new(notify),
        shared,
        events,
        stop: Some(stop),
        debouncer: Some(debouncer),
    };
    watcher.refresh()?;
    Ok(watcher)
}

impl Watcher {
    /// The event stream. Clones share one stream: each event goes to one receiver.
    pub fn events(&self) -> Receiver<WatchEvent> {
        self.events.clone()
    }

    /// Re-reads the tracked folders and updates the watches. Call it after a folder was added
    /// (once its scan is done or underway) or removed. Folders that couldn't be watched before
    /// are retried.
    pub fn refresh(&self) -> Result<()> {
        let folders = db::folders(&db::open(&self.db_path)?)?;
        let wanted: Vec<PathBuf> = db::roots(&folders).into_iter().map(|(_, p)| p).collect();

        let mut notify = lock(&self.notify);
        // `roots` is never held across a notify call: notify's thread needs it to classify
        // events, and `watch` waits for that thread.
        let (stale, missing) = {
            let mut roots = lock(&self.shared.roots);
            let stale: Vec<PathBuf> = roots
                .keys()
                .filter(|p| !wanted.contains(p))
                .cloned()
                .collect();
            for path in &stale {
                roots.remove(path);
            }
            let missing: Vec<PathBuf> = wanted
                .into_iter()
                .filter(|p| roots.get(p) != Some(&true))
                .collect();
            for path in &missing {
                // Registered before watching so events from it are classified right away.
                roots.insert(path.clone(), false);
            }
            (stale, missing)
        };
        // Unwatch first: a new root may contain an old one, and unwatching that afterwards
        // would drop the watches the new root just added.
        for path in stale {
            if let Err(e) = notify.unwatch(&path) {
                log::debug!("unwatch {}: {e}", path.display());
            }
        }
        let mut limit_hit = false;
        for path in missing {
            match notify.watch(&path, RecursiveMode::Recursive) {
                Ok(()) => {
                    lock(&self.shared.roots).insert(path, true);
                }
                Err(e) if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) => {
                    log::warn!(
                        "inotify watch limit reached while watching {}",
                        path.display()
                    );
                    limit_hit = true;
                }
                // Typically an unavailable folder; the next refresh or rescan deals with it.
                Err(e) => {
                    log::warn!("cannot watch {}: {e}", path.display());
                    lock(&self.shared.roots).remove(&path);
                }
            }
        }
        if limit_hit {
            self.shared.report_limit();
        }
        Ok(())
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Disconnecting the stop channel ends the debounce thread; dropping `notify` afterwards
        // shuts down its inotify thread.
        drop(self.stop.take());
        if let Some(debouncer) = self.debouncer.take() {
            let _ = debouncer.join();
        }
    }
}

impl Shared {
    fn report_limit(&self) {
        if self.limit_reported.swap(true, Ordering::Relaxed) {
            return;
        }
        let roots = lock(&self.roots);
        let watched = roots.values().filter(|&&full| full).count();
        let _ = self.events.send(WatchEvent::LimitReached {
            watched,
            failed: roots.len() - watched,
        });
    }

    /// The tracked folder containing `path`, if any.
    fn root_of(&self, path: &Path) -> Option<PathBuf> {
        lock(&self.roots)
            .keys()
            .find(|r| path.starts_with(r))
            .cloned()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn io_error(e: notify::Error) -> std::io::Error {
    match e.kind {
        notify::ErrorKind::Io(io) => io,
        _ => std::io::Error::other(e.to_string()),
    }
}

/// Runs on notify's thread for every raw event.
fn handle(shared: &Shared, ping: &Sender<()>, result: notify::Result<Event>) {
    match result {
        Ok(event) => {
            if is_relevant(shared, &event) {
                let _ = ping.try_send(());
            }
        }
        // A directory created later couldn't be watched.
        Err(e) if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) => {
            for root in e.paths.iter().filter_map(|p| shared.root_of(p)) {
                lock(&shared.roots).insert(root, false);
            }
            shared.report_limit();
        }
        Err(e) => log::debug!("watch error: {e}"),
    }
}

/// Whether `event` could change the library: a create, modify, remove or rename of a
/// supported file or a directory, outside hidden folders (which scans skip too). Reads and
/// opens, including the scanner's own, are ignored.
fn is_relevant(shared: &Shared, event: &Event) -> bool {
    if event.need_rescan() {
        // The kernel queue overflowed and events were lost.
        return true;
    }
    let visible = |path: &&PathBuf| !is_hidden(shared, path);
    let mut paths = event.paths.iter().filter(visible);
    match event.kind {
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => {
            paths.next().is_some()
        }
        EventKind::Create(_)
        | EventKind::Remove(_)
        | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Metadata(_) | ModifyKind::Any) => {
            paths.any(|p| tags::is_supported(p))
        }
        // Renames don't say whether a directory moved. A path that is gone can't be checked,
        // so it counts unless it is clearly another kind of file; a spare rescan is cheap.
        EventKind::Modify(_) => paths.any(|p| {
            tags::is_supported(p)
                || p.is_dir()
                || (!p.exists() && p.extension().is_none_or(|e| !is_plain_extension(e)))
        }),
        EventKind::Access(_) | EventKind::Any | EventKind::Other => false,
    }
}

/// Whether `path` is inside a hidden (dot) directory below its tracked folder, or is hidden.
fn is_hidden(shared: &Shared, path: &Path) -> bool {
    let below = match shared.root_of(path) {
        Some(root) => path.strip_prefix(&root).unwrap_or(path).to_owned(),
        None => path.to_owned(),
    };
    below
        .components()
        .any(|c| c.as_os_str().as_bytes().first() == Some(&b'.'))
}

/// Looks like a real file extension ("txt", "part", "jpg") rather than part of a folder name
/// such as "Vol. 2".
fn is_plain_extension(ext: &std::ffi::OsStr) -> bool {
    let ext = ext.as_bytes();
    (1..=5).contains(&ext.len()) && ext.iter().all(u8::is_ascii_alphanumeric)
}

/// Emits `Changed` once pings stop for `QUIET` (or `MAX_DELAY` after the first ping).
fn debounce(pings: &Receiver<()>, stop: &Receiver<()>, events: &Sender<WatchEvent>) {
    // (first, last) ping of the pending burst.
    let mut pending: Option<(Instant, Instant)> = None;
    loop {
        let timer = match pending {
            Some((first, last)) => crossbeam_channel::at((last + QUIET).min(first + MAX_DELAY)),
            None => crossbeam_channel::never(),
        };
        crossbeam_channel::select! {
            recv(pings) -> ping => {
                if ping.is_err() {
                    return;
                }
                let now = Instant::now();
                pending = Some(pending.map_or((now, now), |(first, _)| (first, now)));
            }
            recv(timer) -> _ => {
                pending = None;
                let _ = events.send(WatchEvent::Changed);
            }
            recv(stop) -> _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, DataChange, Flag, MetadataKind, RenameMode};

    fn shared(roots: &[(&str, bool)]) -> (Shared, Receiver<WatchEvent>) {
        let (events, rx) = crossbeam_channel::unbounded();
        let roots = roots
            .iter()
            .map(|&(p, full)| (PathBuf::from(p), full))
            .collect();
        let shared = Shared {
            events,
            roots: Mutex::new(roots),
            limit_reported: AtomicBool::new(false),
        };
        (shared, rx)
    }

    fn event(kind: EventKind, paths: &[&str]) -> Event {
        paths
            .iter()
            .fold(Event::new(kind), |e, p| e.add_path(p.into()))
    }

    #[test]
    fn classifies_events() {
        let (s, _) = shared(&[("/m/.music", true)]);
        let relevant = |kind, paths: &[&str]| is_relevant(&s, &event(kind, paths));
        let data = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        let rename = EventKind::Modify(ModifyKind::Name(RenameMode::From));

        assert!(relevant(
            EventKind::Create(CreateKind::File),
            &["/m/.music/a.flac"]
        ));
        assert!(relevant(data, &["/m/.music/a/B.MP3"]));
        assert!(relevant(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
            &["/m/.music/a.ogg"]
        ));
        assert!(relevant(
            EventKind::Remove(RemoveKind::File),
            &["/m/.music/a.opus"]
        ));
        assert!(relevant(
            EventKind::Create(CreateKind::Folder),
            &["/m/.music/Album"]
        ));
        assert!(relevant(
            EventKind::Remove(RemoveKind::Folder),
            &["/m/.music/Album"]
        ));
        // Gone, and might have been a folder.
        assert!(relevant(rename, &["/m/.music/Vol. 2"]));
        assert!(relevant(rename, &["/m/.music/Album"]));
        assert!(relevant(rename, &["/m/.music/x.part", "/m/.music/x.mp3"]));
        assert!(!relevant(EventKind::Other, &[]));
        assert!(is_relevant(
            &s,
            &Event::new(EventKind::Other).set_flag(Flag::Rescan)
        ));

        // Reads, other file types, and hidden folders below the root.
        let read = EventKind::Access(AccessKind::Open(notify::event::AccessMode::Read));
        assert!(!relevant(read, &["/m/.music/a.mp3"]));
        assert!(!relevant(
            EventKind::Create(CreateKind::File),
            &["/m/.music/cover.jpg"]
        ));
        assert!(!relevant(data, &["/m/.music/notes.txt"]));
        assert!(!relevant(rename, &["/m/.music/notes.txt"]));
        assert!(!relevant(
            EventKind::Create(CreateKind::File),
            &["/m/.music/.trash/a.mp3"]
        ));
        assert!(!relevant(
            EventKind::Create(CreateKind::Folder),
            &["/m/.music/.git"]
        ));
    }

    #[test]
    fn limit_is_reported_once() {
        let (s, rx) = shared(&[("/a", true), ("/b", false), ("/c", false)]);
        s.report_limit();
        s.report_limit();
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [WatchEvent::LimitReached {
                watched: 1,
                failed: 2
            }]
        );
    }
}
