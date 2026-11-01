//! Application state, messages, and the update loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use iced::keyboard::{self, Key, Modifiers, key::Named};
use iced::widget::{image, operation, pane_grid};
use iced::{Subscription, Task, window};
use vibrance_audio::{Analyzer, Engine, Event as AudioEvent, PlayState, Sink, Tap};
use vibrance_library::{Library, ScanEvent, Track, TrackId};

use crate::config::{Config, Paths, TitleBar};
use crate::model::{self, AlbumCol, AlbumKey, ArtistCol, Model, SongCol};
use crate::persist::Persist;
use crate::queue::{Queue, Step};
use crate::theme::{ColorMode, Colors, PaletteChoice};
use crate::ui::{spectrum, table};

pub const SPECTRUM_BARS: usize = 28;
const PALETTE_FADE: Duration = Duration::from_millis(300);
const GLIDE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pane {
    Artists,
    Albums,
    Songs,
    Queue,
}

impl Pane {
    pub const ALL: [Pane; 4] = [Pane::Artists, Pane::Albums, Pane::Songs, Pane::Queue];

    fn ix(self) -> usize {
        self as usize
    }

    /// Artists and Albums have a pinned "All (N)" first row.
    pub fn has_all_row(self) -> bool {
        matches!(self, Pane::Artists | Pane::Albums)
    }
}

/// What a pane-grid slot shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    List(Pane),
    AlbumArt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Menu {
    Closed,
    Main,
    Palette,
}

#[derive(Debug, Clone)]
pub struct Art {
    pub track: TrackId,
    pub large: image::Handle,
    pub small: image::Handle,
    pub seed: material_colors::color::Rgb,
}

#[derive(Debug, Clone)]
pub enum Message {
    Frame(Instant),
    Key(keyboard::Event),
    WindowId(Option<window::Id>),
    WindowResized,
    Maximized(bool),
    DragWindow,
    ToggleMaximize,
    Minimize,
    CloseWindow,
    ResizeWindow(window::Direction),
    SystemTheme(iced::theme::Mode),

    Focus(Pane),
    RowPress(Pane, usize),
    RowActivate(Pane, usize),
    RowHover(Pane, usize, bool),
    Scrolled(Pane, f32, f32),
    SortBy(Pane, usize),
    PaneResized(pane_grid::ResizeEvent),
    PaneDragged(pane_grid::DragEvent),

    PlayPause,
    Stop,
    Next,
    Prev,
    SeekHover(bool),
    SeekPreview(f32),
    SeekCommit { fraction: f32, glide: bool },
    Volume(f32),
    VolumeStep(f32),
    ToggleMute,
    ToggleShuffle,
    CycleRepeat,
    ClearQueue,

    ToggleMenu,
    OpenPaletteMenu,
    CloseMenu,
    PickPalette(PaletteChoice),
    ToggleColorMode,
    ImportFolder,
    FolderPicked(Option<PathBuf>),
    Rescan,
    ScanPoll,
    ArtLoaded(TrackId, Option<Art>),
    Shot(window::Screenshot),
}

#[derive(Debug, Clone)]
pub struct Options {
    pub sink: Sink,
    pub screenshot: Option<PathBuf>,
    pub screenshot_after: Duration,
    pub script: Vec<String>,
}

struct Jump {
    pane: Pane,
    term: String,
    at: Instant,
}

struct Scan {
    rx: crossbeam_channel::Receiver<ScanEvent>,
    scanned: usize,
    total: usize,
}

struct Glide {
    from: f32,
    to: f32,
    start: Instant,
}

pub struct App {
    pub paths: Paths,
    pub config: Config,
    persist: Persist,

    pub model: Model,
    pub artist_rows: Vec<usize>,
    pub album_rows: Vec<usize>,
    pub song_rows: Vec<usize>,
    pub sel_artists: Vec<String>,
    pub sel_albums: Vec<AlbumKey>,
    pub sel_songs: Vec<TrackId>,
    pub sel_queue: Vec<usize>,
    anchors: [Option<usize>; 4],
    pub sort_artists: Option<(ArtistCol, bool)>,
    pub sort_albums: Option<(AlbumCol, bool)>,
    pub sort_songs: Option<(SongCol, bool)>,
    pub offsets: [f32; 4],
    viewports: [f32; 4],
    pub hovered: [Option<usize>; 4],
    pub focus: Pane,
    jump: Option<Jump>,
    modifiers: Modifiers,
    pub panes: pane_grid::State<Slot>,

    engine: Option<Engine>,
    engine_init: Option<std::thread::JoinHandle<Result<Engine, vibrance_audio::Error>>>,
    events: Option<crossbeam_channel::Receiver<AudioEvent>>,
    tap: Option<Tap>,
    analyzer: Option<Analyzer>,
    pub queue: Queue,
    pub play_state: PlayState,
    pub position: Duration,
    pub duration: Option<Duration>,
    resume_at: Option<Duration>,
    pub volume: f32,
    pub muted: bool,
    pub bars: Vec<f32>,
    pub peaks: Vec<f32>,
    pub seek_hover: bool,
    pub seek_preview: Option<f32>,
    pub seek_lift: f32,
    pub seek_drag: f32,
    glide: Option<Glide>,
    pub art: Option<Art>,
    art_loading: Option<TrackId>,

    pub colors: Colors,
    fade_from: Colors,
    fade_to: Colors,
    fade_start: Option<Instant>,
    system_dark: bool,

    pub window: Option<window::Id>,
    pub maximized: bool,
    pub menu: Menu,
    scan: Option<Scan>,
    pub scan_label: Option<String>,
    last_frame: Instant,
    last_position_save: Instant,
    screenshot: Option<(PathBuf, Instant)>,
    first_frame_seen: bool,
}

impl App {
    pub fn new(options: Options) -> (App, Task<Message>) {
        let t0 = Instant::now();
        let paths = Paths::resolve();
        paths.create();
        let config = Config::load(&paths);

        let library = Library::open(&paths.database());
        let (tracks, queue_state, volume_state, position_state) = match &library {
            Ok(lib) => (
                lib.tracks().unwrap_or_else(|err| {
                    log::error!("cannot load library: {err}");
                    Vec::new()
                }),
                lib.get_state("queue").ok().flatten(),
                lib.get_state("volume").ok().flatten(),
                lib.get_state("position_ms").ok().flatten(),
            ),
            Err(err) => {
                log::error!("cannot open library database: {err}");
                (Vec::new(), None, None, None)
            }
        };
        drop(library);
        let t_library = t0.elapsed();

        let (volume, muted) = volume_state
            .and_then(|s| {
                let (v, m) = s.split_once(';')?;
                Some((v.parse().ok()?, m == "1"))
            })
            .unwrap_or((0.72, false));

        // Opening the output device takes ~20 ms, so it happens off the startup path.
        let engine_init = std::env::var_os("VIBRANCE_NO_AUDIO").is_none().then(|| {
            let sink = options.sink.clone();
            std::thread::Builder::new()
                .name("vibrance-audio-init".into())
                .spawn(move || Engine::new(sink))
                .expect("spawn audio init thread")
        });

        let model = Model::build(tracks);
        let mut queue = queue_state.as_deref().and_then(Queue::from_state).unwrap_or_default();
        queue.items.retain(|id| model.index.contains_key(id));
        if queue.now.is_some_and(|id| !model.index.contains_key(&id)) {
            queue.now = None;
        }

        let colors = target_colors(&config, None, true);
        let now = Instant::now();
        let mut app = App {
            persist: Persist::spawn(paths.database()),
            paths,
            config,
            model,
            artist_rows: Vec::new(),
            album_rows: Vec::new(),
            song_rows: Vec::new(),
            sel_artists: Vec::new(),
            sel_albums: Vec::new(),
            sel_songs: Vec::new(),
            sel_queue: Vec::new(),
            anchors: [None; 4],
            sort_artists: None,
            sort_albums: None,
            sort_songs: None,
            offsets: [0.0; 4],
            viewports: [200.0; 4],
            hovered: [None; 4],
            focus: Pane::Artists,
            jump: None,
            modifiers: Modifiers::default(),
            panes: default_layout(),
            engine: None,
            engine_init,
            events: None,
            tap: None,
            analyzer: None,
            queue,
            play_state: PlayState::Stopped,
            position: position_state.and_then(|s| s.parse().ok()).map(Duration::from_millis).unwrap_or_default(),
            duration: None,
            resume_at: None,
            volume,
            muted,
            bars: vec![0.0; SPECTRUM_BARS],
            peaks: vec![0.0; SPECTRUM_BARS],
            seek_hover: false,
            seek_preview: None,
            seek_lift: 0.0,
            seek_drag: 0.0,
            glide: None,
            art: None,
            art_loading: None,
            colors,
            fade_from: colors,
            fade_to: colors,
            fade_start: None,
            system_dark: true,
            window: None,
            maximized: false,
            menu: Menu::Closed,
            scan: None,
            scan_label: None,
            last_frame: now,
            last_position_save: now,
            screenshot: options.screenshot.map(|p| (p, now + options.screenshot_after)),
            first_frame_seen: false,
        };
        app.resume_at = Some(app.position);
        app.duration = app.queue.now.and_then(|id| app.model.track(id)).map(|t| Duration::from_millis(t.duration_ms));
        app.refresh_rows();
        log::debug!(
            "boot: library {t_library:?}, model + rows {:?}, total {:?} ({} tracks)",
            t0.elapsed() - t_library,
            t0.elapsed(),
            app.model.tracks.len()
        );

        let mut tasks = vec![window::latest().map(Message::WindowId), app.load_art()];
        for step in &options.script {
            tasks.push(app.run_script(step));
        }
        (app, Task::batch(tasks))
    }

    pub fn title(&self) -> String {
        match self.queue.now.and_then(|id| self.model.track(id)) {
            Some(t) if self.play_state == PlayState::Playing => format!("{} · {} — Vibrance", t.title, t.artist),
            _ => "Vibrance".to_owned(),
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        self.adopt_engine(!matches!(message, Message::Frame(_)));
        match message {
            Message::Frame(now) => return self.frame(now),
            Message::Key(event) => return self.key(event),
            Message::WindowId(id) => self.window = id,
            Message::WindowResized => {
                if let Some(id) = self.window {
                    return window::is_maximized(id).map(Message::Maximized);
                }
            }
            Message::Maximized(m) => self.maximized = m,
            Message::DragWindow => return self.with_window(window::drag),
            Message::ToggleMaximize => return self.with_window(window::toggle_maximize),
            Message::Minimize => return self.with_window(|id| window::minimize(id, true)),
            Message::ResizeWindow(direction) => return self.with_window(move |id| window::drag_resize(id, direction)),
            Message::CloseWindow => {
                self.save_session();
                self.persist.flush();
                return iced::exit();
            }
            Message::SystemTheme(mode) => {
                self.system_dark = !matches!(mode, iced::theme::Mode::Light);
                self.retheme();
            }

            Message::Focus(pane) => {
                if self.focus != pane {
                    self.jump = None;
                }
                self.focus = pane;
            }
            Message::RowPress(pane, row) => {
                self.focus = pane;
                self.menu = Menu::Closed;
                self.select(pane, row, self.modifiers);
            }
            Message::RowActivate(pane, row) => self.activate(pane, row),
            Message::RowHover(pane, row, entered) => {
                let slot = &mut self.hovered[pane.ix()];
                if entered {
                    *slot = Some(row);
                } else if *slot == Some(row) {
                    *slot = None;
                }
            }
            Message::Scrolled(pane, y, height) => {
                self.offsets[pane.ix()] = y;
                self.viewports[pane.ix()] = height;
            }
            Message::SortBy(pane, col) => {
                self.sort_by(pane, col);
                self.refresh_rows();
            }
            Message::PaneResized(pane_grid::ResizeEvent { split, ratio }) => self.panes.resize(split, ratio),
            Message::PaneDragged(pane_grid::DragEvent::Dropped { pane, target }) => self.panes.drop(pane, target),
            Message::PaneDragged(_) => {}

            Message::PlayPause => self.play_pause(),
            Message::Stop => {
                if let Some(engine) = &self.engine {
                    engine.stop();
                }
                self.play_state = PlayState::Stopped;
                self.position = Duration::ZERO;
            }
            Message::Next => {
                let step = self.queue.next();
                self.apply(step);
            }
            Message::Prev => {
                let step = if self.position > Duration::from_secs(3) { Step::Restart } else { self.queue.prev() };
                self.apply(step);
            }
            Message::SeekHover(h) => self.seek_hover = h,
            Message::SeekPreview(f) => self.seek_preview = Some(f),
            Message::SeekCommit { fraction, glide } => {
                let from = self.displayed_fraction();
                self.seek_preview = None;
                if let (Some(engine), Some(duration)) = (&self.engine, self.duration) {
                    let target = duration.mul_f32(fraction);
                    if self.play_state == PlayState::Stopped {
                        self.resume_at = Some(target);
                    } else {
                        engine.seek(target);
                    }
                    self.position = target;
                    if glide {
                        self.glide = Some(Glide { from, to: fraction, start: Instant::now() });
                    }
                }
            }
            Message::Volume(v) => self.set_volume(v, false),
            Message::VolumeStep(d) => self.set_volume(self.volume + d, false),
            Message::ToggleMute => self.set_volume(self.volume, !self.muted),
            Message::ToggleShuffle => {
                self.queue.toggle_shuffle();
                self.queue_changed();
            }
            Message::CycleRepeat => {
                self.queue.cycle_repeat();
                self.queue_changed();
            }
            Message::ClearQueue => {
                self.queue.clear();
                self.sel_queue.clear();
                self.queue_changed();
            }

            Message::ToggleMenu => self.menu = if self.menu == Menu::Closed { Menu::Main } else { Menu::Closed },
            Message::OpenPaletteMenu => self.menu = if self.menu == Menu::Palette { Menu::Main } else { Menu::Palette },
            Message::CloseMenu => self.menu = Menu::Closed,
            Message::PickPalette(choice) => {
                self.config.palette = choice;
                self.save_config();
                self.retheme();
            }
            Message::ToggleColorMode => {
                self.config.color_mode = match self.config.color_mode {
                    ColorMode::Light => ColorMode::Dark,
                    _ => ColorMode::Light,
                };
                self.save_config();
                self.retheme();
            }
            Message::ImportFolder => {
                self.menu = Menu::Closed;
                return Task::perform(pick_folder(), Message::FolderPicked);
            }
            Message::FolderPicked(Some(folder)) => self.start_scan(Some(folder)),
            Message::FolderPicked(None) => {}
            Message::Rescan => {
                self.menu = Menu::Closed;
                self.start_scan(None);
            }
            Message::ScanPoll => self.poll_scan(),
            Message::ArtLoaded(track, art) => {
                if self.art_loading == Some(track) {
                    self.art_loading = None;
                    self.art = art;
                    if self.config.palette == PaletteChoice::AlbumArt {
                        self.retheme();
                    }
                }
            }
            Message::Shot(shot) => {
                if let Some((path, _)) = self.screenshot.take() {
                    save_screenshot(&path, &shot);
                }
                return self.update(Message::CloseWindow);
            }
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            keyboard::listen().map(Message::Key),
            window::resize_events().map(|_| Message::WindowResized),
            window::close_requests().map(|_| Message::CloseWindow),
        ];
        if self.animating() {
            subs.push(window::frames().map(Message::Frame));
        }
        if self.scan.is_some() {
            subs.push(iced::time::every(Duration::from_millis(80)).map(|_| Message::ScanPoll));
        }
        if self.config.color_mode == ColorMode::System {
            subs.push(iced::system::theme_changes().map(Message::SystemTheme));
        }
        Subscription::batch(subs)
    }

    /// Frames run only while something moves, so idle CPU stays near zero.
    fn animating(&self) -> bool {
        self.play_state == PlayState::Playing
            || self.peaks.iter().any(|&p| p > 0.001)
            || (self.seek_lift - self.lift_target()).abs() > 0.002
            || (self.seek_drag - self.drag_target()).abs() > 0.002
            || self.glide.is_some()
            || self.fade_start.is_some()
            || self.jump.is_some()
            || self.screenshot.is_some()
            || !self.first_frame_seen
    }

    fn lift_target(&self) -> f32 {
        if self.seek_hover || self.seek_preview.is_some() { 1.0 } else { 0.0 }
    }

    fn drag_target(&self) -> f32 {
        if self.seek_preview.is_some() { 1.0 } else { 0.0 }
    }

    fn frame(&mut self, now: Instant) -> Task<Message> {
        if !self.first_frame_seen {
            self.first_frame_seen = true;
            if let Some(launch) = crate::LAUNCH.get() {
                log::debug!("first frame: {:?} after launch", launch.elapsed());
            }
        }
        let dt = now.saturating_duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        self.poll_engine();

        match (&self.tap, &mut self.analyzer, self.play_state) {
            (Some(tap), Some(analyzer), PlayState::Playing) => {
                let analysis = analyzer.update(tap, Duration::from_secs_f32(dt));
                self.bars.clear();
                self.bars.extend_from_slice(&analysis.bars);
                self.bars.resize(SPECTRUM_BARS, 0.0);
            }
            _ => {
                let k = (-dt * 8.0).exp();
                self.bars.iter_mut().for_each(|b| *b *= k);
            }
        }
        spectrum::update_peaks(&self.bars, &mut self.peaks, dt);

        // Soft spring toward the targets (SPEC 4.3 motion).
        let k = 1.0 - (-dt * 16.0).exp();
        self.seek_lift += (self.lift_target() - self.seek_lift) * k;
        self.seek_drag += (self.drag_target() - self.seek_drag) * k;
        if self.glide.as_ref().is_some_and(|g| now.saturating_duration_since(g.start) >= GLIDE) {
            self.glide = None;
        }

        if let Some(start) = self.fade_start {
            let t = now.saturating_duration_since(start).as_secs_f32() / PALETTE_FADE.as_secs_f32();
            if t >= 1.0 {
                self.colors = self.fade_to;
                self.fade_start = None;
            } else {
                let eased = 1.0 - (1.0 - t).powi(3);
                self.colors = self.fade_from.mix(&self.fade_to, eased);
            }
        }

        let timeout = Duration::from_millis(self.config.type_to_jump_timeout_ms);
        if self.jump.as_ref().is_some_and(|j| now.saturating_duration_since(j.at) > timeout) {
            self.jump = None;
        }

        if self.play_state == PlayState::Playing && now.saturating_duration_since(self.last_position_save) > Duration::from_secs(5) {
            self.last_position_save = now;
            self.persist.state("position_ms", self.position.as_millis().to_string());
        }

        if let Some((_, due)) = &self.screenshot
            && now >= *due
            && let Some(id) = self.window
        {
            *due_mut(&mut self.screenshot) = now + Duration::from_secs(3600);
            return window::screenshot(id).map(Message::Shot);
        }
        Task::none()
    }

    fn poll_engine(&mut self) {
        let Some(events) = self.events.clone() else { return };
        while let Ok(event) = events.try_recv() {
            match event {
                AudioEvent::Started { id, duration } => {
                    let id = id as TrackId;
                    if self.queue.now != Some(id) && self.queue.upcoming() == Some(id) {
                        self.queue.advanced_to(id);
                        self.queue_changed();
                    }
                    self.duration = duration.or(self.duration);
                }
                AudioEvent::Advanced { to, .. } => {
                    self.queue.advanced_to(to as TrackId);
                    self.duration = self.model.track(to as TrackId).map(|t| Duration::from_millis(t.duration_ms));
                    self.queue_changed();
                }
                AudioEvent::Finished { .. } => {
                    let step = self.queue.finished();
                    self.apply(step);
                }
                AudioEvent::Failed { id, error } => {
                    log::warn!("cannot play track {id}: {error}");
                }
            }
        }
        if let Some(engine) = &self.engine {
            let status = engine.status();
            if self.play_state != PlayState::Stopped || status.state != PlayState::Stopped {
                self.play_state = status.state;
                self.position = status.position;
                if status.duration.is_some() {
                    self.duration = status.duration;
                }
            }
        }
    }

    /// Take over the engine from its start-up thread once ready; `wait` blocks for it.
    fn adopt_engine(&mut self, wait: bool) {
        let Some(handle) = self.engine_init.take_if(|h| wait || h.is_finished()) else { return };
        match handle.join() {
            Ok(Ok(engine)) => {
                engine.set_volume(self.volume);
                engine.set_muted(self.muted);
                self.events = Some(engine.events());
                self.tap = Some(engine.tap());
                self.analyzer = Some(Analyzer::new(SPECTRUM_BARS));
                self.engine = Some(engine);
            }
            Ok(Err(err)) => log::error!("audio engine unavailable: {err}"),
            Err(_) => log::error!("audio engine start-up panicked"),
        }
    }

    fn with_window(&self, f: impl FnOnce(window::Id) -> Task<Message>) -> Task<Message> {
        self.window.map(f).unwrap_or_else(Task::none)
    }

    // ---- playback ----

    fn engine_track(&self, id: TrackId) -> Option<vibrance_audio::Track> {
        self.model.track(id).map(|t| vibrance_audio::Track { id: id as u64, path: t.path.clone() })
    }

    fn apply(&mut self, step: Step) {
        match step {
            Step::Play(id) => self.start(id, Duration::ZERO),
            Step::Restart => {
                if let Some(id) = self.queue.now {
                    self.start(id, Duration::ZERO);
                }
            }
            Step::Stop => {
                if let Some(engine) = &self.engine {
                    engine.stop();
                }
                self.play_state = PlayState::Stopped;
                self.position = Duration::ZERO;
            }
        }
        self.queue_changed();
    }

    fn start(&mut self, id: TrackId, at: Duration) {
        self.adopt_engine(true);
        let Some(track) = self.engine_track(id) else { return };
        self.duration = self.model.track(id).map(|t| Duration::from_millis(t.duration_ms));
        self.position = at;
        self.resume_at = None;
        if let Some(engine) = &self.engine {
            engine.play(track, at);
            self.play_state = PlayState::Playing;
        }
    }

    fn play_pause(&mut self) {
        match self.play_state {
            PlayState::Playing => {
                if let Some(engine) = &self.engine {
                    engine.pause();
                }
                self.play_state = PlayState::Paused;
            }
            PlayState::Paused => {
                if let Some(engine) = &self.engine {
                    engine.resume();
                }
                self.play_state = PlayState::Playing;
            }
            PlayState::Stopped => {
                if let Some(id) = self.queue.now {
                    let at = self.resume_at.take().unwrap_or_default();
                    self.start(id, at);
                    self.sync_next();
                } else if !self.queue.items.is_empty() {
                    let step = self.queue.jump(0);
                    self.apply(step);
                }
            }
        }
    }

    fn play_ids(&mut self, ids: Vec<TrackId>) {
        let step = self.queue.replace(ids);
        self.sel_queue.clear();
        self.apply(step);
    }

    /// Keep the engine's gapless preload and the saved queue in step with the queue.
    fn queue_changed(&mut self) {
        self.sync_next();
        self.persist.state("queue", self.queue.to_state());
    }

    fn sync_next(&self) {
        if let Some(engine) = &self.engine {
            engine.set_next(self.queue.upcoming().and_then(|id| self.engine_track(id)));
        }
    }

    fn set_volume(&mut self, volume: f32, muted: bool) {
        self.volume = volume.clamp(0.0, 1.0);
        self.muted = muted;
        if let Some(engine) = &self.engine {
            engine.set_volume(self.volume);
            engine.set_muted(self.muted);
        }
        self.persist.state("volume", format!("{};{}", self.volume, u8::from(self.muted)));
    }

    pub fn displayed_fraction(&self) -> f32 {
        let Some(duration) = self.duration.filter(|d| !d.is_zero()) else { return 0.0 };
        if let Some(p) = self.seek_preview {
            return p;
        }
        let actual = self.position.as_secs_f32() / duration.as_secs_f32();
        if let Some(g) = &self.glide {
            let t = (self.last_frame.saturating_duration_since(g.start).as_secs_f32() / GLIDE.as_secs_f32()).clamp(0.0, 1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            return g.from + (actual.max(g.to) - g.from) * eased;
        }
        actual.clamp(0.0, 1.0)
    }

    /// Request art for the audible track when it changes. Called after every update.
    pub fn load_art(&mut self) -> Task<Message> {
        let Some(id) = self.queue.now else { return Task::none() };
        if self.art.as_ref().is_some_and(|a| a.track == id) || self.art_loading == Some(id) {
            return Task::none();
        }
        let Some(path) = self.model.track(id).map(|t| t.path.clone()) else { return Task::none() };
        self.art_loading = Some(id);
        Task::perform(
            async move { tokio::task::spawn_blocking(move || decode_art(id, &path)).await.ok().flatten() },
            move |art| Message::ArtLoaded(id, art),
        )
    }

    // ---- library, panes, selection ----

    pub fn refresh_rows(&mut self) {
        self.artist_rows = self.model.sorted_artists(self.sort_artists);
        let artists: Vec<usize> = self.sel_artists.iter().filter_map(|k| self.model.artist_by_key(k)).collect();
        self.album_rows = self.model.sorted_albums(&artists, self.sort_albums);
        let albums: Vec<usize> = if self.sel_albums.is_empty() {
            self.album_rows.clone()
        } else {
            self.sel_albums.iter().filter_map(|k| self.model.album_by_key(k)).collect()
        };
        self.song_rows = self.model.sorted_songs(&albums, self.sort_songs);
    }

    pub fn row_count(&self, pane: Pane) -> usize {
        match pane {
            Pane::Artists => self.artist_rows.len() + 1,
            Pane::Albums => self.album_rows.len() + 1,
            Pane::Songs => self.song_rows.len(),
            Pane::Queue => self.queue.items.len(),
        }
    }

    fn select(&mut self, pane: Pane, row: usize, modifiers: Modifiers) {
        let anchor = self.anchors[pane.ix()];
        let range: Vec<usize> = match anchor {
            Some(a) if modifiers.shift() => (a.min(row)..=a.max(row)).collect(),
            _ => vec![row],
        };
        let toggle = modifiers.control() && !modifiers.shift();
        if !modifiers.shift() {
            self.anchors[pane.ix()] = Some(row);
        }
        fn apply<K: PartialEq + Clone>(sel: &mut Vec<K>, keys: Vec<K>, toggle: bool, extend: bool) {
            if toggle {
                for k in keys {
                    if let Some(i) = sel.iter().position(|s| *s == k) {
                        sel.remove(i);
                    } else {
                        sel.push(k);
                    }
                }
            } else if extend {
                for k in keys {
                    if !sel.contains(&k) {
                        sel.push(k);
                    }
                }
            } else {
                *sel = keys;
            }
        }
        let extend = modifiers.shift() && modifiers.control();
        match pane {
            Pane::Artists => {
                if row == 0 {
                    self.sel_artists.clear();
                } else {
                    let keys = range.iter().filter(|&&r| r > 0).filter_map(|&r| self.artist_rows.get(r - 1)).map(|&a| self.model.artists[a].key.clone()).collect();
                    apply(&mut self.sel_artists, keys, toggle, extend);
                }
                self.sel_albums.clear();
                self.offsets[Pane::Albums.ix()] = 0.0;
                self.offsets[Pane::Songs.ix()] = 0.0;
                self.refresh_rows();
            }
            Pane::Albums => {
                if row == 0 {
                    self.sel_albums.clear();
                } else {
                    let keys = range.iter().filter(|&&r| r > 0).filter_map(|&r| self.album_rows.get(r - 1)).map(|&a| self.model.albums[a].key.clone()).collect();
                    apply(&mut self.sel_albums, keys, toggle, extend);
                }
                self.offsets[Pane::Songs.ix()] = 0.0;
                self.refresh_rows();
            }
            Pane::Songs => {
                let keys = range.iter().filter_map(|&r| self.song_rows.get(r)).map(|&t| self.model.tracks[t].id).collect();
                apply(&mut self.sel_songs, keys, toggle, extend);
            }
            Pane::Queue => {
                let keys = range.into_iter().filter(|&r| r < self.queue.items.len()).collect();
                apply(&mut self.sel_queue, keys, toggle, extend);
            }
        }
    }

    pub fn is_selected(&self, pane: Pane, row: usize) -> bool {
        match pane {
            Pane::Artists if row == 0 => self.sel_artists.is_empty(),
            Pane::Artists => self.artist_rows.get(row - 1).is_some_and(|&a| self.sel_artists.contains(&self.model.artists[a].key)),
            Pane::Albums if row == 0 => self.sel_albums.is_empty(),
            Pane::Albums => self.album_rows.get(row - 1).is_some_and(|&a| self.sel_albums.contains(&self.model.albums[a].key)),
            Pane::Songs => self.song_rows.get(row).is_some_and(|&t| self.sel_songs.contains(&self.model.tracks[t].id)),
            Pane::Queue => self.sel_queue.contains(&row),
        }
    }

    fn activate(&mut self, pane: Pane, row: usize) {
        let ids = match pane {
            Pane::Artists if row > 0 => self.artist_rows.get(row - 1).map(|&a| self.model.artist_track_ids(a)),
            Pane::Albums if row > 0 => self.album_rows.get(row - 1).map(|&a| self.model.album_track_ids(a)),
            Pane::Songs => self.song_rows.get(row).map(|&t| vec![self.model.tracks[t].id]),
            Pane::Queue => {
                let step = self.queue.jump(row);
                self.apply(step);
                return;
            }
            _ => None,
        };
        if let Some(ids) = ids {
            self.play_ids(ids);
        }
    }

    /// Enter: play the whole selection in display order (SPEC 4.6).
    fn play_selection(&mut self, pane: Pane) {
        let ids: Vec<TrackId> = match pane {
            Pane::Artists => self.artist_rows.iter().filter(|&&a| self.sel_artists.contains(&self.model.artists[a].key)).flat_map(|&a| self.model.artist_track_ids(a)).collect(),
            Pane::Albums => self.album_rows.iter().filter(|&&a| self.sel_albums.contains(&self.model.albums[a].key)).flat_map(|&a| self.model.album_track_ids(a)).collect(),
            Pane::Songs => self.song_rows.iter().map(|&t| self.model.tracks[t].id).filter(|id| self.sel_songs.contains(id)).collect(),
            Pane::Queue => {
                if let Some(&first) = self.sel_queue.iter().min() {
                    let step = self.queue.jump(first);
                    self.apply(step);
                }
                return;
            }
        };
        if !ids.is_empty() {
            self.play_ids(ids);
        }
    }

    fn selection_ids(&self, pane: Pane) -> Vec<TrackId> {
        match pane {
            Pane::Artists => self.artist_rows.iter().filter(|&&a| self.sel_artists.contains(&self.model.artists[a].key)).flat_map(|&a| self.model.artist_track_ids(a)).collect(),
            Pane::Albums => self.album_rows.iter().filter(|&&a| self.sel_albums.contains(&self.model.albums[a].key)).flat_map(|&a| self.model.album_track_ids(a)).collect(),
            Pane::Songs => self.song_rows.iter().map(|&t| self.model.tracks[t].id).filter(|id| self.sel_songs.contains(id)).collect(),
            Pane::Queue => Vec::new(),
        }
    }

    fn sort_by(&mut self, pane: Pane, col: usize) {
        fn toggle<C: PartialEq + Copy>(current: Option<(C, bool)>, col: C, default: Option<C>) -> Option<(C, bool)> {
            match current {
                Some((c, desc)) if c == col => Some((col, !desc)),
                None if default == Some(col) => Some((col, true)),
                _ => Some((col, false)),
            }
        }
        match pane {
            Pane::Artists => {
                let c = [ArtistCol::Name, ArtistCol::Albums, ArtistCol::Tracks][col.min(2)];
                self.sort_artists = toggle(self.sort_artists, c, Some(ArtistCol::Name));
            }
            Pane::Albums => {
                let c = [AlbumCol::Name, AlbumCol::Artist, AlbumCol::Year, AlbumCol::Tracks, AlbumCol::Duration][col.min(4)];
                self.sort_albums = toggle(self.sort_albums, c, Some(AlbumCol::Artist));
            }
            Pane::Songs => {
                let c = [SongCol::Number, SongCol::Title, SongCol::Artist, SongCol::Album, SongCol::Duration][col.min(4)];
                self.sort_songs = toggle(self.sort_songs, c, None);
            }
            Pane::Queue => {}
        }
    }

    pub fn jump_term(&self, pane: Pane) -> Option<&str> {
        self.jump.as_ref().filter(|j| j.pane == pane).map(|j| j.term.as_str())
    }

    fn type_to_jump(&mut self, typed: &str) -> Task<Message> {
        let pane = self.focus;
        let term = match &self.jump {
            Some(j) if j.pane == pane => format!("{}{typed}", j.term),
            _ => typed.to_owned(),
        };
        self.jump = Some(Jump { pane, term, at: Instant::now() });
        self.jump_to_term()
    }

    fn jump_to_term(&mut self) -> Task<Message> {
        let Some(j) = &self.jump else { return Task::none() };
        let (pane, term) = (j.pane, j.term.clone());
        let labels: Vec<String> = match pane {
            Pane::Artists => self.artist_rows.iter().map(|&a| self.model.artists[a].key.clone()).collect(),
            Pane::Albums => self.album_rows.iter().map(|&a| model::fold(&self.model.albums[a].name)).collect(),
            Pane::Songs => self.song_rows.iter().map(|&t| self.model.title_key(t).to_owned()).collect(),
            Pane::Queue => self.queue.items.iter().filter_map(|&id| self.model.index.get(&id)).map(|&t| self.model.title_key(t).to_owned()).collect(),
        };
        let Some(hit) = model::best_match(labels.iter().map(String::as_str), &term) else { return Task::none() };
        let row = hit + usize::from(pane.has_all_row());
        self.select(pane, row, Modifiers::default());
        self.scroll_into_view(pane, row, true)
    }

    /// Bring `row` into view; `center` puts it mid-list (type-to-jump), otherwise minimal scroll.
    fn scroll_into_view(&mut self, pane: Pane, row: usize, center: bool) -> Task<Message> {
        let top = 4.0 + row as f32 * table::ROW_H;
        let current = self.offsets[pane.ix()];
        let height = self.viewports[pane.ix()];
        let visible = top >= current && top + table::ROW_H <= current + height;
        let target = if center && !visible {
            top - (height - table::ROW_H) / 2.0
        } else if top < current {
            top - 4.0
        } else if top + table::ROW_H > current + height {
            top + table::ROW_H - height + 4.0
        } else {
            return Task::none();
        }
        .max(0.0);
        self.offsets[pane.ix()] = target;
        operation::scroll_to(table::scroll_id(pane), iced::widget::scrollable::AbsoluteOffset { x: None, y: Some(target) })
    }

    fn move_selection(&mut self, delta: isize, extend: bool) -> Task<Message> {
        let pane = self.focus;
        let count = self.row_count(pane);
        if count == 0 {
            return Task::none();
        }
        let current = self.anchors[pane.ix()].unwrap_or(0) as isize;
        let row = (current + delta).clamp(0, count as isize - 1) as usize;
        let modifiers = if extend { Modifiers::SHIFT } else { Modifiers::default() };
        self.select(pane, row, modifiers);
        if !extend {
            self.anchors[pane.ix()] = Some(row);
        }
        self.scroll_into_view(pane, row, false)
    }

    fn key(&mut self, event: keyboard::Event) -> Task<Message> {
        let keyboard::Event::KeyPressed { key, modifiers, text, .. } = event else {
            if let keyboard::Event::ModifiersChanged(m) = event {
                self.modifiers = m;
            }
            return Task::none();
        };
        self.modifiers = modifiers;
        let ctrl = modifiers.control();
        let shift = modifiers.shift();
        let jump_active = self.jump_term(self.focus).is_some();

        match key.as_ref() {
            Key::Named(Named::Escape) => {
                if self.menu != Menu::Closed {
                    self.menu = Menu::Closed;
                } else {
                    self.jump = None;
                }
            }
            Key::Named(Named::Space) if !jump_active && !ctrl => self.play_pause(),
            Key::Named(Named::ArrowLeft) if ctrl => return self.update(Message::Prev),
            Key::Named(Named::ArrowRight) if ctrl => return self.update(Message::Next),
            Key::Named(Named::ArrowLeft) | Key::Named(Named::ArrowRight) => {
                let forward = matches!(key.as_ref(), Key::Named(Named::ArrowRight));
                if let (Some(engine), Some(duration)) = (&self.engine, self.duration) {
                    let target = if forward { self.position + Duration::from_secs(5) } else { self.position.saturating_sub(Duration::from_secs(5)) };
                    let target = target.min(duration);
                    engine.seek(target);
                    self.position = target;
                }
            }
            Key::Named(Named::ArrowUp) if ctrl => self.set_volume(self.volume + 0.05, false),
            Key::Named(Named::ArrowDown) if ctrl => self.set_volume(self.volume - 0.05, false),
            Key::Named(Named::ArrowUp) => return self.move_selection(-1, shift),
            Key::Named(Named::ArrowDown) => return self.move_selection(1, shift),
            Key::Named(Named::PageUp) => return self.move_selection(-12, shift),
            Key::Named(Named::PageDown) => return self.move_selection(12, shift),
            Key::Named(Named::Home) => return self.move_selection(-(self.row_count(self.focus) as isize), shift),
            Key::Named(Named::End) => return self.move_selection(self.row_count(self.focus) as isize, shift),
            Key::Named(Named::Tab) => {
                let i = Pane::ALL.iter().position(|&p| p == self.focus).unwrap_or(0);
                self.focus = Pane::ALL[if shift { (i + 3) % 4 } else { (i + 1) % 4 }];
                self.jump = None;
            }
            Key::Named(Named::Enter) if ctrl => {
                let ids = self.selection_ids(self.focus);
                self.queue.insert_next(&ids);
                self.queue_changed();
            }
            Key::Named(Named::Enter) if shift => {
                let ids = self.selection_ids(self.focus);
                self.queue.append(&ids);
                self.queue_changed();
            }
            Key::Named(Named::Enter) => self.play_selection(self.focus),
            Key::Named(Named::Delete) if ctrl && shift => return self.update(Message::ClearQueue),
            Key::Named(Named::Delete) if self.focus == Pane::Queue => {
                let rows = std::mem::take(&mut self.sel_queue);
                self.queue.remove(&rows);
                self.queue_changed();
            }
            Key::Named(Named::Backspace) if jump_active => {
                if let Some(j) = &mut self.jump {
                    j.term.pop();
                    j.at = Instant::now();
                    if j.term.is_empty() {
                        self.jump = None;
                    } else {
                        return self.jump_to_term();
                    }
                }
            }
            Key::Named(Named::F5) => return self.update(Message::Rescan),
            Key::Character(c) if ctrl => match c {
                "o" => return self.update(Message::ImportFolder),
                "q" => return self.update(Message::CloseWindow),
                "s" => return self.update(Message::ToggleShuffle),
                "r" => return self.update(Message::CycleRepeat),
                "m" => return self.update(Message::ToggleMute),
                "." => return self.update(Message::Stop),
                "a" => {
                    let count = self.row_count(self.focus);
                    if count > 0 {
                        let first = usize::from(self.focus.has_all_row());
                        self.anchors[self.focus.ix()] = Some(first);
                        self.select(self.focus, count - 1, Modifiers::SHIFT);
                    }
                }
                "j" => {
                    if let Some(c) = self.queue.current {
                        self.focus = Pane::Queue;
                        self.sel_queue = vec![c];
                        return self.scroll_into_view(Pane::Queue, c, true);
                    }
                }
                _ => {}
            },
            _ => {
                let printable = text.as_deref().filter(|t| !t.is_empty() && t.chars().all(|c| !c.is_control()));
                if let Some(typed) = printable
                    && !ctrl
                    && !modifiers.alt()
                    && !modifiers.logo()
                {
                    return self.type_to_jump(typed);
                }
            }
        }
        Task::none()
    }

    fn start_scan(&mut self, add: Option<PathBuf>) {
        if self.scan.is_some() {
            return;
        }
        let rx = vibrance_library::scan(self.paths.database(), add);
        self.scan = Some(Scan { rx, scanned: 0, total: 0 });
        self.scan_label = Some("Scanning…".to_owned());
    }

    fn poll_scan(&mut self) {
        let Some(scan) = &mut self.scan else { return };
        let mut upserts: Vec<Track> = Vec::new();
        let mut removed: Vec<TrackId> = Vec::new();
        let mut done = false;
        loop {
            let event = match scan.rx.try_recv() {
                Ok(event) => event,
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            };
            match event {
                ScanEvent::Progress { scanned, total } => {
                    scan.scanned = scanned;
                    scan.total = total;
                }
                ScanEvent::Upserted(tracks) => upserts.extend(tracks),
                ScanEvent::Removed(ids) => removed.extend(ids),
                ScanEvent::Done { added, updated, removed, elapsed } => {
                    log::info!("scan done in {elapsed:?}: {added} added, {updated} updated, {removed} removed");
                    done = true;
                }
                ScanEvent::Error(err) => log::warn!("scan: {err}"),
            }
        }
        self.scan_label = Some(if scan.total > 0 {
            format!("Scanning {} / {}", thousands(scan.scanned), thousands(scan.total))
        } else {
            "Scanning…".to_owned()
        });
        if !upserts.is_empty() || !removed.is_empty() {
            let mut tracks: HashMap<TrackId, Track> = std::mem::take(&mut self.model.tracks).into_iter().map(|t| (t.id, t)).collect();
            for id in removed {
                tracks.remove(&id);
            }
            for t in upserts {
                tracks.insert(t.id, t);
            }
            self.model = Model::build(tracks.into_values().collect());
            self.refresh_rows();
        }
        if done {
            self.scan = None;
            self.scan_label = None;
        }
    }

    fn retheme(&mut self) {
        let dark = match self.config.color_mode {
            ColorMode::Dark => true,
            ColorMode::Light => false,
            ColorMode::System => self.system_dark,
        };
        let target = target_colors(&self.config, self.art.as_ref().map(|a| a.seed), dark);
        if target != self.fade_to {
            self.fade_from = self.colors;
            self.fade_to = target;
            self.fade_start = Some(Instant::now());
        }
    }

    fn save_config(&self) {
        self.persist.file(self.paths.config_file(), self.config.to_toml());
    }

    fn save_session(&self) {
        self.persist.state("queue", self.queue.to_state());
        self.persist.state("position_ms", self.position.as_millis().to_string());
        self.persist.state("volume", format!("{};{}", self.volume, u8::from(self.muted)));
    }

    pub fn custom_title_bar(&self) -> bool {
        self.config.title_bar == TitleBar::Custom
    }

    /// Test and screenshot hooks (SPEC 9): `artist=swo`, `album=low`, `play`, `jump=swo`, ...
    fn run_script(&mut self, step: &str) -> Task<Message> {
        let (verb, arg) = step.split_once('=').unwrap_or((step, ""));
        match verb {
            "focus" => {
                self.focus = match arg {
                    "albums" => Pane::Albums,
                    "songs" => Pane::Songs,
                    "queue" => Pane::Queue,
                    _ => Pane::Artists,
                };
                Task::none()
            }
            "artist" | "album" => {
                self.focus = if verb == "artist" { Pane::Artists } else { Pane::Albums };
                self.jump = Some(Jump { pane: self.focus, term: arg.to_owned(), at: Instant::now() });
                let task = self.jump_to_term();
                self.jump = None;
                task
            }
            "jump" => {
                let mut tasks = Vec::new();
                for ch in arg.chars() {
                    tasks.push(self.type_to_jump(&ch.to_string()));
                }
                Task::batch(tasks)
            }
            "play" => {
                self.play_selection(self.focus);
                self.load_art()
            }
            "pause" => {
                self.play_pause();
                Task::none()
            }
            "seek" => self.update(Message::SeekCommit { fraction: arg.parse().unwrap_or(0.4), glide: false }),
            "hover-seek" => self.update(Message::SeekHover(true)),
            "menu" => self.update(Message::ToggleMenu),
            "palette-menu" => {
                self.menu = Menu::Palette;
                Task::none()
            }
            "palette" => {
                let choice = PaletteChoice::ALL.into_iter().find(|p| p.label().eq_ignore_ascii_case(arg)).unwrap_or_default();
                self.config.palette = choice;
                self.retheme();
                self.colors = self.fade_to;
                self.fade_start = None;
                Task::none()
            }
            "mode" => {
                self.config.color_mode = if arg == "light" { ColorMode::Light } else { ColorMode::Dark };
                self.retheme();
                self.colors = self.fade_to;
                self.fade_start = None;
                Task::none()
            }
            "scan" => {
                self.start_scan((!arg.is_empty()).then(|| PathBuf::from(arg)));
                Task::none()
            }
            "shuffle" => self.update(Message::ToggleShuffle),
            "repeat" => self.update(Message::CycleRepeat),
            "select-song" => {
                self.select(Pane::Songs, arg.parse().unwrap_or(0), Modifiers::default());
                Task::none()
            }
            "hover" => {
                let (pane, row) = arg.split_once(':').unwrap_or(("songs", "0"));
                let pane = match pane {
                    "artists" => Pane::Artists,
                    "albums" => Pane::Albums,
                    "queue" => Pane::Queue,
                    _ => Pane::Songs,
                };
                self.hovered[pane.ix()] = row.parse().ok();
                Task::none()
            }
            other => {
                log::warn!("unknown script step: {other}");
                Task::none()
            }
        }
    }
}

fn due_mut(s: &mut Option<(PathBuf, Instant)>) -> &mut Instant {
    &mut s.as_mut().expect("screenshot pending").1
}

fn target_colors(config: &Config, art_seed: Option<material_colors::color::Rgb>, system_dark: bool) -> Colors {
    let dark = match config.color_mode {
        ColorMode::Dark => true,
        ColorMode::Light => false,
        ColorMode::System => system_dark,
    };
    let (seed, variant) = config.palette.seed(art_seed);
    Colors::generate(seed, variant, dark)
}

/// The default layout (SPEC 5.2): Artists | Albums over Songs, Queue over Album Art.
fn default_layout() -> pane_grid::State<Slot> {
    use pane_grid::{Axis, Configuration};
    let split = |axis, ratio, a, b| Configuration::Split { axis, ratio, a: Box::new(a), b: Box::new(b) };
    pane_grid::State::with_configuration(split(
        Axis::Vertical,
        0.71,
        split(
            Axis::Horizontal,
            0.385,
            split(Axis::Vertical, 0.38, Configuration::Pane(Slot::List(Pane::Artists)), Configuration::Pane(Slot::List(Pane::Albums))),
            Configuration::Pane(Slot::List(Pane::Songs)),
        ),
        split(Axis::Horizontal, 0.5, Configuration::Pane(Slot::List(Pane::Queue)), Configuration::Pane(Slot::AlbumArt)),
    ))
}

async fn pick_folder() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Import Folder")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_path_buf())
}

fn decode_art(track: TrackId, path: &std::path::Path) -> Option<Art> {
    let large = vibrance_library::load_art(path, 1024)?;
    let small = ::image::imageops::thumbnail(&large, 160, 160 * large.height() / large.width().max(1));
    let seed = crate::theme::art_seed(&large);
    let handle = |img: ::image::RgbaImage| image::Handle::from_rgba(img.width(), img.height(), img.into_raw());
    Some(Art { track, small: handle(small), large: handle(large), seed })
}

fn save_screenshot(path: &std::path::Path, shot: &window::Screenshot) {
    let (w, h) = (shot.size.width, shot.size.height);
    match ::image::RgbaImage::from_raw(w, h, shot.rgba.to_vec()) {
        Some(img) => {
            if let Err(err) = img.save(path) {
                log::error!("cannot save screenshot: {err}");
            }
        }
        None => log::error!("screenshot buffer has unexpected size"),
    }
}

pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}
