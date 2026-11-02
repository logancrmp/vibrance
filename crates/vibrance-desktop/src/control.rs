//! Command routing: which MPRIS requests act on the engine directly (so they work while the UI
//! thread is hung) and which go to the UI as `Command`s.
//!
//! Everything here is synchronous and runs on the MPRIS thread. The engine sits behind
//! [`Transport`] so the rules can be tested with a fake.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use vibrance_audio::{Engine, PlayState};

use crate::metadata::track_object_path;
use crate::{Command, LoopStatus, PlayerState, Status};

/// How long to wait for the engine to apply a direct command before telling the UI anyway.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(100);
const SETTLE_POLL: Duration = Duration::from_millis(1);
/// A seek counts as applied once the reported position is this close to the target.
const SEEK_TOLERANCE: Duration = Duration::from_millis(250);

/// Snapshot of the engine, as far as MPRIS cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineStatus {
    pub state: Status,
    /// A track is loaded (playing, paused, or stopped at its start).
    pub loaded: bool,
    pub position: Duration,
    pub duration: Option<Duration>,
}

/// The engine operations MPRIS uses. Every method must be non-blocking.
pub trait Transport: Send + Sync + 'static {
    fn status(&self) -> EngineStatus;
    fn pause(&self);
    fn resume(&self);
    fn stop(&self);
    fn seek(&self, position: Duration);
}

impl Transport for Engine {
    fn status(&self) -> EngineStatus {
        let s = Engine::status(self);
        EngineStatus {
            state: match s.state {
                PlayState::Playing => Status::Playing,
                PlayState::Paused => Status::Paused,
                PlayState::Stopped => Status::Stopped,
            },
            loaded: s.track.is_some(),
            position: s.position,
            duration: s.duration,
        }
    }

    fn pause(&self) {
        Engine::pause(self);
    }

    fn resume(&self) {
        Engine::resume(self);
    }

    fn stop(&self) {
        Engine::stop(self);
    }

    fn seek(&self, position: Duration) {
        Engine::seek(self, position);
    }
}

pub struct Core<E> {
    engine: Arc<E>,
    /// What MPRIS publishes: the latest `PlayerState` from the UI, plus property values set
    /// over D-Bus since then. Only touched on the MPRIS thread.
    state: Mutex<PlayerState>,
    commands: async_channel::Sender<Command>,
}

impl<E: Transport> Core<E> {
    pub fn new(engine: Arc<E>, commands: async_channel::Sender<Command>) -> Core<E> {
        Core { engine, state: Mutex::new(PlayerState::default()), commands }
    }

    pub fn state(&self) -> MutexGuard<'_, PlayerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn send(&self, command: Command) {
        // Unbounded, so this only fails once the UI dropped its receiver.
        if self.commands.try_send(command).is_err() {
            log::debug!("mpris: command receiver is gone");
        }
    }

    pub fn play_pause(&self) {
        match self.engine.status().state {
            Status::Playing => self.direct(|e| e.pause(), |s| s.state != Status::Playing),
            Status::Paused => self.direct(|e| e.resume(), |s| s.state != Status::Paused),
            Status::Stopped => self.send(Command::Play),
        }
    }

    pub fn play(&self) {
        match self.engine.status().state {
            Status::Playing => {}
            Status::Paused => self.direct(|e| e.resume(), |s| s.state != Status::Paused),
            Status::Stopped => self.send(Command::Play),
        }
    }

    pub fn pause(&self) {
        if self.engine.status().state == Status::Playing {
            self.direct(|e| e.pause(), |s| s.state != Status::Playing);
        }
    }

    pub fn stop(&self) {
        if self.engine.status().state != Status::Stopped {
            self.direct(|e| e.stop(), |s| s.state == Status::Stopped);
        }
    }

    /// MPRIS `Seek`: move by `offset_us` from the current position, clamped to the track.
    /// Returns the new position for the `Seeked` signal, or `None` if nothing is loaded.
    pub fn seek_by(&self, offset_us: i64) -> Option<Duration> {
        let s = self.engine.status();
        if !s.loaded {
            return None;
        }
        let current = i64::try_from(s.position.as_micros()).unwrap_or(i64::MAX);
        Some(self.seek_to(current.saturating_add(offset_us), s))
    }

    /// MPRIS `SetPosition`: ignored unless `track_id` names the current track.
    pub fn set_position(&self, track_id: &str, position_us: i64) -> Option<Duration> {
        let current = self.state().track.as_ref().map(|t| track_object_path(t.id));
        if current.as_deref() != Some(track_id) {
            log::debug!("mpris: SetPosition for stale track {track_id}");
            return None;
        }
        let s = self.engine.status();
        if !s.loaded {
            return None;
        }
        Some(self.seek_to(position_us, s))
    }

    fn seek_to(&self, target_us: i64, s: EngineStatus) -> Duration {
        let limit = s.duration.or_else(|| self.state().track.as_ref().and_then(|t| t.duration));
        let target = clamp_position(target_us, limit);
        self.direct(|e| e.seek(target), |s| s.position.abs_diff(target) <= SEEK_TOLERANCE);
        target
    }

    /// Current position, read live from the engine.
    pub fn position(&self) -> Duration {
        let s = self.engine.status();
        if s.loaded { s.position } else { Duration::ZERO }
    }

    // Property setters. zbus answers a D-Bus `Set` by emitting PropertiesChanged with the
    // getter's value, so the requested value is recorded first; otherwise every step of a
    // panel's volume-slider drag would echo the old volume back. The UI's next `update` is
    // still the source of truth and overrides it.

    pub fn set_volume(&self, volume: f64) {
        if volume.is_finite() {
            let volume = volume.clamp(0.0, 1.0);
            self.state().volume = volume;
            self.send(Command::SetVolume(volume));
        }
    }

    pub fn set_shuffle(&self, shuffle: bool) {
        self.state().shuffle = shuffle;
        self.send(Command::SetShuffle(shuffle));
    }

    pub fn set_loop(&self, loop_status: LoopStatus) {
        self.state().loop_status = loop_status;
        self.send(Command::SetLoop(loop_status));
    }

    /// Act on the engine, wait (briefly) until its status shows the effect, then tell the UI.
    ///
    /// The engine applies commands on its decode thread a moment later. Waiting here means
    /// the UI reads the new status when it handles `Changed`, and a second media-key press right
    /// after the first sees the new state. This blocks only the MPRIS thread, for at most
    /// `SETTLE_TIMEOUT`.
    fn direct(&self, act: impl FnOnce(&E), applied: impl Fn(&EngineStatus) -> bool) {
        act(&self.engine);
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        while !applied(&self.engine.status()) && Instant::now() < deadline {
            thread::sleep(SETTLE_POLL);
        }
        self.send(Command::Changed);
    }
}

/// Microseconds (possibly negative) to a position in `[0, limit]`.
pub fn clamp_position(us: i64, limit: Option<Duration>) -> Duration {
    let d = Duration::from_micros(u64::try_from(us).unwrap_or(0));
    limit.map_or(d, |l| d.min(l))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::TrackInfo;

    /// Applies commands synchronously and records them.
    #[derive(Default)]
    struct Fake {
        inner: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        status: Option<EngineStatus>,
        calls: Vec<String>,
    }

    impl Fake {
        fn with(state: Status, position: Duration, duration: Option<Duration>) -> Arc<Fake> {
            let fake = Fake::default();
            fake.inner.lock().unwrap().status =
                Some(EngineStatus { state, loaded: true, position, duration });
            Arc::new(fake)
        }

        fn empty() -> Arc<Fake> {
            Arc::new(Fake::default())
        }

        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut self.inner.lock().unwrap().calls)
        }

        fn edit(&self, call: String, f: impl FnOnce(&mut EngineStatus)) {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(call);
            if let Some(s) = inner.status.as_mut() {
                f(s);
            }
        }
    }

    impl Transport for Fake {
        fn status(&self) -> EngineStatus {
            self.inner.lock().unwrap().status.unwrap_or(EngineStatus {
                state: Status::Stopped,
                loaded: false,
                position: Duration::ZERO,
                duration: None,
            })
        }
        fn pause(&self) {
            self.edit("pause".into(), |s| s.state = Status::Paused);
        }
        fn resume(&self) {
            self.edit("resume".into(), |s| s.state = Status::Playing);
        }
        fn stop(&self) {
            self.edit("stop".into(), |s| {
                s.state = Status::Stopped;
                s.position = Duration::ZERO;
            });
        }
        fn seek(&self, position: Duration) {
            self.edit(format!("seek {}ms", position.as_millis()), |s| s.position = position);
        }
    }

    fn core(engine: &Arc<Fake>) -> (Core<Fake>, async_channel::Receiver<Command>) {
        let (tx, rx) = async_channel::unbounded();
        (Core::new(Arc::clone(engine), tx), rx)
    }

    fn drain(rx: &async_channel::Receiver<Command>) -> Vec<Command> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn with_track(core: &Core<Fake>, id: i64, duration: Option<Duration>) {
        core.state().track = Some(TrackInfo {
            id,
            duration,
            path: PathBuf::from("/m/a.flac"),
            ..TrackInfo::default()
        });
    }

    #[test]
    fn play_pause_toggles_engine_directly() {
        let e = Fake::with(Status::Playing, secs(3), Some(secs(10)));
        let (c, rx) = core(&e);
        c.play_pause();
        assert_eq!(e.calls(), ["pause"]);
        assert_eq!(drain(&rx), [Command::Changed]);
        c.play_pause();
        assert_eq!(e.calls(), ["resume"]);
        assert_eq!(drain(&rx), [Command::Changed]);
    }

    #[test]
    fn play_and_play_pause_while_stopped_go_to_ui() {
        for loaded in [true, false] {
            let e = if loaded { Fake::with(Status::Stopped, secs(0), Some(secs(10))) } else { Fake::empty() };
            let (c, rx) = core(&e);
            c.play_pause();
            c.play();
            assert!(e.calls().is_empty());
            assert_eq!(drain(&rx), [Command::Play, Command::Play]);
        }
    }

    #[test]
    fn play_resumes_when_paused_and_is_a_noop_when_playing() {
        let e = Fake::with(Status::Paused, secs(3), None);
        let (c, rx) = core(&e);
        c.play();
        assert_eq!(e.calls(), ["resume"]);
        assert_eq!(drain(&rx), [Command::Changed]);
        c.play();
        assert!(e.calls().is_empty());
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn pause_only_acts_while_playing() {
        let e = Fake::with(Status::Playing, secs(3), None);
        let (c, rx) = core(&e);
        c.pause();
        c.pause();
        assert_eq!(e.calls(), ["pause"]);
        assert_eq!(drain(&rx), [Command::Changed]);

        let e = Fake::empty();
        let (c, rx) = core(&e);
        c.pause();
        assert!(e.calls().is_empty());
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn stop_acts_while_playing_or_paused() {
        for state in [Status::Playing, Status::Paused] {
            let e = Fake::with(state, secs(3), None);
            let (c, rx) = core(&e);
            c.stop();
            c.stop();
            assert_eq!(e.calls(), ["stop"]);
            assert_eq!(drain(&rx), [Command::Changed]);
        }
    }

    #[test]
    fn seek_is_relative_and_clamped() {
        let e = Fake::with(Status::Playing, secs(5), Some(secs(10)));
        let (c, rx) = core(&e);
        assert_eq!(c.seek_by(2_000_000), Some(secs(7)));
        assert_eq!(c.seek_by(-60_000_000), Some(secs(0)));
        assert_eq!(c.seek_by(60_000_000), Some(secs(10)));
        assert_eq!(c.seek_by(i64::MAX), Some(secs(10)));
        assert_eq!(e.calls(), ["seek 7000ms", "seek 0ms", "seek 10000ms", "seek 10000ms"]);
        assert_eq!(drain(&rx), [Command::Changed, Command::Changed, Command::Changed, Command::Changed]);
    }

    #[test]
    fn seek_uses_ui_duration_when_engine_has_none() {
        let e = Fake::with(Status::Paused, secs(5), None);
        let (c, _rx) = core(&e);
        assert_eq!(c.seek_by(100_000_000), Some(secs(105)));
        with_track(&c, 1, Some(secs(8)));
        assert_eq!(c.seek_by(100_000_000), Some(secs(8)));
    }

    #[test]
    fn seek_without_a_track_does_nothing() {
        let e = Fake::empty();
        let (c, rx) = core(&e);
        assert_eq!(c.seek_by(1_000_000), None);
        with_track(&c, 1, Some(secs(8)));
        assert_eq!(c.set_position("/org/vibrance/track/1", 1_000_000), None);
        assert!(e.calls().is_empty());
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn set_position_checks_track_id() {
        let e = Fake::with(Status::Playing, secs(5), Some(secs(10)));
        let (c, rx) = core(&e);
        // No current track in the UI state: stale.
        assert_eq!(c.set_position("/org/vibrance/track/7", 1_000_000), None);
        with_track(&c, 7, Some(secs(10)));
        assert_eq!(c.set_position("/org/vibrance/track/8", 1_000_000), None);
        assert!(e.calls().is_empty());
        assert!(drain(&rx).is_empty());

        assert_eq!(c.set_position("/org/vibrance/track/7", 2_500_000), Some(Duration::from_millis(2500)));
        assert_eq!(c.set_position("/org/vibrance/track/7", -1), Some(secs(0)));
        assert_eq!(c.set_position("/org/vibrance/track/7", 99_000_000), Some(secs(10)));
        assert_eq!(e.calls(), ["seek 2500ms", "seek 0ms", "seek 10000ms"]);
        assert_eq!(drain(&rx), [Command::Changed, Command::Changed, Command::Changed]);
    }

    #[test]
    fn position_is_live_and_zero_without_track() {
        let e = Fake::with(Status::Playing, secs(4), None);
        let (c, _rx) = core(&e);
        assert_eq!(c.position(), secs(4));
        e.seek(secs(6));
        assert_eq!(c.position(), secs(6));
        let (c, _rx) = core(&Fake::empty());
        assert_eq!(c.position(), Duration::ZERO);
    }

    #[test]
    fn setters_forward_to_ui() {
        let e = Fake::empty();
        let (c, rx) = core(&e);
        c.set_volume(0.3);
        c.set_volume(1.7);
        c.set_volume(-1.0);
        c.set_volume(f64::NAN);
        c.set_shuffle(true);
        c.set_loop(LoopStatus::Track);
        assert_eq!(
            drain(&rx),
            [
                Command::SetVolume(0.3),
                Command::SetVolume(1.0),
                Command::SetVolume(0.0),
                Command::SetShuffle(true),
                Command::SetLoop(LoopStatus::Track),
            ]
        );
        assert!(e.calls().is_empty());
        // The published values follow the request until the UI says otherwise.
        let s = c.state().clone();
        assert_eq!((s.volume, s.shuffle, s.loop_status), (0.0, true, LoopStatus::Track));
    }

    #[test]
    fn clamp_position_bounds() {
        assert_eq!(clamp_position(-5, Some(secs(1))), Duration::ZERO);
        assert_eq!(clamp_position(5, None), Duration::from_micros(5));
        assert_eq!(clamp_position(i64::MAX, Some(secs(1))), secs(1));
    }

    #[test]
    fn closed_command_channel_is_harmless() {
        let e = Fake::with(Status::Playing, secs(1), None);
        let (c, rx) = core(&e);
        drop(rx);
        c.play_pause();
        assert_eq!(e.calls(), ["pause"]);
    }
}
