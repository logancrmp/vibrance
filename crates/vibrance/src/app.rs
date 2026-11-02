//! Application state, messages, and the update loop. Behaviour lives in the submodules.

mod browse;
mod input;
mod playback;
mod services;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use iced::keyboard::{self, Modifiers};
use iced::widget::{image, pane_grid};
use iced::{Point, Size, Subscription, Task, event, mouse, window};
use vibrance_audio::{Analyzer, Engine, Event as AudioEvent, PlayState, Sink, Tap};
use vibrance_desktop::{Command as MprisCommand, Mpris, PlayerState};
use vibrance_library::{Library, ScanEvent, Track, TrackId, WatchEvent, Watcher};

use crate::columns::{ColId, Columns};
use crate::config::{Config, Paths, TitleBar};
use crate::layout;
use crate::model::{AlbumKey, Model};
use crate::persist::Persist;
use crate::queue::Queue;
use crate::theme::{ColorMode, Colors, PaletteChoice};
use crate::ui::spectrum;
use crate::viz;

pub use services::{Channel, MprisLink, WatcherLink};

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

    pub fn ix(self) -> usize {
        self as usize
    }

    /// Artists and Albums have a pinned "All (N)" first row.
    pub fn has_all_row(self) -> bool {
        matches!(self, Pane::Artists | Pane::Albums)
    }

    pub fn key(self) -> &'static str {
        match self {
            Pane::Artists => "artists",
            Pane::Albums => "albums",
            Pane::Songs => "songs",
            Pane::Queue => "queue",
        }
    }
}

/// What a pane-grid slot shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    List(Pane),
    AlbumArt,
}

impl Slot {
    pub fn label(self) -> &'static str {
        match self {
            Slot::List(Pane::Artists) => "Artists",
            Slot::List(Pane::Albums) => "Albums",
            Slot::List(Pane::Songs) => "Songs",
            Slot::List(Pane::Queue) => "Queue",
            Slot::AlbumArt => "Album Art",
        }
    }
}

/// What the Album Art pane shows: the cover, or the docked visualizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtMode {
    Art,
    Visualizer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Menu {
    Closed,
    Main,
    Palette,
    Layout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextKind {
    Rows(Pane),
    Header(Pane),
    Pane(Slot),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Play,
    PlayNext,
    AddToQueue,
    GoToArtist,
    GoToAlbum,
    ShowInFolder,
    Remove,
}

#[derive(Debug, Clone)]
pub struct Art {
    pub track: TrackId,
    pub large: image::Handle,
    pub small: image::Handle,
    pub seed: material_colors::color::Rgb,
    /// A copy on disk for MPRIS `artUrl`.
    pub file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Frame(Instant),
    Key(keyboard::Event, bool),
    Cursor(Point),
    MouseUp,
    WindowId(Option<window::Id>),
    WindowResized(Size),
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
    RowMenu(Pane, usize),
    RowHover(Pane, usize, bool),
    Scrolled(Pane, f32, f32),
    HeaderPress(Pane, usize),
    HeaderEnter(Pane, usize),
    HeaderMenu(Pane),
    ResizeStart(Pane, usize),
    DropHover(Option<f32>),
    PaneMenu(Slot),
    PaneResized(pane_grid::ResizeEvent),
    PaneDragged(pane_grid::DragEvent),
    ArtHover(bool),
    SetArtMode(ArtMode),
    VizFullscreen(bool),
    VizNext,
    VizLock,
    /// Open (at the cursor) or close the preset menu.
    VizMenu(bool),
    /// A preset to stay on, or `None` to change automatically again.
    VizPick(Option<usize>),
    ToggleReduceFlashing,

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
    OpenSubmenu(Menu),
    CloseMenu,
    CloseContext,
    Context(Action),
    ToggleColumn(Pane, ColId),
    ResetColumns(Pane),
    TogglePane(Slot),
    ResetLayout,
    PickPalette(PaletteChoice),
    ToggleColorMode,
    SetColorMode(ColorMode),
    SetTitleBar(TitleBar),
    ToggleNormalization,
    DelayInput(String),
    OpenSettings,
    CloseSettings,
    Folders(Vec<PathBuf>),
    RemoveFolder(PathBuf),
    TracksReloaded(Vec<Track>),
    DismissNotice,

    ImportFolder,
    FolderPicked(Option<PathBuf>),
    Rescan,
    Scan(ScanEvent),
    Watch(WatchEvent),
    Engine(AudioEvent),
    Gain(TrackId, f32, f32),
    Mpris(MprisCommand),
    MprisReady(MprisLink),
    WatcherReady(WatcherLink),
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

pub struct Jump {
    pane: Pane,
    term: String,
    at: Instant,
}

struct Glide {
    from: f32,
    to: f32,
    start: Instant,
}

/// A left press on a row that may turn into a drag.
struct Press {
    pane: Pane,
    row: usize,
    origin: Point,
    /// The row was already selected: change the selection on release, not press.
    deferred: bool,
}

pub struct Drag {
    pub source: Pane,
    pub ids: Vec<TrackId>,
    /// Queue rows being reordered.
    pub rows: Vec<usize>,
    pub title: String,
    pub detail: String,
    pub icon: &'static str,
}

struct HeaderPress {
    pane: Pane,
    index: usize,
    origin_x: f32,
    reordering: bool,
}

struct ColumnResize {
    pane: Pane,
    index: usize,
    target: (ColId, f32),
    start_x: f32,
}

pub struct ContextMenu {
    pub at: Point,
    pub kind: ContextKind,
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
    pub sorts: [Option<(ColId, bool)>; 4],
    pub columns: [Columns; 4],
    pub offsets: [f32; 4],
    viewports: [f32; 4],
    pub hovered: [Option<usize>; 4],
    pub focus: Pane,
    jump: Option<Jump>,
    modifiers: Modifiers,
    pub panes: pane_grid::State<Slot>,

    pub cursor: Point,
    press: Option<Press>,
    pub drag: Option<Drag>,
    pub drop_at: Option<usize>,
    header_press: Option<HeaderPress>,
    resize: Option<ColumnResize>,
    pub context: Option<ContextMenu>,
    /// A row to bring into view once the current update is done.
    pending_scroll: Option<(Pane, usize)>,
    pub menu: Menu,
    pub settings: bool,
    pub folders: Vec<PathBuf>,
    pub delay_input: String,
    pub notice: Option<String>,
    pub art_hover: bool,
    pub art_mode: ArtMode,
    pub viz: viz::Engine,
    pub viz_frame: Arc<viz::Frame>,
    pub viz_fullscreen: bool,
    /// The preset menu, anchored where it was opened.
    pub viz_menu: Option<Point>,
    /// The full-screen overlay stays visible until this moment.
    pub viz_ui_until: Instant,

    engine: Option<Arc<Engine>>,
    engine_init: Option<std::thread::JoinHandle<Result<Engine, vibrance_audio::Error>>>,
    tap: Option<Tap>,
    analyzer: Option<Analyzer>,
    pub queue: Queue,
    pub play_state: PlayState,
    pub position: Duration,
    pub duration: Option<Duration>,
    resume_at: Option<Duration>,
    pub volume: f32,
    pub muted: bool,
    pub failed: HashSet<TrackId>,
    pub bars: Vec<f32>,
    pub peaks: Vec<f32>,
    pub seek_hover: bool,
    pub seek_preview: Option<f32>,
    pub seek_lift: f32,
    pub seek_drag: f32,
    glide: Option<Glide>,
    pub art: Option<Art>,
    art_loading: Option<TrackId>,
    /// Seconds of playback since the current track's titles started scrolling.
    pub marquee_phase: f32,
    marquee_track: Option<TrackId>,

    pub colors: Colors,
    fade_from: Colors,
    fade_to: Colors,
    fade_start: Option<Instant>,
    system_dark: bool,

    pub window: Option<window::Id>,
    pub window_size: Size,
    pub maximized: bool,

    channels: services::Channels,
    scanning: bool,
    rescan_pending: bool,
    pub scan_label: Option<String>,
    watcher: Option<Arc<Watcher>>,
    pending_import: Option<PathBuf>,
    gain_worker: Option<Arc<AtomicBool>>,
    mpris: Option<Mpris>,
    mpris_state: Option<PlayerState>,

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

        const KEYS: [&str; 9] =
            ["queue", "volume", "position_ms", "layout", "columns.artists", "columns.albums", "columns.songs", "columns.queue", "art_pane"];
        let library = Library::open(&paths.database());
        let (tracks, saved): (Vec<Track>, [Option<String>; 9]) = match &library {
            Ok(lib) => (
                lib.tracks().unwrap_or_else(|err| {
                    log::error!("cannot load library: {err}");
                    Vec::new()
                }),
                KEYS.map(|k| lib.get_state(k).ok().flatten()),
            ),
            Err(err) => {
                log::error!("cannot open library database: {err}");
                (Vec::new(), Default::default())
            }
        };
        drop(library);
        let t_library = t0.elapsed();

        let (volume, muted) = saved[1]
            .as_deref()
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
        let mut queue = saved[0].as_deref().and_then(Queue::from_state).unwrap_or_default();
        queue.items.retain(|id| model.index.contains_key(id));
        if queue.now.is_some_and(|id| !model.index.contains_key(&id)) {
            queue.now = None;
        }
        let columns = Pane::ALL.map(|pane| match &saved[4 + pane.ix()] {
            Some(s) => Columns::from_state(pane, s),
            None => Columns::default_for(pane),
        });
        let position = saved[2].as_deref().and_then(|s| s.parse().ok()).map(Duration::from_millis).unwrap_or_default();

        let colors = target_colors(&config, None, true);
        let reduce_flashing = config.reduce_flashing;
        let now = Instant::now();
        let mut app = App {
            persist: Persist::spawn(paths.database()),
            delay_input: config.type_to_jump_timeout_ms.to_string(),
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
            sorts: [None; 4],
            columns,
            offsets: [0.0; 4],
            viewports: [200.0; 4],
            hovered: [None; 4],
            focus: Pane::Artists,
            jump: None,
            modifiers: Modifiers::default(),
            panes: saved[3].as_deref().and_then(layout::from_state).unwrap_or_else(layout::default),
            cursor: Point::ORIGIN,
            press: None,
            drag: None,
            drop_at: None,
            header_press: None,
            resize: None,
            context: None,
            pending_scroll: None,
            menu: Menu::Closed,
            settings: false,
            folders: Vec::new(),
            notice: None,
            art_hover: false,
            art_mode: if saved[8].as_deref() == Some("viz") { ArtMode::Visualizer } else { ArtMode::Art },
            viz: viz::Engine::new(reduce_flashing),
            viz_frame: Arc::new(viz::Frame::default()),
            viz_fullscreen: false,
            viz_menu: None,
            viz_ui_until: now,
            engine: None,
            engine_init,
            tap: None,
            analyzer: None,
            queue,
            play_state: PlayState::Stopped,
            position,
            duration: None,
            resume_at: Some(position),
            volume,
            muted,
            failed: HashSet::new(),
            bars: vec![0.0; SPECTRUM_BARS],
            peaks: vec![0.0; SPECTRUM_BARS],
            seek_hover: false,
            seek_preview: None,
            seek_lift: 0.0,
            seek_drag: 0.0,
            glide: None,
            art: None,
            art_loading: None,
            marquee_phase: 0.0,
            marquee_track: None,
            colors,
            fade_from: colors,
            fade_to: colors,
            fade_start: None,
            system_dark: true,
            window: None,
            window_size: Size::new(1600.0, 1000.0),
            maximized: false,
            channels: services::Channels::default(),
            scanning: false,
            rescan_pending: false,
            scan_label: None,
            watcher: None,
            pending_import: None,
            gain_worker: None,
            mpris: None,
            mpris_state: None,
            last_frame: now,
            last_position_save: now,
            screenshot: options.screenshot.map(|p| (p, now + options.screenshot_after)),
            first_frame_seen: false,
        };
        app.duration = app.queue.now.and_then(|id| app.model.track(id)).map(|t| Duration::from_millis(t.duration_ms));
        app.pane_visibility_changed();
        log::debug!(
            "boot: library {t_library:?}, model + rows {:?}, total {:?} ({} tracks)",
            t0.elapsed() - t_library,
            t0.elapsed(),
            app.model.tracks.len()
        );

        let mut tasks = vec![window::latest().map(Message::WindowId), app.load_art()];
        if app.config.color_mode == ColorMode::System {
            tasks.push(iced::system::theme().map(Message::SystemTheme));
        }
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
        self.adopt_engine(!matches!(message, Message::Frame(_) | Message::Cursor(_)));
        let task = self.dispatch(message);
        self.publish_mpris();
        let scroll = match self.pending_scroll.take() {
            Some((pane, row)) => self.scroll_into_view(pane, row, true),
            None => Task::none(),
        };
        Task::batch([task, scroll, self.load_art()])
    }

    fn dispatch(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Frame(now) => return self.frame(now),
            Message::Key(event, captured) => return self.key(event, captured),
            Message::Cursor(p) => self.cursor_moved(p),
            Message::MouseUp => return self.mouse_up(),
            Message::WindowId(id) => self.window = id,
            Message::WindowResized(size) => {
                self.window_size = size;
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
            Message::RowPress(pane, row) => self.row_press(pane, row),
            Message::RowActivate(pane, row) => self.activate(pane, row),
            Message::RowMenu(pane, row) => self.row_menu(pane, row),
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
            Message::HeaderPress(pane, index) => {
                self.context = None;
                self.header_press = Some(HeaderPress { pane, index, origin_x: self.cursor.x, reordering: false });
            }
            Message::HeaderEnter(pane, index) => self.header_enter(pane, index),
            Message::HeaderMenu(pane) => self.open_context(ContextKind::Header(pane)),
            Message::ResizeStart(pane, index) => {
                if let Some(target) = self.columns[pane.ix()].resize_target(index) {
                    self.resize = Some(ColumnResize { pane, index, target, start_x: self.cursor.x });
                }
            }
            Message::DropHover(y) => {
                if self.drag.is_some() {
                    let len = self.queue.items.len();
                    self.drop_at = y.map(|y| crate::ui::table::drop_index(y, self.offsets[Pane::Queue.ix()], len));
                }
            }
            Message::PaneMenu(slot) => {
                if layout::can_hide(slot) {
                    self.open_context(ContextKind::Pane(slot));
                }
            }
            Message::PaneResized(pane_grid::ResizeEvent { split, ratio }) => {
                self.panes.resize(split, ratio);
                self.save_layout();
            }
            Message::PaneDragged(pane_grid::DragEvent::Dropped { pane, target }) => {
                self.panes.drop(pane, target);
                self.save_layout();
            }
            Message::PaneDragged(_) => {}
            Message::ArtHover(h) => self.art_hover = h,
            Message::SetArtMode(mode) => {
                self.art_mode = mode;
                self.persist.state("art_pane", if mode == ArtMode::Visualizer { "viz" } else { "art" }.to_owned());
            }
            Message::VizFullscreen(on) => {
                self.viz_fullscreen = on;
                self.menu = Menu::Closed;
                self.context = None;
                self.viz_menu = None;
                self.viz_ui_until = Instant::now() + Duration::from_millis(2500);
                let mode = if on { window::Mode::Fullscreen } else { window::Mode::Windowed };
                return self.with_window(move |id| window::set_mode(id, mode));
            }
            Message::VizNext => self.viz.next_preset(),
            Message::ToggleReduceFlashing => {
                self.config.reduce_flashing = !self.config.reduce_flashing;
                self.viz.reduce_flashing = self.config.reduce_flashing;
                self.save_config();
            }
            Message::VizLock => self.viz.locked = !self.viz.locked,
            Message::VizMenu(open) => {
                self.viz_menu = open.then_some(self.cursor);
                self.viz_ui_until = Instant::now() + Duration::from_millis(2500);
            }
            Message::VizPick(pick) => {
                self.viz_menu = None;
                match pick {
                    Some(index) => self.viz.pick(index),
                    None => self.viz.locked = false,
                }
            }

            Message::PlayPause => self.play_pause(),
            Message::Stop => self.stop(),
            Message::Next => {
                let step = self.queue.next();
                self.apply(step);
            }
            Message::Prev => {
                let step = if self.position > Duration::from_secs(3) { crate::queue::Step::Restart } else { self.queue.prev() };
                self.apply(step);
            }
            Message::SeekHover(h) => self.seek_hover = h,
            Message::SeekPreview(f) => self.seek_preview = Some(f),
            Message::SeekCommit { fraction, glide } => self.seek_commit(fraction, glide),
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

            Message::ToggleMenu => {
                self.context = None;
                self.menu = if self.menu == Menu::Closed { Menu::Main } else { Menu::Closed };
            }
            Message::OpenSubmenu(menu) => self.menu = if self.menu == menu { Menu::Main } else { menu },
            Message::CloseMenu => self.menu = Menu::Closed,
            Message::CloseContext => self.context = None,
            Message::Context(action) => return self.context_action(action),
            Message::ToggleColumn(pane, id) => {
                self.columns[pane.ix()].toggle(id);
                self.save_columns(pane);
            }
            Message::ResetColumns(pane) => {
                self.columns[pane.ix()] = Columns::default_for(pane);
                self.context = None;
                self.save_columns(pane);
            }
            Message::TogglePane(slot) => {
                if layout::is_visible(&self.panes, slot) {
                    layout::hide(&mut self.panes, slot);
                } else {
                    layout::show(&mut self.panes, slot);
                }
                self.context = None;
                self.pane_visibility_changed();
            }
            Message::ResetLayout => {
                self.panes = layout::default();
                self.menu = Menu::Closed;
                self.pane_visibility_changed();
            }
            Message::PickPalette(choice) => {
                self.config.palette = choice;
                self.save_config();
                self.retheme();
            }
            Message::ToggleColorMode => {
                let mode = if self.dark() { ColorMode::Light } else { ColorMode::Dark };
                return self.dispatch(Message::SetColorMode(mode));
            }
            Message::SetColorMode(mode) => {
                self.config.color_mode = mode;
                self.save_config();
                self.retheme();
                if mode == ColorMode::System {
                    return iced::system::theme().map(Message::SystemTheme);
                }
            }
            Message::SetTitleBar(bar) => {
                if self.config.title_bar != bar {
                    self.config.title_bar = bar;
                    self.save_config();
                    return self.with_window(window::toggle_decorations);
                }
            }
            Message::ToggleNormalization => self.toggle_normalization(),
            Message::DelayInput(s) => {
                let digits: String = s.chars().filter(char::is_ascii_digit).take(5).collect();
                if let Ok(ms) = digits.parse::<u64>()
                    && (200..=5000).contains(&ms)
                {
                    self.config.type_to_jump_timeout_ms = ms;
                    self.save_config();
                }
                self.delay_input = digits;
            }
            Message::OpenSettings => {
                self.menu = Menu::Closed;
                self.context = None;
                self.settings = true;
                return self.load_folders();
            }
            Message::CloseSettings => {
                self.settings = false;
                self.delay_input = self.config.type_to_jump_timeout_ms.to_string();
            }
            Message::Folders(folders) => self.folders = folders,
            Message::RemoveFolder(folder) => return self.remove_folder(folder),
            Message::TracksReloaded(tracks) => {
                self.model = Model::build(tracks);
                self.refresh_rows();
                return self.load_folders();
            }
            Message::DismissNotice => self.notice = None,

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
            Message::Scan(event) => return self.scan_event(event),
            Message::Watch(event) => self.watch_event(event),
            Message::Engine(event) => self.engine_event(event),
            Message::Gain(id, gain, peak) => self.gain_measured(id, gain, peak),
            Message::Mpris(command) => return self.mpris_command(command),
            Message::MprisReady(link) => self.mpris_ready(link),
            Message::WatcherReady(link) => self.watcher_ready(link),
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
                return self.dispatch(Message::CloseWindow);
            }
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            event::listen_with(|event, status, _| match event {
                iced::Event::Keyboard(e) => Some(Message::Key(e, status == event::Status::Captured)),
                iced::Event::Mouse(mouse::Event::CursorMoved { position }) => Some(Message::Cursor(position)),
                iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => Some(Message::MouseUp),
                _ => None,
            }),
            window::resize_events().map(|(_, size)| Message::WindowResized(size)),
            window::close_requests().map(|_| Message::CloseWindow),
        ];
        if self.animating() {
            subs.push(window::frames().map(Message::Frame));
        }
        if self.config.color_mode == ColorMode::System {
            subs.push(iced::system::theme_changes().map(Message::SystemTheme));
        }
        subs.extend(self.channels.subscriptions());
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
            || (self.viz_visible() && self.viz.active())
    }

    /// The visualizer renders only while on screen: full screen, or the Album Art pane in
    /// visualizer mode. Otherwise neither the engine nor the GPU does any work for it.
    /// The full-screen overlay shows while the mouse or keyboard was used recently.
    pub fn viz_ui_visible(&self) -> bool {
        self.viz_menu.is_some() || self.last_frame <= self.viz_ui_until
    }

    pub fn viz_visible(&self) -> bool {
        self.viz_fullscreen || (self.art_mode == ArtMode::Visualizer && layout::is_visible(&self.panes, Slot::AlbumArt))
    }

    /// Width / height of the visualizer area, for laying out its shapes.
    fn viz_aspect(&self) -> f32 {
        let size = self.window_size;
        if self.viz_fullscreen {
            return size.width / size.height.max(1.0);
        }
        let top = if self.custom_title_bar() { 40.0 } else { 8.0 };
        let grid = Size::new(size.width - 16.0, size.height - top - 104.0 - 16.0);
        let regions = self.panes.layout().pane_regions(8.0, 0.0, grid);
        layout::find(&self.panes, Slot::AlbumArt)
            .and_then(|p| regions.get(&p))
            .map_or(1.0, |r| (r.width - 24.0).max(1.0) / (r.height - 40.0).max(1.0))
    }

    fn lift_target(&self) -> f32 {
        if self.seek_hover || self.seek_preview.is_some() { 1.0 } else { 0.0 }
    }

    fn drag_target(&self) -> f32 {
        if self.seek_preview.is_some() { 1.0 } else { 0.0 }
    }

    fn frame(&mut self, now: Instant) -> Task<Message> {
        let mut task = Task::none();
        if !self.first_frame_seen {
            self.first_frame_seen = true;
            if let Some(launch) = crate::LAUNCH.get() {
                log::debug!("first frame: {:?} after launch", launch.elapsed());
            }
            self.after_first_frame();
        }
        let dt = now.saturating_duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        self.poll_status();

        let viz_on = self.viz_visible();
        let aspect = if viz_on { self.viz_aspect() } else { 1.0 };
        match (&self.tap, &mut self.analyzer, self.play_state) {
            (Some(tap), Some(analyzer), PlayState::Playing) => {
                let analysis = analyzer.update(tap, Duration::from_secs_f32(dt));
                self.bars.clear();
                self.bars.extend_from_slice(&analysis.bars);
                self.bars.resize(SPECTRUM_BARS, 0.0);
                if viz_on {
                    self.viz_frame = self.viz.update(dt, Some(analysis), aspect);
                }
            }
            _ => {
                let k = (-dt * 8.0).exp();
                self.bars.iter_mut().for_each(|b| *b *= k);
                if viz_on && self.viz.active() {
                    self.viz_frame = self.viz.update(dt, None, aspect);
                }
            }
        }
        spectrum::update_peaks(&self.bars, &mut self.peaks, dt);
        if self.play_state == PlayState::Playing {
            self.marquee_phase += dt;
        }

        // Soft spring toward the targets.
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

        if let Some((_, due)) = &mut self.screenshot
            && now >= *due
            && let Some(id) = self.window
        {
            *due = now + Duration::from_secs(3600);
            task = window::screenshot(id).map(Message::Shot);
        }
        task
    }

    fn with_window(&self, f: impl FnOnce(window::Id) -> Task<Message>) -> Task<Message> {
        self.window.map(f).unwrap_or_else(Task::none)
    }

    pub fn dark(&self) -> bool {
        match self.config.color_mode {
            ColorMode::Dark => true,
            ColorMode::Light => false,
            ColorMode::System => self.system_dark,
        }
    }

    fn retheme(&mut self) {
        let target = target_colors(&self.config, self.art.as_ref().map(|a| a.seed), self.system_dark);
        if target != self.fade_to {
            self.fade_from = self.colors;
            self.fade_to = target;
            self.fade_start = Some(Instant::now());
        }
    }

    fn save_config(&self) {
        self.persist.file(self.paths.config_file(), self.config.to_toml());
    }

    fn save_layout(&self) {
        self.persist.state("layout", layout::to_state(&self.panes));
    }

    fn save_columns(&self, pane: Pane) {
        self.persist.state(&format!("columns.{}", pane.key()), self.columns[pane.ix()].to_state());
    }

    fn save_session(&self) {
        self.persist.state("queue", self.queue.to_state());
        self.persist.state("position_ms", self.position.as_millis().to_string());
        self.persist.state("volume", format!("{};{}", self.volume, u8::from(self.muted)));
        self.save_layout();
    }

    /// A hidden Artists or Albums pane counts as "All".
    fn pane_visibility_changed(&mut self) {
        if !layout::is_visible(&self.panes, Slot::List(Pane::Artists)) {
            self.sel_artists.clear();
        }
        if !layout::is_visible(&self.panes, Slot::List(Pane::Albums)) {
            self.sel_albums.clear();
        }
        self.refresh_rows();
        self.save_layout();
    }

    pub fn watching(&self) -> bool {
        self.watcher.is_some()
    }

    pub fn custom_title_bar(&self) -> bool {
        self.config.title_bar == TitleBar::Custom
    }

    pub fn jump_term(&self, pane: Pane) -> Option<&str> {
        self.jump.as_ref().filter(|j| j.pane == pane).map(|j| j.term.as_str())
    }

    /// Test and screenshot hooks, e.g. `artist=swo`, `activate=songs:3`, `drag=songs:2@0`.
    fn run_script(&mut self, step: &str) -> Task<Message> {
        let (verb, arg) = step.split_once('=').unwrap_or((step, ""));
        let pane_named = |name: &str| match name {
            "artists" => Pane::Artists,
            "albums" => Pane::Albums,
            "queue" => Pane::Queue,
            _ => Pane::Songs,
        };
        match verb {
            "focus" => {
                self.focus = pane_named(arg);
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
                let tasks: Vec<_> = arg.chars().map(|ch| self.type_to_jump(&ch.to_string())).collect();
                Task::batch(tasks)
            }
            "play" => {
                self.play_selection(self.focus);
                Task::none()
            }
            "activate" => {
                let (pane, row) = arg.split_once(':').unwrap_or(("songs", "0"));
                self.activate(pane_named(pane), row.parse().unwrap_or(0));
                Task::none()
            }
            "pause" => {
                self.play_pause();
                Task::none()
            }
            "seek" => self.dispatch(Message::SeekCommit { fraction: arg.parse().unwrap_or(0.4), glide: false }),
            "hover-seek" => self.dispatch(Message::SeekHover(true)),
            "menu" => self.dispatch(Message::ToggleMenu),
            "palette-menu" => {
                self.menu = Menu::Palette;
                Task::none()
            }
            "layout-menu" => {
                self.menu = Menu::Layout;
                Task::none()
            }
            "settings" => self.dispatch(Message::OpenSettings),
            "palette" => {
                self.config.palette = PaletteChoice::ALL.into_iter().find(|p| p.label().eq_ignore_ascii_case(arg)).unwrap_or_default();
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
            "select" => {
                let (pane, rows) = arg.split_once(':').unwrap_or(("songs", "0"));
                let pane = pane_named(pane);
                for (i, row) in rows.split('+').filter_map(|r| r.parse().ok()).enumerate() {
                    let m = if i == 0 { Modifiers::default() } else { Modifiers::CTRL };
                    self.select(pane, row, m);
                }
                self.focus = pane;
                Task::none()
            }
            "drag" => {
                let (pane, rest) = arg.split_once(':').unwrap_or(("songs", "0@0"));
                let (row, at) = rest.split_once('@').unwrap_or((rest, "0"));
                self.press = Some(Press { pane: pane_named(pane), row: row.parse().unwrap_or(0), origin: Point::ORIGIN, deferred: false });
                self.start_drag();
                self.cursor = Point::new(1200.0, 400.0);
                self.drop_at = at.parse().ok();
                Task::none()
            }
            "drop" => self.mouse_up(),
            "context" => {
                let (pane, row) = arg.split_once(':').unwrap_or(("songs", "0"));
                self.cursor = Point::new(420.0, 700.0);
                self.row_menu(pane_named(pane), row.parse().unwrap_or(0));
                Task::none()
            }
            "header-menu" => {
                self.cursor = Point::new(300.0, 650.0);
                self.open_context(ContextKind::Header(pane_named(arg)));
                Task::none()
            }
            "column" => {
                let (pane, key) = arg.split_once(':').unwrap_or(("songs", "genre"));
                let pane = pane_named(pane);
                if let Some(d) = crate::columns::defs(pane).iter().find(|d| d.key == key) {
                    self.columns[pane.ix()].toggle(d.id);
                }
                Task::none()
            }
            "hide" => {
                let slot = match arg {
                    "art" => Slot::AlbumArt,
                    other => Slot::List(pane_named(other)),
                };
                self.dispatch(Message::TogglePane(slot))
            }
            "viz" => self.dispatch(Message::SetArtMode(ArtMode::Visualizer)),
            "fullscreen" => {
                // Screenshot hook: the full-screen layout without taking over the monitor.
                self.viz_fullscreen = true;
                self.viz_ui_until = Instant::now() + Duration::from_secs(60);
                Task::none()
            }
            "viz-next" => self.dispatch(Message::VizNext),
            "viz-menu" => {
                self.cursor = Point::new(1500.0, 40.0);
                self.dispatch(Message::VizMenu(true))
            }
            "preset" => {
                self.viz.select(arg);
                self.viz.locked = true;
                Task::none()
            }
            "normalize" => {
                self.toggle_normalization();
                Task::none()
            }
            "shuffle" => self.dispatch(Message::ToggleShuffle),
            "repeat" => self.dispatch(Message::CycleRepeat),
            other => {
                log::warn!("unknown script step: {other}");
                Task::none()
            }
        }
    }
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

async fn pick_folder() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Import Folder")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_path_buf())
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
