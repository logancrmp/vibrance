//! Background services wired into the UI: channels-as-subscriptions, scanning, folder
//! watching, loudness measurement, MPRIS, and tracked-folder management.

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use iced::futures::StreamExt;
use vibrance_desktop::{LoopStatus, Status, TrackInfo};

use super::*;
use crate::queue::Repeat;

/// A background channel delivered to `update` as messages, with no polling.
pub struct Channel<T> {
    id: u64,
    rx: async_channel::Receiver<T>,
    map: fn(T) -> Message,
}

impl<T> Clone for Channel<T> {
    fn clone(&self) -> Self {
        Channel { id: self.id, rx: self.rx.clone(), map: self.map }
    }
}

impl<T> Hash for Channel<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<T: Send + 'static> Channel<T> {
    pub fn new(rx: async_channel::Receiver<T>, map: fn(T) -> Message) -> Channel<T> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Channel { id: NEXT.fetch_add(1, Ordering::Relaxed), rx, map }
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::run_with(self.clone(), |c| c.rx.clone().map(c.map))
    }
}

/// An MPRIS server that finished registering on its start-up thread.
#[derive(Clone)]
pub struct MprisLink(Mpris, async_channel::Receiver<MprisCommand>);

impl std::fmt::Debug for MprisLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MprisLink")
    }
}

#[derive(Default)]
pub struct Channels {
    pub engine: Option<Channel<AudioEvent>>,
    pub scan: Option<Channel<ScanEvent>>,
    pub watch: Option<Channel<WatchEvent>>,
    pub gain: Option<Channel<(TrackId, f32, f32)>>,
    pub mpris: Option<Channel<MprisCommand>>,
    pub mpris_ready: Option<Channel<MprisLink>>,
    pub watcher_ready: Option<Channel<WatcherLink>>,
}

impl Channels {
    pub fn subscriptions(&self) -> Vec<Subscription<Message>> {
        let mut subs = Vec::new();
        if let Some(c) = &self.engine {
            subs.push(c.subscription());
        }
        if let Some(c) = &self.scan {
            subs.push(c.subscription());
        }
        if let Some(c) = &self.watch {
            subs.push(c.subscription());
        }
        if let Some(c) = &self.gain {
            subs.push(c.subscription());
        }
        if let Some(c) = &self.mpris {
            subs.push(c.subscription());
        }
        if let Some(c) = &self.mpris_ready {
            subs.push(c.subscription());
        }
        if let Some(c) = &self.watcher_ready {
            subs.push(c.subscription());
        }
        subs
    }
}

/// A folder watcher built off the UI thread (it walks every directory).
#[derive(Clone)]
pub struct WatcherLink(Arc<std::sync::Mutex<Option<Watcher>>>);

impl std::fmt::Debug for WatcherLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WatcherLink")
    }
}

/// Forward a blocking channel into an async one on a small thread.
pub fn bridge<T: Send + 'static>(rx: crossbeam_channel::Receiver<T>) -> async_channel::Receiver<T> {
    let (tx, out) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("vibrance-bridge".into())
        .spawn(move || {
            for item in rx {
                if tx.send_blocking(item).is_err() {
                    break;
                }
            }
        })
        .expect("spawn bridge thread");
    out
}

impl App {
    /// Work deferred until the window is up: launch rescan, watching, loudness.
    pub(super) fn after_first_frame(&mut self) {
        self.start_scan(None);
        self.start_watcher();
        self.start_gain_worker();
    }

    pub(super) fn start_scan(&mut self, add: Option<PathBuf>) {
        if self.scanning {
            self.rescan_pending |= add.is_none();
            if let Some(folder) = add {
                // Adding a folder can't wait for the running scan to notice it.
                self.notice = Some(format!("Will import {} after the current scan", folder.display()));
                self.pending_import = Some(folder);
            }
            return;
        }
        self.scanning = true;
        self.scan_label = Some("Scanning…".to_owned());
        let rx = vibrance_library::scan(self.paths.database(), add);
        self.channels.scan = Some(Channel::new(bridge(rx), Message::Scan));
    }

    pub(super) fn scan_event(&mut self, event: ScanEvent) -> Task<Message> {
        match event {
            ScanEvent::Progress { scanned, total } => {
                // Unchanged rescans finish in milliseconds; only show progress for real work.
                if total > 0 && scanned < total {
                    self.scan_label = Some(format!("Scanning {} / {}", thousands(scanned), thousands(total)));
                }
            }
            ScanEvent::Upserted(tracks) => {
                let mut all: std::collections::HashMap<TrackId, Track> =
                    std::mem::take(&mut self.model.tracks).into_iter().map(|t| (t.id, t)).collect();
                for t in tracks {
                    all.insert(t.id, t);
                }
                self.model = Model::build(all.into_values().collect());
                self.refresh_rows();
            }
            ScanEvent::Removed(ids) => {
                let gone: HashSet<TrackId> = ids.into_iter().collect();
                let tracks = std::mem::take(&mut self.model.tracks).into_iter().filter(|t| !gone.contains(&t.id)).collect();
                self.model = Model::build(tracks);
                self.refresh_rows();
            }
            ScanEvent::Done { added, updated, removed, elapsed } => {
                log::info!("scan done in {elapsed:?}: {added} added, {updated} updated, {removed} removed");
                self.scanning = false;
                self.scan_label = None;
                self.channels.scan = None;
                if let Some(folder) = self.pending_import.take() {
                    self.notice = None;
                    self.start_scan(Some(folder));
                } else if std::mem::take(&mut self.rescan_pending) {
                    self.start_scan(None);
                }
                self.refresh_watcher();
                self.start_gain_worker();
                if self.settings {
                    return self.load_folders();
                }
            }
            ScanEvent::Error(err) => {
                log::warn!("scan: {err}");
                self.notice = Some(err);
                if self.scanning && self.channels.scan.as_ref().is_some_and(|c| c.rx.is_closed()) {
                    self.scanning = false;
                    self.scan_label = None;
                }
            }
        }
        Task::none()
    }

    fn start_watcher(&mut self) {
        let db = self.paths.database();
        let slot = Arc::new(std::sync::Mutex::new(None));
        let (tx, rx) = async_channel::bounded(1);
        let link = WatcherLink(slot.clone());
        std::thread::Builder::new()
            .name("vibrance-watch-init".into())
            .spawn(move || match vibrance_library::watch(db) {
                Ok(watcher) => {
                    *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(watcher);
                    let _ = tx.send_blocking(link);
                }
                Err(err) => log::warn!("cannot watch library folders: {err}"),
            })
            .expect("spawn watcher thread");
        self.channels.watcher_ready = Some(Channel::new(rx, Message::WatcherReady));
    }

    pub(super) fn watcher_ready(&mut self, link: WatcherLink) {
        self.channels.watcher_ready = None;
        let watcher = link.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(watcher) = watcher {
            self.channels.watch = Some(Channel::new(bridge(watcher.events()), Message::Watch));
            self.watcher = Some(Arc::new(watcher));
        }
    }

    /// Re-read tracked folders after one was added or removed. It walks directories, so it
    /// runs on a short-lived thread.
    fn refresh_watcher(&self) {
        if let Some(watcher) = &self.watcher {
            let watcher = watcher.clone();
            std::thread::spawn(move || {
                if let Err(err) = watcher.refresh() {
                    log::warn!("cannot refresh folder watches: {err}");
                }
            });
        }
    }

    pub(super) fn watch_event(&mut self, event: WatchEvent) {
        match event {
            WatchEvent::Changed => self.start_scan(None),
            WatchEvent::LimitReached { .. } => {
                self.notice = Some("Too many folders to watch live; changes there appear after a rescan (F5).".to_owned());
            }
        }
    }

    /// Measure tracks without ReplayGain in the background while normalization is on.
    pub(super) fn start_gain_worker(&mut self) {
        if self.gain_worker.is_some() || !self.config.normalization {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let (tx, rx) = async_channel::unbounded();
        let db = self.paths.database();
        std::thread::Builder::new()
            .name("vibrance-loudness".into())
            .spawn(move || measure_all(&db, &flag, &tx))
            .expect("spawn loudness thread");
        self.channels.gain = Some(Channel::new(rx, |(id, gain, peak)| Message::Gain(id, gain, peak)));
        self.gain_worker = Some(cancel);
    }

    pub(super) fn start_mpris(&mut self, engine: Arc<Engine>) {
        let (tx, rx) = async_channel::bounded(1);
        std::thread::Builder::new()
            .name("vibrance-mpris-init".into())
            .spawn(move || match Mpris::start(engine) {
                Ok((mpris, commands)) => {
                    let _ = tx.send_blocking(MprisLink(mpris, commands));
                }
                Err(err) => log::warn!("MPRIS unavailable: {err}"),
            })
            .expect("spawn mpris thread");
        self.channels.mpris_ready = Some(Channel::new(rx, Message::MprisReady));
    }

    pub(super) fn mpris_ready(&mut self, link: MprisLink) {
        let MprisLink(mpris, commands) = link;
        self.channels.mpris_ready = None;
        self.channels.mpris = Some(Channel::new(commands, Message::Mpris));
        self.mpris = Some(mpris);
        self.mpris_state = None;
    }

    pub(super) fn mpris_command(&mut self, command: MprisCommand) -> Task<Message> {
        match command {
            MprisCommand::Play => {
                if self.play_state != PlayState::Playing {
                    self.play_pause();
                }
            }
            MprisCommand::Next => return self.dispatch(Message::Next),
            MprisCommand::Previous => return self.dispatch(Message::Prev),
            MprisCommand::Changed => self.poll_status(),
            MprisCommand::SetVolume(v) => self.set_volume(v as f32, false),
            MprisCommand::SetShuffle(on) => {
                if on != self.queue.shuffle {
                    self.queue.toggle_shuffle();
                    self.queue_changed();
                }
            }
            MprisCommand::SetLoop(status) => {
                self.queue.repeat = match status {
                    LoopStatus::None => Repeat::Off,
                    LoopStatus::Track => Repeat::Song,
                    LoopStatus::Playlist => Repeat::Queue,
                };
                self.queue_changed();
            }
            MprisCommand::Raise => return self.with_window(window::gain_focus),
            MprisCommand::Quit => return self.dispatch(Message::CloseWindow),
        }
        Task::none()
    }

    /// Push player state to MPRIS when anything it shows has changed.
    pub(super) fn publish_mpris(&mut self) {
        let Some(mpris) = &self.mpris else { return };
        let track = self.queue.now.and_then(|id| self.model.track(id)).map(|t| TrackInfo {
            id: t.id,
            title: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            album_artist: t.album_artist.clone(),
            track_no: t.track_no,
            duration: Some(Duration::from_millis(t.duration_ms)),
            path: t.path.clone(),
            art: self.art.as_ref().filter(|a| a.track == t.id).and_then(|a| a.file.clone()),
        });
        let state = PlayerState {
            status: match self.play_state {
                PlayState::Playing => Status::Playing,
                PlayState::Paused => Status::Paused,
                PlayState::Stopped => Status::Stopped,
            },
            can_previous: track.is_some(),
            track,
            volume: f64::from(self.volume),
            shuffle: self.queue.shuffle,
            loop_status: match self.queue.repeat {
                Repeat::Off => LoopStatus::None,
                Repeat::Song => LoopStatus::Track,
                Repeat::Queue => LoopStatus::Playlist,
            },
            can_next: self.queue.upcoming().is_some() || self.queue.cursor() < self.queue.items.len(),
        };
        if self.mpris_state.as_ref() != Some(&state) {
            mpris.update(state.clone());
            self.mpris_state = Some(state);
        }
    }

    pub(super) fn load_folders(&self) -> Task<Message> {
        let db = self.paths.database();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || Library::open(&db).and_then(|lib| lib.folders()).unwrap_or_default())
                    .await
                    .unwrap_or_default()
            },
            Message::Folders,
        )
    }

    pub(super) fn remove_folder(&mut self, folder: PathBuf) -> Task<Message> {
        self.folders.retain(|f| f != &folder);
        let db = self.paths.database();
        let task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let lib = Library::open(&db)?;
                    lib.remove_folder(&folder)?;
                    lib.tracks()
                })
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default()
            },
            Message::TracksReloaded,
        );
        self.refresh_watcher();
        task
    }
}

fn measure_all(db: &std::path::Path, cancel: &AtomicBool, tx: &async_channel::Sender<(TrackId, f32, f32)>) {
    let finish = || {
        let _ = tx.send_blocking((-1, 0.0, 0.0));
    };
    let lib = match Library::open(db) {
        Ok(lib) => lib,
        Err(err) => {
            log::warn!("loudness worker: {err}");
            return finish();
        }
    };
    let todo = lib.tracks_without_gain().unwrap_or_default();
    log::info!("measuring loudness of {} tracks", todo.len());
    for (id, path) in todo {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        match vibrance_audio::measure_loudness(&path) {
            Ok(l) => {
                // Silence gets 0 dB so it is never measured again.
                let (gain, peak) = if l.integrated_lufs.is_finite() { (-18.0 - l.integrated_lufs, l.true_peak) } else { (0.0, 0.0) };
                if let Err(err) = lib.set_measured_gain(id, gain, peak) {
                    log::debug!("cannot store loudness for {}: {err}", path.display());
                }
                if tx.send_blocking((id, gain, peak)).is_err() {
                    return;
                }
            }
            Err(err) => log::debug!("cannot measure {}: {err}", path.display()),
        }
    }
    finish();
}
