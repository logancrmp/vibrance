//! State shared between the engine handle, the decode thread, and the output callback.
//!
//! Everything here is lock-free. The output callback only ever does single-word atomic loads and
//! stores and never waits for another thread.

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering, fence};
use std::time::{Duration, Instant};

use crate::{PlayState, TrackId};

/// Sentinel for "no value" in encoded words.
pub(crate) const NONE: u64 = u64::MAX;

/// A sequence lock over `N` words with a single writer.
///
/// Readers never block the writer. `try_read` gives up instead of spinning, which is what the
/// output callback uses.
pub(crate) struct SeqLock<const N: usize> {
    seq: AtomicU64,
    words: [AtomicU64; N],
}

impl<const N: usize> SeqLock<N> {
    pub fn new() -> Self {
        Self { seq: AtomicU64::new(0), words: std::array::from_fn(|_| AtomicU64::new(0)) }
    }

    /// Only one thread may ever call this for a given lock.
    pub fn write(&self, words: [u64; N]) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (slot, value) in self.words.iter().zip(words) {
            slot.store(value, Ordering::Relaxed);
        }
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    pub fn try_read(&self) -> Option<[u64; N]> {
        let before = self.seq.load(Ordering::Acquire);
        if before & 1 == 1 {
            return None;
        }
        let words = std::array::from_fn(|i| self.words[i].load(Ordering::Relaxed));
        fence(Ordering::Acquire);
        (self.seq.load(Ordering::Relaxed) == before).then_some(words)
    }

    /// Retries until a consistent snapshot is read. Not for the output callback.
    pub fn read(&self) -> [u64; N] {
        let mut spins = 0u32;
        loop {
            if let Some(words) = self.try_read() {
                return words;
            }
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }
}

/// What a segment announces when its first frame is played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Announce {
    Nothing,
    Started,
    /// Playback moved on from `from` (a gapless join, or skipping a failed track).
    Advanced { from: TrackId },
}

/// A contiguous run of one track's audio in the output stream.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SegInfo {
    pub track: Option<TrackId>,
    /// Position of the segment's first frame within its track, in output frames.
    pub pos0: u64,
    pub duration: Option<Duration>,
    pub announce: Announce,
    /// Normalization gain for this track (linear), applied from the segment's first frame.
    pub gain: f32,
}

/// The latest flush published by the decode thread.
///
/// Audio before `cut` is still valid and plays out first (0 means cut immediately). Ring data
/// from there up to `start` is stale; the new segment's audio begins at `start`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Flush {
    pub generation: u64,
    pub cut: u64,
    pub start: u64,
    pub seg: SegInfo,
}

impl Flush {
    pub fn encode(&self) -> [u64; 9] {
        let (kind, from) = match self.seg.announce {
            Announce::Nothing => (0, 0),
            Announce::Started => (1, 0),
            Announce::Advanced { from } => (2, from),
        };
        [
            self.generation,
            self.cut,
            self.start,
            self.seg.track.unwrap_or(NONE),
            self.seg.pos0,
            encode_duration(self.seg.duration),
            kind,
            from,
            u64::from(self.seg.gain.to_bits()),
        ]
    }

    pub fn decode(w: [u64; 9]) -> Self {
        let announce = match w[6] {
            1 => Announce::Started,
            2 => Announce::Advanced { from: w[7] },
            _ => Announce::Nothing,
        };
        Flush {
            generation: w[0],
            cut: w[1],
            start: w[2],
            seg: SegInfo {
                track: (w[3] != NONE).then_some(w[3]),
                pos0: w[4],
                duration: decode_duration(w[5]),
                announce,
                gain: f32::from_bits(w[8] as u32),
            },
        }
    }
}

/// A new normalization gain for a track (decode thread to output callback). It applies to
/// segments of that track in `generation` starting at ring index `from` or later: gliding if
/// one is playing, and from the first frame for those not reached yet.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GainUpdate {
    pub generation: u64,
    pub track: TrackId,
    pub from: u64,
    pub gain: f32,
}

/// Converts a gain in dB to linear, treating nonsense as no gain.
pub(crate) fn db_to_gain(db: f32) -> f32 {
    if db.is_finite() { 10f32.powf(db.clamp(-60.0, 24.0) / 20.0) } else { 1.0 }
}

/// Marks a point in the sample stream (decode thread to output callback).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Marker {
    pub generation: u64,
    /// Absolute frame index in the sample ring where this marker takes effect.
    pub start: u64,
    pub kind: MarkerKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum MarkerKind {
    /// A new track begins here, joined without a gap.
    Join { seg: SegInfo },
    /// Nothing follows; the stream ends here.
    End,
}

/// Something the output callback made audible (output callback to decode thread).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Notice {
    pub generation: u64,
    /// When the event becomes audible, in nanoseconds since the engine epoch.
    pub due_ns: u64,
    pub kind: NoticeKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NoticeKind {
    Started { track: TrackId, duration: Option<Duration> },
    Advanced { from: TrackId, to: TrackId, duration: Option<Duration> },
    Finished { track: TrackId },
}

/// What the output callback last published about playback.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CoreStatus {
    pub generation: u64,
    pub track: Option<TrackId>,
    /// Position of the current segment's first frame, in output frames.
    pub pos0: u64,
    /// Position just after the last consumed frame, in output frames.
    pub pos_end: u64,
    pub duration: Option<Duration>,
    pub ended: bool,
    /// When the frame at `pos_end` will be audible, in nanoseconds since the engine epoch.
    pub end_due_ns: u64,
    /// Absolute index of the next frame the callback will read from the ring.
    pub read_index: u64,
}

impl CoreStatus {
    pub fn encode(&self) -> [u64; 8] {
        [
            self.generation,
            self.track.unwrap_or(NONE),
            self.pos0,
            self.pos_end,
            encode_duration(self.duration),
            u64::from(self.ended),
            self.end_due_ns,
            self.read_index,
        ]
    }

    pub fn decode(w: [u64; 8]) -> Self {
        CoreStatus {
            generation: w[0],
            track: (w[1] != NONE).then_some(w[1]),
            pos0: w[2],
            pos_end: w[3],
            duration: decode_duration(w[4]),
            ended: w[5] != 0,
            end_due_ns: w[6],
            read_index: w[7],
        }
    }
}

pub(crate) struct Shared {
    state: AtomicU8,
    /// Target output gain (after the perceptual curve and mute), as f32 bits.
    gain: AtomicU32,
    pub flush: SeqLock<9>,
    pub status: SeqLock<8>,
    pub rate: u32,
    pub epoch: Instant,
    /// Whether output is paced in real time, so positions may be extrapolated from the clock.
    pub realtime: bool,
}

impl Shared {
    pub fn new(rate: u32, realtime: bool) -> Self {
        let shared = Shared {
            state: AtomicU8::new(encode_state(PlayState::Stopped)),
            gain: AtomicU32::new(1.0f32.to_bits()),
            flush: SeqLock::new(),
            status: SeqLock::new(),
            rate,
            epoch: Instant::now(),
            realtime,
        };
        let idle =
            SegInfo { track: None, pos0: 0, duration: None, announce: Announce::Nothing, gain: 1.0 };
        shared.flush.write(Flush { generation: 0, cut: 0, start: 0, seg: idle }.encode());
        shared.status.write(CoreStatus::default().encode());
        shared
    }

    pub fn state(&self) -> PlayState {
        match self.state.load(Ordering::Acquire) {
            1 => PlayState::Playing,
            2 => PlayState::Paused,
            _ => PlayState::Stopped,
        }
    }

    pub fn set_state(&self, state: PlayState) {
        self.state.store(encode_state(state), Ordering::Release);
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn now_ns(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    pub fn frames_to_duration(&self, frames: u64) -> Duration {
        let rate = u64::from(self.rate.max(1));
        Duration::from_secs(frames / rate)
            + Duration::from_nanos((frames % rate) * 1_000_000_000 / rate)
    }

    pub fn duration_to_frames(&self, d: Duration) -> u64 {
        let nanos = d.as_nanos() * u128::from(self.rate);
        u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX)
    }
}

fn encode_state(state: PlayState) -> u8 {
    match state {
        PlayState::Stopped => 0,
        PlayState::Playing => 1,
        PlayState::Paused => 2,
    }
}

fn encode_duration(d: Option<Duration>) -> u64 {
    d.map_or(NONE, |d| u64::try_from(d.as_nanos()).unwrap_or(NONE - 1))
}

fn decode_duration(w: u64) -> Option<Duration> {
    (w != NONE).then(|| Duration::from_nanos(w))
}
