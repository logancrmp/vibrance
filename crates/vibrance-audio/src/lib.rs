//! Playback engine: decoding, output, gapless transitions, and the analysis tap.
//!
//! Audio is decoded and played inside this process (SPEC 3.2). The UI owns the
//! queue; it tells the engine what to play now and what to join gaplessly next.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

mod analyzer;
mod decode;
mod engine;
mod loudness;
mod mp4;
mod output;
mod player;
mod render;
mod resample;
mod shared;
mod tap;
mod wav;

use player::Command;

/// Where the final output stream goes.
#[derive(Debug, Clone)]
pub enum Sink {
    /// The default system output device (PipeWire/Pulse via ALSA).
    Device,
    /// Write the output stream to a WAV file instead of the sound server (SPEC 9).
    /// With `realtime` false, output is produced as fast as decoding allows, and nothing is
    /// written while paused or stopped.
    ///
    /// The file is 32-bit float stereo at 48 kHz. Its header is completed when the engine is
    /// dropped.
    Wav { path: PathBuf, realtime: bool },
    /// Discard output, paced in real time.
    Null,
}

pub type TrackId = u64;

#[derive(Debug, Clone)]
pub struct Track {
    pub id: TrackId,
    pub path: PathBuf,
    /// Normalization gain in dB applied to this track (0.0: none), from its first sample.
    /// Applied after resampling and before the visualizer tap and the volume.
    pub gain_db: f32,
}

impl Track {
    /// A track played without normalization gain.
    pub fn new(id: TrackId, path: impl Into<PathBuf>) -> Track {
        Track { id, path: path.into(), gain_db: 0.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

/// Events arrive when they become audible, in order.
///
/// When playback moves on to the `set_next` track, `Advanced` comes first, then `Started` for
/// the new track. That includes moving on because the current track failed: then the order is
/// `Failed`, `Advanced`, `Started`.
#[derive(Debug, Clone)]
pub enum Event {
    /// A track became audible, after `play` or a gapless advance.
    Started { id: TrackId, duration: Option<Duration> },
    /// Playback joined the track set with `set_next` without a gap.
    Advanced { from: TrackId, to: TrackId },
    /// The current track ended with nothing queued next; playback stopped. The track stays
    /// loaded at position zero, so `resume` plays it again.
    Finished { id: TrackId },
    /// A track could not be opened or decoded. Playback moves on to `next` if set.
    ///
    /// This also fires from `set_next` when the next track cannot be opened, long before the
    /// current one ends, so the queue can pick another.
    Failed { id: TrackId, error: String },
}

#[derive(Debug, Clone, Copy)]
pub struct Status {
    pub state: PlayState,
    pub track: Option<TrackId>,
    /// Position of the audible sample within the current track.
    pub position: Duration,
    pub duration: Option<Duration>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no audio output device available")]
    NoDevice,
    #[error("audio output error: {0}")]
    Output(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not audio Vibrance can decode, or is damaged beyond use.
    #[error("cannot decode: {0}")]
    Decode(String),
}

/// Loudness of a whole file (EBU R128 / ITU BS.1770).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    /// Integrated loudness in LUFS. `f32::NEG_INFINITY` when the file is silent or holds no
    /// audio at all (nothing above the -70 LUFS gate): no gain should be derived from it.
    pub integrated_lufs: f32,
    /// Highest true peak over all channels, linear (1.0 = full scale).
    pub true_peak: f32,
}

/// Measures a file's integrated loudness and true peak at its own sample rate and channel
/// layout (LFE excluded), decoding it fully. This is slow work (a full decode plus 4x
/// oversampling for the peak), meant for a background thread; it never panics.
///
/// Errors: `Error::Io` if the file cannot be opened, `Error::Decode` if it is not decodable
/// audio. Silence and empty files are not errors; see `Loudness::integrated_lufs`.
pub fn measure_loudness(path: &Path) -> Result<Loudness, Error> {
    loudness::measure(path)
}

/// Handle to the playback engine. Cheap to call from the UI thread; no method blocks.
///
/// Commands take effect on the decode thread a moment later; `status` reflects them as soon as
/// they do. Dropping the engine stops playback and joins its threads.
pub struct Engine {
    inner: engine::Inner,
}

impl Engine {
    pub fn new(sink: Sink) -> Result<Engine, Error> {
        Ok(Engine { inner: engine::Inner::new(sink)? })
    }

    /// Replace whatever is playing and start `track` at `start`.
    ///
    /// This also clears the track set with `set_next`, which belonged to the old track; call
    /// `set_next` again right after `play`.
    pub fn play(&self, track: Track, start: Duration) {
        self.inner.send(Command::Play(track, start));
    }

    /// The track to join gaplessly when the current one ends (None: stop at the end).
    ///
    /// The file is opened right away. Changing it shortly before the end, after the old next
    /// track was already decoded ahead, still switches cleanly at the boundary. For repeat-song,
    /// pass the current track again.
    pub fn set_next(&self, next: Option<Track>) {
        self.inner.send(Command::SetNext(next));
    }

    pub fn pause(&self) {
        self.inner.send(Command::Pause);
    }

    /// Continue after `pause`, or start the loaded track after `stop` or `Finished`.
    pub fn resume(&self) {
        self.inner.send(Command::Resume);
    }

    /// Halt playback; the current track stays loaded at position zero.
    pub fn stop(&self) {
        self.inner.send(Command::Stop);
    }

    pub fn seek(&self, position: Duration) {
        self.inner.send(Command::Seek(position));
    }

    /// Slider value in 0..=1. The perceptual curve and click-free ramp are applied inside.
    pub fn set_volume(&self, volume: f32) {
        self.inner.set_volume(volume);
    }

    pub fn set_muted(&self, muted: bool) {
        self.inner.set_muted(muted);
    }

    /// Changes the normalization gain of track `id` if it is playing or queued next. A playing
    /// track glides to the new gain over about 10 ms; a queued one starts with it. Later
    /// `play` or `set_next` calls use the gain in the `Track` they are given.
    pub fn set_gain(&self, id: TrackId, gain_db: f32) {
        self.inner.send(Command::SetGain(id, gain_db));
    }

    pub fn status(&self) -> Status {
        self.inner.status()
    }

    /// The event stream. Every call returns a handle to the same queue, so each event is
    /// delivered once, to whichever receiver takes it first.
    pub fn events(&self) -> crossbeam_channel::Receiver<Event> {
        self.inner.events()
    }

    /// Reader for the most recent audible samples (before volume), for visualizers.
    pub fn tap(&self) -> Tap {
        Tap { buf: self.inner.tap() }
    }
}

/// Clonable reader over the samples the output is playing right now.
#[derive(Clone)]
pub struct Tap {
    buf: Arc<tap::TapBuffer>,
}

impl Tap {
    /// Fill `out` with the latest mono samples, oldest first, aligned to what is
    /// audible now. Returns the sample rate.
    pub fn read(&self, out: &mut [f32]) -> u32 {
        self.buf.read_at(self.buf.cursor(), out);
        self.buf.rate()
    }

    /// Index of the sample audible now, counted in samples since the engine started. Advances
    /// at the sample rate while audio is playing.
    pub fn position(&self) -> u64 {
        self.buf.cursor()
    }

    /// Like `read`, but the copied samples end just before sample index `end` (see `position`).
    /// Samples older than the history (16384 samples) read as zero.
    pub fn read_at(&self, end: u64, out: &mut [f32]) -> u32 {
        self.buf.read_at(end, out);
        self.buf.rate()
    }

    pub fn sample_rate(&self) -> u32 {
        self.buf.rate()
    }
}

/// Per-frame audio features for the visualizers (SPEC 4.4).
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    /// Log-spaced spectrum bars, each 0..=1.
    pub bars: Vec<f32>,
    /// Instant band levels, normalized so the long-term average is about 1.
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    /// Smoothed band levels (fast attack, slow release), same normalization.
    pub bass_att: f32,
    pub mid_att: f32,
    pub treble_att: f32,
    /// A beat onset was detected since the previous update.
    pub beat: bool,
    pub beat_strength: f32,
    /// Estimated tempo, once confident.
    pub bpm: Option<f32>,
    /// Latest mono samples for waveform drawing.
    pub waveform: Vec<f32>,
}

pub struct Analyzer {
    state: analyzer::State,
}

impl Analyzer {
    pub fn new(bars: usize) -> Analyzer {
        Analyzer { state: analyzer::State::new(bars) }
    }

    /// Recompute from the tap. Call once per rendered frame.
    pub fn update(&mut self, tap: &Tap, dt: Duration) -> &Analysis {
        self.state.update(tap, dt)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn handles_can_be_shared_across_threads() {
        fn shared<T: Send + Sync>() {}
        fn sent<T: Send>() {}
        shared::<super::Engine>();
        shared::<super::Tap>();
        sent::<super::Analyzer>();
    }
}
