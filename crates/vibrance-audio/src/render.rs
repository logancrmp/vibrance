//! The real-time side: pulls decoded audio from the ring buffer and produces the output stream.
//!
//! `Core::render` runs inside the output callback. It never allocates, locks, or blocks: it
//! reads the ring buffers, does single-word atomic loads and stores, and fills the buffer. A
//! panic (which would be a bug) is caught and turns the output silent instead of unwinding into
//! the audio backend.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use rtrb::{Consumer, Producer};

use crate::PlayState;
use crate::TrackId;
use crate::shared::{
    Announce, CoreStatus, Flush, GainUpdate, Marker, MarkerKind, Notice, NoticeKind, SegInfo,
    Shared,
};
use crate::tap::TapBuffer;

/// Fade length for pause, resume, stop, and around the cut on play and seek.
const FADE_MS: u32 = 4;
/// Volume and normalization gain changes glide over this long.
const VOLUME_GLIDE_MS: u32 = 10;
/// Gain updates remembered for segments not reached yet.
const GAIN_SLOTS: usize = 8;

pub(crate) struct Core {
    samples: Consumer<f32>,
    markers: Consumer<Marker>,
    notices: Producer<Notice>,
    gain_updates: Consumer<GainUpdate>,
    shared: Arc<Shared>,
    tap: Arc<TapBuffer>,
    rate: u64,
    generation: u64,
    /// Absolute index of the next frame in the sample ring.
    read: u64,
    /// Frames before `read` that are still physically in the ring and must be discarded.
    skip: u64,
    seg: Option<Seg>,
    ended: bool,
    pending_flush: Option<Flush>,
    /// The pending flush waits for `read` to reach its cut point instead of fading out.
    pending_cut: bool,
    transport: Fade,
    /// The current segment's normalization gain.
    norm: Glide,
    /// Latest gain per track for this generation, newest last, so a segment that starts later
    /// uses the gain set after its marker was queued.
    gains: [Option<GainUpdate>; GAIN_SLOTS],
    volume: Glide,
    tap_pos: u64,
    fade: u32,
    volume_glide: u32,
    /// When frame 0 of the current callback becomes audible.
    cb_due_ns: u64,
    /// Frames already produced in the current callback.
    cb_offset: u64,
    /// When the frame after the last consumed one becomes audible.
    end_due_ns: u64,
    poisoned: bool,
}

#[derive(Clone, Copy)]
struct Seg {
    info: SegInfo,
    /// Ring index of the segment's first frame.
    start: u64,
    consumed: u64,
    announced: bool,
}

impl Core {
    pub fn new(
        samples: Consumer<f32>,
        markers: Consumer<Marker>,
        notices: Producer<Notice>,
        gain_updates: Consumer<GainUpdate>,
        shared: Arc<Shared>,
        tap: Arc<TapBuffer>,
    ) -> Self {
        let ms = |ms: u32| (shared.rate * ms / 1000).max(1);
        Core {
            samples,
            markers,
            notices,
            gain_updates,
            rate: u64::from(shared.rate.max(1)),
            fade: ms(FADE_MS),
            volume_glide: ms(VOLUME_GLIDE_MS),
            volume: Glide::new(shared.gain()),
            shared,
            tap,
            generation: 0,
            read: 0,
            skip: 0,
            seg: None,
            ended: false,
            pending_flush: None,
            pending_cut: false,
            transport: Fade::default(),
            norm: Glide::new(1.0),
            gains: [None; GAIN_SLOTS],
            tap_pos: 0,
            cb_due_ns: 0,
            cb_offset: 0,
            end_due_ns: 0,
            poisoned: false,
        }
    }

    /// Fills `out` (interleaved stereo) completely, with silence where there is nothing to play.
    /// `due_ns` is when `out[0]` will be audible.
    pub fn render(&mut self, out: &mut [f32], due_ns: u64) {
        self.guarded(out, |core, out| {
            core.begin(due_ns);
            let frames = out.len() / 2;
            let mut done = 0;
            let mut stalls = 0;
            while done < frames {
                let rest = &mut out[done * 2..frames * 2];
                let n = match core.step(rest) {
                    Some(n) => n,
                    None => {
                        if core.transport.target > 0.0 {
                            // Starved while playing: fade back in when data returns.
                            core.transport.value = 0.0;
                        }
                        core.silence(rest)
                    }
                };
                if n == 0 {
                    stalls += 1;
                    if stalls > 64 {
                        core.silence(rest);
                        break;
                    }
                }
                done += n;
                core.cb_offset += n as u64;
            }
            core.finish();
            frames
        });
    }

    /// Renders only frames backed by decoded audio, never padding with silence. Returns the
    /// number of frames written to the front of `out`. Used by the faster-than-real-time sink.
    pub fn render_ready(&mut self, out: &mut [f32]) -> usize {
        self.guarded(out, |core, out| {
            core.begin(0);
            let frames = out.len() / 2;
            let mut done = 0;
            let mut stalls = 0;
            while done < frames {
                match core.step(&mut out[done * 2..frames * 2]) {
                    Some(0) => {
                        stalls += 1;
                        if stalls > 64 {
                            break;
                        }
                    }
                    Some(n) => {
                        done += n;
                        core.cb_offset += n as u64;
                    }
                    None => break,
                }
            }
            core.finish();
            done
        })
    }

    fn guarded(
        &mut self,
        out: &mut [f32],
        f: impl FnOnce(&mut Core, &mut [f32]) -> usize,
    ) -> usize {
        if self.poisoned {
            out.fill(0.0);
            return out.len() / 2;
        }
        match catch_unwind(AssertUnwindSafe(|| f(self, out))) {
            Ok(n) => n,
            Err(_) => {
                self.poisoned = true;
                out.fill(0.0);
                out.len() / 2
            }
        }
    }

    fn begin(&mut self, due_ns: u64) {
        self.cb_due_ns = due_ns;
        self.cb_offset = 0;
        let gain = self.shared.gain();
        if gain != self.volume.target {
            self.volume.retarget(gain, self.volume_glide);
        }
        // Before the flush, so a segment it starts already sees the newest gains.
        self.take_gain_updates();
        self.poll_flush();
    }

    fn take_gain_updates(&mut self) {
        while let Ok(update) = self.gain_updates.pop() {
            if update.generation < self.generation {
                continue;
            }
            // Replace the entry this one supersedes, or the oldest.
            let same = |g: GainUpdate| g.track == update.track && g.from == update.from;
            let slot = self
                .gains
                .iter()
                .position(|g| g.is_some_and(same))
                .or_else(|| self.gains.iter().position(Option::is_none))
                .unwrap_or(0);
            if slot + 1 < GAIN_SLOTS {
                self.gains[slot..].rotate_left(1);
            }
            self.gains[GAIN_SLOTS - 1] = Some(update);
            let applies = self.seg.is_some_and(|s| {
                s.info.track == Some(update.track) && s.start >= update.from
            });
            if update.generation == self.generation && applies {
                self.norm.retarget(update.gain, self.volume_glide);
            }
        }
    }

    /// The gain a segment of `track` starting at ring index `start` begins with: the newest
    /// update covering it, else the gain it was queued with.
    fn start_gain(&self, track: Option<TrackId>, start: u64, queued: f32) -> f32 {
        self.gains
            .iter()
            .rev()
            .flatten()
            .find(|g| g.generation == self.generation && Some(g.track) == track && start >= g.from)
            .map_or(queued, |g| g.gain)
    }

    fn finish(&mut self) {
        let first = self.tap_pos - self.cb_offset.min(self.tap_pos);
        let live = !self.transport.silent();
        self.tap.commit(first, self.tap_pos, self.cb_due_ns, live);
        let (track, pos0, pos_end, duration) = match &self.seg {
            Some(s) => (s.info.track, s.info.pos0, s.info.pos0 + s.consumed, s.info.duration),
            None => (None, 0, 0, None),
        };
        let status = CoreStatus {
            generation: self.generation,
            track,
            pos0,
            pos_end,
            duration,
            ended: self.ended,
            end_due_ns: self.end_due_ns,
            read_index: self.read,
        };
        self.shared.status.write(status.encode());
    }

    /// Renders some frames into the front of `out`. `Some(0)` means state changed and the caller
    /// should call again; `None` means there is nothing to play right now.
    fn step(&mut self, out: &mut [f32]) -> Option<usize> {
        self.discard_skipped();
        if self.skip > 0 {
            return None;
        }
        if let Some(flush) = self.pending_flush {
            if self.pending_cut {
                if self.read >= flush.cut || self.ended {
                    self.apply_flush(flush, false);
                }
            } else if self.transport.value <= 0.0 {
                self.apply_flush(flush, true);
            }
        }
        self.handle_markers();

        let fading_out = self.pending_flush.is_some() && !self.pending_cut;
        let playing = self.shared.state() == PlayState::Playing;
        let audible = playing && !fading_out && !self.ended && self.seg.is_some();
        self.transport.set(if audible { 1.0 } else { 0.0 }, self.fade);
        if self.transport.silent() {
            return None;
        }

        // Read the fill level before looking at markers: a marker is always pushed before the
        // samples that follow it, so any marker inside `avail` is already visible.
        let avail = self.samples.slots() / 2;
        let mut limit = (out.len() / 2).min(avail);
        let mut until = self.next_marker_start();
        if let Some(flush) = self.pending_flush
            && self.pending_cut
        {
            until = Some(until.map_or(flush.cut, |m| m.min(flush.cut)));
        }
        if let Some(until) = until {
            limit = limit.min(usize::try_from(until.saturating_sub(self.read)).unwrap_or(usize::MAX));
        }
        if self.transport.target <= 0.0 {
            // Stop consuming the moment the fade reaches silence, so nothing is skipped.
            limit = limit.min(self.transport.remaining());
        }
        if limit == 0 {
            if avail == 0 {
                if let Some(flush) = self.pending_flush
                    && fading_out
                {
                    // Nothing left to fade out with; cut over now.
                    self.apply_flush(flush, true);
                    return Some(0);
                }
                return None;
            }
            return Some(0);
        }
        Some(self.consume(out, limit))
    }

    fn consume(&mut self, out: &mut [f32], frames: usize) -> usize {
        let Ok(chunk) = self.samples.read_chunk(frames * 2) else {
            return 0;
        };
        let (a, b) = chunk.as_slices();
        let src = a.as_chunks::<2>().0.iter().chain(b.as_chunks::<2>().0);
        for (dst, src) in out.as_chunks_mut::<2>().0.iter_mut().zip(src) {
            let g = self.transport.next() * self.norm.next();
            let l = finite(src[0]) * g;
            let r = finite(src[1]) * g;
            self.tap.store(self.tap_pos, (l + r) * 0.5);
            self.tap_pos += 1;
            let v = self.volume.next();
            dst[0] = l * v;
            dst[1] = r * v;
        }
        chunk.commit_all();
        self.read += frames as u64;

        let mut announce = None;
        if let Some(seg) = &mut self.seg {
            if !seg.announced {
                seg.announced = true;
                announce = Some(seg.info);
            }
            seg.consumed += frames as u64;
        }
        if let Some(info) = announce {
            self.announce(info, self.cb_offset);
        }
        self.end_due_ns = self.due_at(self.cb_offset + frames as u64);
        frames
    }

    fn silence(&mut self, out: &mut [f32]) -> usize {
        out.fill(0.0);
        let frames = out.len() / 2;
        for _ in 0..frames {
            self.tap.store(self.tap_pos, 0.0);
            self.tap_pos += 1;
            self.volume.next();
        }
        frames
    }

    fn poll_flush(&mut self) {
        let Some(words) = self.shared.flush.try_read() else {
            return;
        };
        let flush = Flush::decode(words);
        let known = self.pending_flush.map_or(self.generation, |p| p.generation);
        if flush.generation <= known {
            return;
        }
        let live = self.seg.is_some() && !self.ended;
        if live && flush.cut > self.read {
            // Audio before the cut stays valid: play up to it, then switch without a gap.
            self.pending_flush = Some(flush);
            self.pending_cut = true;
        } else if live && self.transport.value > 0.0 {
            // Fade out what is playing first; `step` cuts over once the fade reaches zero.
            self.pending_flush = Some(flush);
            self.pending_cut = false;
        } else {
            self.apply_flush(flush, true);
        }
    }

    fn apply_flush(&mut self, flush: Flush, fade_in: bool) {
        while let Ok(m) = self.markers.peek() {
            if m.generation >= flush.generation {
                break;
            }
            let _ = self.markers.pop();
        }
        self.skip += flush.start.saturating_sub(self.read);
        self.read = self.read.max(flush.start);
        self.discard_skipped();
        self.generation = flush.generation;
        for slot in &mut self.gains {
            if slot.is_some_and(|g| g.generation < flush.generation) {
                *slot = None;
            }
        }
        self.norm = Glide::new(self.start_gain(flush.seg.track, flush.start, flush.seg.gain));
        self.seg = flush.seg.track.map(|_| Seg {
            info: flush.seg,
            start: flush.start,
            consumed: 0,
            announced: flush.seg.announce == Announce::Nothing,
        });
        self.ended = false;
        self.pending_flush = None;
        self.pending_cut = false;
        if fade_in {
            self.transport.value = 0.0;
        }
    }

    fn discard_skipped(&mut self) {
        if self.skip == 0 {
            return;
        }
        let avail = (self.samples.slots() / 2) as u64;
        let n = self.skip.min(avail) as usize;
        if n > 0
            && let Ok(chunk) = self.samples.read_chunk(n * 2)
        {
            chunk.commit_all();
            self.skip -= n as u64;
        }
    }

    fn next_marker_start(&self) -> Option<u64> {
        let m = self.markers.peek().ok()?;
        if m.generation < self.generation {
            // Stale; `handle_markers` drops it on the next pass.
            Some(self.read)
        } else if m.generation == self.generation {
            Some(m.start)
        } else {
            None
        }
    }

    fn handle_markers(&mut self) {
        loop {
            let Ok(&m) = self.markers.peek() else {
                return;
            };
            if m.generation < self.generation {
                let _ = self.markers.pop();
                continue;
            }
            if m.generation > self.generation || m.start > self.read {
                return;
            }
            let _ = self.markers.pop();
            match m.kind {
                MarkerKind::Join { seg } => {
                    // Each track's gain applies from its first frame, without a glide.
                    self.norm = Glide::new(self.start_gain(seg.track, m.start, seg.gain));
                    self.seg = Some(Seg { info: seg, start: m.start, consumed: 0, announced: false });
                }
                MarkerKind::End => {
                    self.ended = true;
                    self.transport.value = 0.0;
                    if let Some(track) = self.seg.and_then(|s| s.info.track) {
                        self.notify(NoticeKind::Finished { track }, self.cb_offset);
                    }
                }
            }
        }
    }

    fn announce(&mut self, info: SegInfo, offset: u64) {
        let Some(to) = info.track else {
            return;
        };
        let kind = match info.announce {
            Announce::Nothing => return,
            Announce::Started => NoticeKind::Started { track: to, duration: info.duration },
            Announce::Advanced { from } => {
                NoticeKind::Advanced { from, to, duration: info.duration }
            }
        };
        self.notify(kind, offset);
    }

    fn notify(&mut self, kind: NoticeKind, offset: u64) {
        let notice =
            Notice { generation: self.generation, due_ns: self.due_at(offset), kind };
        // A full queue drops the notice rather than waiting.
        let _ = self.notices.push(notice);
    }

    fn due_at(&self, offset: u64) -> u64 {
        self.cb_due_ns.saturating_add(offset * 1_000_000_000 / self.rate)
    }
}

#[inline]
fn finite(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

/// Linear fade between silence and full level at a fixed rate.
#[derive(Default)]
struct Fade {
    value: f32,
    target: f32,
    step: f32,
}

impl Fade {
    fn set(&mut self, target: f32, frames: u32) {
        self.target = target;
        self.step = 1.0 / frames.max(1) as f32;
    }

    #[inline]
    fn next(&mut self) -> f32 {
        if self.value < self.target {
            self.value = (self.value + self.step).min(self.target);
        } else if self.value > self.target {
            self.value = (self.value - self.step).max(self.target);
        }
        self.value
    }

    fn remaining(&self) -> usize {
        if self.step <= 0.0 {
            return 0;
        }
        ((self.value - self.target).abs() / self.step).ceil() as usize
    }

    fn silent(&self) -> bool {
        self.value <= 0.0 && self.target <= 0.0
    }
}

/// Moves to a new value over a fixed number of frames, whatever the distance.
struct Glide {
    value: f32,
    target: f32,
    step: f32,
    left: u32,
}

impl Glide {
    fn new(value: f32) -> Self {
        Glide { value, target: value, step: 0.0, left: 0 }
    }

    fn retarget(&mut self, target: f32, frames: u32) {
        self.target = target;
        self.left = frames.max(1);
        self.step = (target - self.value) / self.left as f32;
    }

    #[inline]
    fn next(&mut self) -> f32 {
        if self.left > 0 {
            self.left -= 1;
            self.value = if self.left == 0 { self.target } else { self.value + self.step };
        }
        self.value
    }
}
