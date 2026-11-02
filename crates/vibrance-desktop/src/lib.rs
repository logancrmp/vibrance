//! Desktop integration over D-Bus: MPRIS (SPEC 3.2, 4.6) and "Show in Folder" (SPEC 4.7).
//!
//! The MPRIS server runs on its own thread with its own executor (zbus's `block_on`), so media
//! keys and panel widgets can pause, stop and seek even while the UI thread is hung.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use vibrance_audio::Engine;

mod control;
mod folder;
mod metadata;
mod server;

pub use metadata::{file_url, track_object_path};

/// Bus name suffix: the server registers `org.mpris.MediaPlayer2.vibrance`.
pub const BUS_NAME_SUFFIX: &str = "vibrance";

pub type TrackId = i64;

/// Requests from the desktop (media keys, panel widgets, `playerctl`).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Anything that needs the queue: start playback while stopped.
    Play,
    Next,
    Previous,
    /// The engine was paused, resumed, stopped, or seeked directly; refresh from its status.
    Changed,
    /// Volume slider value in 0..=1.
    SetVolume(f64),
    SetShuffle(bool),
    SetLoop(LoopStatus),
    Raise,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    Playing,
    Paused,
    #[default]
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopStatus {
    #[default]
    None,
    Track,
    Playlist,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackInfo {
    pub id: TrackId,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track_no: Option<u32>,
    pub duration: Option<Duration>,
    pub path: PathBuf,
    /// Cover image file for `mpris:artUrl`, if one exists on disk.
    pub art: Option<PathBuf>,
}

/// Everything MPRIS exposes. Send a fresh copy whenever any of it changes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlayerState {
    pub status: Status,
    pub track: Option<TrackInfo>,
    pub volume: f64,
    pub shuffle: bool,
    pub loop_status: LoopStatus,
    pub can_next: bool,
    pub can_previous: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("d-bus error: {0}")]
    Dbus(String),
}

/// Handle to the MPRIS server thread. Cheap to clone; never blocks.
///
/// The server thread runs until every clone is dropped, then releases the bus name.
#[derive(Clone)]
pub struct Mpris {
    messages: async_channel::Sender<server::Msg>,
    bus_name: Arc<str>,
}

impl Mpris {
    /// Register `org.mpris.MediaPlayer2.vibrance` on the session bus from a dedicated thread.
    ///
    /// Play/pause, pause, stop, and seeking while a track is loaded act on `engine`
    /// directly, so they work even if the UI thread hangs; the UI then gets `Changed`.
    ///
    /// If another process owns that name (a second Vibrance), this registers
    /// `org.mpris.MediaPlayer2.vibrance.instance<pid>` instead, as MPRIS recommends.
    ///
    /// Blocks until the name is registered (a few milliseconds normally, 5 s at most). Returns
    /// `Err` without panicking when there is no session bus.
    pub fn start(engine: Arc<Engine>) -> Result<(Mpris, async_channel::Receiver<Command>), Error> {
        let instance = format!("{BUS_NAME_SUFFIX}.instance{}", std::process::id());
        Self::spawn(engine, vec![BUS_NAME_SUFFIX.to_owned(), instance])
    }

    /// Like [`Mpris::start`], but registers exactly `org.mpris.MediaPlayer2.<bus_name_suffix>`
    /// (no fallback). For tests, so they never clash with a running Vibrance.
    ///
    /// Each dot-separated element of the suffix must match `[A-Za-z_-][A-Za-z0-9_-]*`.
    pub fn start_with_suffix(
        engine: Arc<Engine>,
        bus_name_suffix: &str,
    ) -> Result<(Mpris, async_channel::Receiver<Command>), Error> {
        Self::spawn(engine, vec![bus_name_suffix.to_owned()])
    }

    fn spawn(
        engine: Arc<Engine>,
        suffixes: Vec<String>,
    ) -> Result<(Mpris, async_channel::Receiver<Command>), Error> {
        let started = server::spawn(engine, suffixes)?;
        let mpris = Mpris { messages: started.messages, bus_name: started.bus_name.into() };
        Ok((mpris, started.commands))
    }

    /// The well-known name this server owns, e.g. `org.mpris.MediaPlayer2.vibrance`.
    pub fn bus_name(&self) -> &str {
        &self.bus_name
    }

    /// Publish new state; only changed properties are signalled.
    pub fn update(&self, state: PlayerState) {
        self.post(server::Msg::Update(state));
    }

    /// Signal `Seeked` after a jump in position (not for normal playback progress).
    pub fn seeked(&self, position: Duration) {
        self.post(server::Msg::Seeked(position));
    }

    fn post(&self, msg: server::Msg) {
        // Unbounded: never blocks. Fails only if the server thread is gone.
        if self.messages.try_send(msg).is_err() {
            log::debug!("mpris: server thread is gone");
        }
    }
}

/// Reveal `file` in the user's file manager (FileManager1.ShowItems, falling back to
/// opening the containing folder). Returns immediately.
pub fn show_in_folder(file: &Path) {
    folder::show_in_folder(file);
}

#[cfg(test)]
mod tests {
    #[test]
    fn handles_can_be_shared_across_threads() {
        fn shared<T: Send + Sync + Clone + 'static>() {}
        shared::<super::Mpris>();
    }
}
