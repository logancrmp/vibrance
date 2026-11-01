//! The decode thread: owns the open files, decodes ahead into the ring buffer, and turns what
//! the output callback reports back into `Event`s.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use rtrb::{Consumer, Producer};

use crate::decode::Source;
use crate::resample::Resample;
use crate::shared::{
    Announce, CoreStatus, Flush, Marker, MarkerKind, Notice, NoticeKind, SegInfo, Shared,
};
use crate::{Event, PlayState, Track, TrackId};

pub(crate) enum Command {
    Play(Track, Duration),
    SetNext(Option<Track>),
    Pause,
    Resume,
    Stop,
    Seek(Duration),
    Shutdown,
}

/// Requests to the thread that owns the output.
pub(crate) enum OutputControl {
    Suspend,
    Resume,
    Shutdown,
}

/// While playing, how often to look for notices from the output callback.
const POLL: Duration = Duration::from_millis(10);
/// While waiting for the output callback to pick up a flush.
const POLL_FAST: Duration = Duration::from_millis(1);
/// Idle this long before the device stream is suspended.
const SUSPEND_AFTER: Duration = Duration::from_millis(300);
/// Upper bound on one round of decoding before commands are checked again.
const DECODE_SLICE: Duration = Duration::from_millis(5);

pub(crate) struct Player {
    commands: Receiver<Command>,
    events: Sender<Event>,
    shared: Arc<Shared>,
    writer: Writer,
    notices: Consumer<Notice>,
    output: Option<Sender<OutputControl>>,
    rate: u32,
    generation: u64,
    state: PlayState,
    /// Tracks queued into the ring since the last flush, oldest first. The last one is the one
    /// being decoded; earlier ones may still be audible.
    chain: Vec<Entry>,
    next: Option<Preload>,
    resampler: Option<Resample>,
    spare_resamplers: Vec<Resample>,
    /// Ring index where the current resampler stream started.
    session_start: u64,
    /// Ring index of the end marker, once the last track has been fully decoded.
    eos: Option<u64>,
    pending_events: VecDeque<(u64, Event)>,
    suspended: bool,
    idle_since: Option<Instant>,
    decoded: Vec<f32>,
    converted: Vec<f32>,
}

struct Entry {
    track: Track,
    /// Ring index of this track's first frame.
    start: u64,
    /// Position of that frame within the track, in output frames.
    pos0: u64,
    source: Option<Source>,
    duration: Option<Duration>,
    frames: u64,
    /// The join marker, pushed once the track has produced audio.
    join: Option<Marker>,
}

struct Preload {
    track: Track,
    source: Source,
}

impl Player {
    pub fn new(
        commands: Receiver<Command>,
        events: Sender<Event>,
        shared: Arc<Shared>,
        samples: Producer<f32>,
        markers: Producer<Marker>,
        notices: Consumer<Notice>,
        output: Option<Sender<OutputControl>>,
    ) -> Self {
        let rate = shared.rate;
        Player {
            commands,
            events,
            shared,
            writer: Writer::new(samples, markers),
            notices,
            output,
            rate,
            generation: 0,
            state: PlayState::Stopped,
            chain: Vec::new(),
            next: None,
            resampler: None,
            spare_resamplers: Vec::new(),
            session_start: 0,
            eos: None,
            pending_events: VecDeque::new(),
            suspended: false,
            idle_since: Some(Instant::now()),
            decoded: Vec::new(),
            converted: Vec::new(),
        }
    }

    pub fn run(mut self) {
        loop {
            let command = match self.wait_time() {
                None => match self.commands.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
                Some(t) if t.is_zero() => match self.commands.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                },
                Some(t) => match self.commands.recv_timeout(t) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            if let Some(command) = command
                && !self.handle_batch(command)
            {
                return;
            }
            self.drain_notices();
            self.send_due_events();
            self.produce();
            self.manage_output();
        }
    }

    /// How long to sleep before there is work to do; `None` means until a command arrives.
    fn wait_time(&self) -> Option<Duration> {
        if self.can_produce() {
            return Some(Duration::ZERO);
        }
        let mut wait: Option<Duration> = None;
        let mut shorten = |d: Duration| wait = Some(wait.map_or(d, |w| w.min(d)));
        if let Some((due, _)) = self.pending_events.front() {
            shorten(Duration::from_nanos(due.saturating_sub(self.shared.now_ns())));
        }
        // Until the output picks up a flush, the ring is full of stale audio; check back soon
        // to refill it (a suspended device stream only picks it up once resumed).
        if self.core_status().generation < self.generation && !self.suspended {
            shorten(POLL_FAST);
        }
        if self.state == PlayState::Playing {
            shorten(POLL);
        } else if self.output.is_some()
            && !self.suspended
            && let Some(since) = self.idle_since
        {
            shorten(SUSPEND_AFTER.saturating_sub(since.elapsed()));
        }
        wait
    }

    fn can_produce(&self) -> bool {
        if self.writer.has_pending() {
            return self.writer.can_pump();
        }
        self.writer.free_frames() > 0 && self.chain.last().is_some_and(|e| e.source.is_some())
    }

    /// Handles `first` and everything else already queued. A `Play` is held back until the end
    /// of the batch so a `SetNext` sent right after it is known if the file fails to open, and
    /// a run of seeks (dragging the seek bar) only performs the last one.
    fn handle_batch(&mut self, first: Command) -> bool {
        let mut play: Option<(Track, Duration)> = None;
        let mut seek: Option<Duration> = None;
        let mut command = Some(first);
        while let Some(c) = command {
            match c {
                Command::Shutdown => return false,
                Command::Play(track, start) => {
                    self.next = None;
                    seek = None;
                    play = Some((track, start));
                }
                Command::Seek(pos) => match &mut play {
                    Some((_, start)) => *start = pos,
                    None => seek = Some(pos),
                },
                Command::SetNext(next) => {
                    if let Some(pos) = seek.take() {
                        self.seek(pos);
                    }
                    self.set_next(next, play.is_none());
                }
                Command::Pause | Command::Resume | Command::Stop => {
                    if let Some((track, start)) = play.take() {
                        self.play(track, start);
                    }
                    if let Some(pos) = seek.take() {
                        self.seek(pos);
                    }
                    match c {
                        Command::Pause => self.pause(),
                        Command::Resume => self.resume(),
                        _ => self.stop(),
                    }
                }
            }
            command = self.commands.try_recv().ok();
        }
        if let Some((track, start)) = play {
            self.play(track, start);
        }
        if let Some(pos) = seek {
            self.seek(pos);
        }
        true
    }

    fn play(&mut self, track: Track, start: Duration) {
        self.set_state(PlayState::Playing);
        self.start(track, start, Announce::Started, None);
    }

    fn pause(&mut self) {
        if self.state == PlayState::Playing {
            self.set_state(PlayState::Paused);
        }
    }

    fn resume(&mut self) {
        let loaded = !self.chain.is_empty();
        if self.state == PlayState::Paused || (self.state == PlayState::Stopped && loaded) {
            self.set_state(PlayState::Playing);
        }
    }

    fn stop(&mut self) {
        self.set_state(PlayState::Stopped);
        self.seek(Duration::ZERO);
    }

    fn seek(&mut self, pos: Duration) {
        let Some(i) = self.audible_index() else {
            return;
        };
        let track = self.chain[i].track.clone();
        // Reuse the open file when it is the one still being decoded.
        let source = if i + 1 == self.chain.len() { self.chain[i].source.take() } else { None };
        self.requeue_after(i);
        self.start(track, pos, Announce::Nothing, source);
    }

    /// Tracks decoded ahead of the audible one go back to being `next`.
    fn requeue_after(&mut self, i: usize) {
        let ahead = self.chain.split_off(i + 1);
        if self.next.is_none()
            && let Some(entry) = ahead.into_iter().next()
            && let Ok(source) = open(&entry.track.path, Duration::ZERO)
        {
            self.next = Some(Preload { track: entry.track, source });
        }
    }

    fn set_next(&mut self, next: Option<Track>, may_rejoin: bool) {
        if let (Some(n), Some(p)) = (&next, &self.next)
            && same_track(n, &p.track)
        {
            return;
        }
        if may_rejoin && let Some(i) = self.audible_index() {
            let joined = self.chain.get(i + 1).map(|e| (e.track.clone(), e.start));
            match (joined, &next) {
                // Already joined the requested track.
                (Some((t, _)), Some(n)) if same_track(&t, n) => return,
                // The decoder joined a different track that is not audible yet: undo that.
                (Some((_, at)), _) => {
                    let preload = next.and_then(|t| self.preload(t));
                    self.rejoin(i, at, preload);
                    return;
                }
                // The audible track was the last one and its end is decoded: join after all.
                (None, Some(_)) if self.eos.is_some() && i + 1 == self.chain.len() => {
                    let at = self.eos.unwrap_or_default();
                    let preload = next.and_then(|t| self.preload(t));
                    if preload.is_some() {
                        self.rejoin(i, at, preload);
                    }
                    return;
                }
                _ => {}
            }
        }
        self.next = next.and_then(|t| self.preload(t));
    }

    fn preload(&mut self, track: Track) -> Option<Preload> {
        match open(&track.path, Duration::ZERO) {
            Ok(source) => Some(Preload { track, source }),
            Err(error) => {
                self.emit(Event::Failed { id: track.id, error });
                None
            }
        }
    }

    /// Replaces whatever follows chain entry `i` with `next`, from ring index `at` on. Audio
    /// before `at` keeps playing; the switch is gapless.
    fn rejoin(&mut self, i: usize, at: u64, next: Option<Preload>) {
        self.finish_resampler();
        self.writer.truncate(at);
        self.chain.truncate(i + 1);
        let current = &self.chain[i];
        let from = current.track.id;
        let end_pos = current.pos0 + at.saturating_sub(current.start);
        let duration = current.duration;
        let start = self.writer.produced;
        self.generation += 1;
        self.eos = None;
        match next {
            Some(p) => {
                let seg = SegInfo {
                    track: Some(p.track.id),
                    pos0: 0,
                    duration: p.source.duration(),
                    announce: Announce::Advanced { from },
                };
                self.publish_flush(at, start, seg);
                self.begin_session(p.source.rate());
                self.chain.push(Entry::new(p.track, start, 0, p.source, None));
            }
            None => {
                let seg = SegInfo {
                    track: Some(from),
                    pos0: end_pos,
                    duration,
                    announce: Announce::Nothing,
                };
                self.publish_flush(at, start, seg);
                self.push_end(start);
            }
        }
    }

    /// Opens `track` (or uses `source`) at `pos` and makes it the only thing in the ring. On
    /// failure, moves on to the preloaded next track, or stops.
    fn start(
        &mut self,
        track: Track,
        pos: Duration,
        announce: Announce,
        source: Option<Source>,
    ) {
        let opened = match source {
            Some(mut source) => guard(|| source.seek(pos)).map(|()| source),
            None => open(&track.path, pos),
        };
        match opened {
            Ok(source) => self.begin(track, source, pos, announce),
            Err(error) => {
                self.emit(Event::Failed { id: track.id, error });
                match self.next.take() {
                    Some(p) => {
                        let announce = Announce::Advanced { from: track.id };
                        self.begin(p.track, p.source, Duration::ZERO, announce);
                    }
                    None => self.halt(),
                }
            }
        }
    }

    fn begin(&mut self, track: Track, source: Source, pos: Duration, announce: Announce) {
        let pos0 = self.shared.duration_to_frames(pos);
        let seg = SegInfo { track: Some(track.id), pos0, duration: source.duration(), announce };
        self.flush(seg);
        let start = self.writer.produced;
        self.begin_session(source.rate());
        self.chain = vec![Entry::new(track, start, pos0, source, None)];
    }

    /// Stops playback with nothing loaded.
    fn halt(&mut self) {
        self.set_state(PlayState::Stopped);
        self.flush(SegInfo { track: None, pos0: 0, duration: None, announce: Announce::Nothing });
        self.chain.clear();
    }

    /// Makes everything in the ring stale, immediately.
    fn flush(&mut self, seg: SegInfo) {
        self.writer.discard();
        if let Some(mut r) = self.resampler.take() {
            r.reset();
            self.spare_resamplers.push(r);
        }
        self.generation += 1;
        self.eos = None;
        self.publish_flush(0, self.writer.written, seg);
    }

    fn publish_flush(&mut self, cut: u64, start: u64, seg: SegInfo) {
        let flush = Flush { generation: self.generation, cut, start, seg };
        self.shared.flush.write(flush.encode());
    }

    /// Prepares the conversion for a stream starting at the current end of the ring.
    fn begin_session(&mut self, in_rate: u32) {
        self.session_start = self.writer.produced;
        if in_rate == self.rate {
            if let Some(r) = self.resampler.take() {
                self.spare_resamplers.push(r);
            }
            return;
        }
        if self.resampler.as_ref().is_some_and(|r| r.in_rate() == in_rate) {
            return;
        }
        if let Some(r) = self.resampler.take() {
            self.spare_resamplers.push(r);
        }
        let spare = self.spare_resamplers.iter().position(|r| r.in_rate() == in_rate);
        self.resampler = match spare {
            Some(i) => Some(self.spare_resamplers.swap_remove(i)),
            None => match Resample::new(in_rate, self.rate) {
                Ok(r) => Some(r),
                Err(e) => {
                    log::error!("{e}");
                    None
                }
            },
        };
    }

    fn finish_resampler(&mut self) {
        if let Some(r) = &mut self.resampler {
            self.converted.clear();
            if let Err(e) = r.finish(&mut self.converted) {
                log::warn!("resampler flush failed: {e}");
            }
            self.writer.append(&self.converted);
        }
    }

    fn produce(&mut self) {
        let deadline = Instant::now() + DECODE_SLICE;
        while self.produce_once() {
            if Instant::now() >= deadline || !self.commands.is_empty() {
                break;
            }
        }
    }

    fn produce_once(&mut self) -> bool {
        if self.writer.has_pending() {
            return self.writer.pump();
        }
        if self.writer.free_frames() == 0 {
            return false;
        }
        let Some(entry) = self.chain.last_mut() else {
            return false;
        };
        let Some(source) = entry.source.as_mut() else {
            return false;
        };
        self.decoded.clear();
        let decoded = &mut self.decoded;
        match guard(|| source.read(decoded)) {
            Ok(true) => {
                if let Some(join) = entry.join.take() {
                    self.writer.marker(join);
                }
                entry.frames += (self.decoded.len() / 2) as u64;
                match &mut self.resampler {
                    Some(r) => {
                        self.converted.clear();
                        if let Err(e) = r.push(&self.decoded, &mut self.converted) {
                            log::error!("resampling failed: {e}");
                        }
                        self.writer.append(&self.converted);
                    }
                    None => self.writer.append(&self.decoded),
                }
                self.writer.pump();
            }
            Ok(false) => self.end_of_source(None),
            Err(error) => self.end_of_source(Some(error)),
        }
        true
    }

    /// The track being decoded has no more audio: join the next one or mark the end.
    fn end_of_source(&mut self, error: Option<String>) {
        let Some(entry) = self.chain.last_mut() else {
            return;
        };
        entry.source = None;
        let id = entry.track.id;
        // A joined track that never produced audio is dropped; the previous one stays current.
        let never_joined = entry.join.take().is_some();
        let nothing_played = error.is_some() && entry.frames == 0 && self.chain.len() == 1;
        if let Some(error) = error {
            self.emit(Event::Failed { id, error });
        }
        if never_joined {
            self.chain.pop();
        } else if nothing_played {
            match self.next.take() {
                Some(p) => {
                    let announce = Announce::Advanced { from: id };
                    self.begin(p.track, p.source, Duration::ZERO, announce);
                }
                None => self.halt(),
            }
            return;
        }
        let from = self.chain.last().map_or(id, |e| e.track.id);
        match self.next.take() {
            Some(p) => self.join(from, p),
            None => {
                self.finish_resampler();
                let at = self.writer.produced;
                self.push_end(at);
            }
        }
    }

    fn push_end(&mut self, at: u64) {
        self.writer.marker(Marker { generation: self.generation, start: at, kind: MarkerKind::End });
        self.eos = Some(at);
    }

    /// Queues `next` to follow the current track without a gap.
    fn join(&mut self, from: TrackId, next: Preload) {
        let rate = next.source.rate();
        let continuous = self.resampler.as_ref().is_some_and(|r| r.in_rate() == rate);
        let start = if continuous {
            // Same rate: keep the resampler running so the join stays seamless.
            self.session_start + self.resampler.as_ref().map_or(0, |r| r.expected())
        } else {
            self.finish_resampler();
            let start = self.writer.produced;
            self.begin_session(rate);
            start
        };
        let seg = SegInfo {
            track: Some(next.track.id),
            pos0: 0,
            duration: next.source.duration(),
            announce: Announce::Advanced { from },
        };
        let join = Marker { generation: self.generation, start, kind: MarkerKind::Join { seg } };
        self.chain.push(Entry::new(next.track, start, 0, next.source, Some(join)));
    }

    fn drain_notices(&mut self) {
        while let Ok(notice) = self.notices.pop() {
            let due = notice.due_ns;
            match notice.kind {
                NoticeKind::Started { track, duration } => {
                    self.emit_at(due, Event::Started { id: track, duration });
                }
                NoticeKind::Advanced { from, to, duration } => {
                    if notice.generation == self.generation
                        && let Some(i) = self.audible_index()
                    {
                        self.chain.drain(..i);
                    }
                    self.emit_at(due, Event::Advanced { from, to });
                    self.emit_at(due, Event::Started { id: to, duration });
                }
                NoticeKind::Finished { track } => {
                    // A finish from before the latest flush was superseded by the user.
                    if notice.generation == self.generation {
                        self.emit_at(due, Event::Finished { id: track });
                        self.finished(track);
                    }
                }
            }
        }
    }

    /// Playback ran off the end: stop, with the finished track loaded at the start.
    fn finished(&mut self, id: TrackId) {
        self.set_state(PlayState::Stopped);
        let track = self.chain.iter().find(|e| e.track.id == id).map(|e| e.track.clone());
        if let Some(track) = track {
            self.chain.clear();
            self.start(track, Duration::ZERO, Announce::Nothing, None);
        }
    }

    fn emit(&mut self, event: Event) {
        self.emit_at(0, event);
    }

    fn emit_at(&mut self, due_ns: u64, event: Event) {
        // Keep order: an event never overtakes an earlier one.
        let due = self.pending_events.back().map_or(due_ns, |(d, _)| due_ns.max(*d));
        self.pending_events.push_back((due, event));
        self.send_due_events();
    }

    fn send_due_events(&mut self) {
        let now = self.shared.now_ns();
        while let Some((due, _)) = self.pending_events.front() {
            if *due > now {
                break;
            }
            if let Some((_, event)) = self.pending_events.pop_front() {
                let _ = self.events.send(event);
            }
        }
    }

    fn set_state(&mut self, state: PlayState) {
        self.state = state;
        self.shared.set_state(state);
        if state == PlayState::Playing {
            self.idle_since = None;
            if self.suspended
                && let Some(output) = &self.output
            {
                let _ = output.send(OutputControl::Resume);
                self.suspended = false;
            }
        } else if self.idle_since.is_none() {
            self.idle_since = Some(Instant::now());
        }
    }

    /// Suspends the device stream after a while without playback, so an idle player costs
    /// nothing. Anything pending is picked up when it resumes.
    fn manage_output(&mut self) {
        let Some(output) = &self.output else {
            return;
        };
        if self.state != PlayState::Playing
            && !self.suspended
            && self.idle_since.is_some_and(|t| t.elapsed() >= SUSPEND_AFTER)
        {
            let _ = output.send(OutputControl::Suspend);
            self.suspended = true;
        }
    }

    fn core_status(&self) -> CoreStatus {
        CoreStatus::decode(self.shared.status.read())
    }

    /// Index in `chain` of the track that is audible (or will be, once a flush is picked up).
    fn audible_index(&self) -> Option<usize> {
        if self.chain.is_empty() {
            return None;
        }
        let read = self.core_status().read_index;
        Some(self.chain.iter().rposition(|e| e.start <= read).unwrap_or(0))
    }
}

impl Entry {
    fn new(track: Track, start: u64, pos0: u64, source: Source, join: Option<Marker>) -> Self {
        let duration = source.duration();
        Entry { track, start, pos0, source: Some(source), duration, frames: 0, join }
    }
}

fn same_track(a: &Track, b: &Track) -> bool {
    a.id == b.id && a.path == b.path
}

fn open(path: &std::path::Path, pos: Duration) -> Result<Source, String> {
    guard(|| {
        let mut source = Source::open(path)?;
        if !pos.is_zero() {
            source.seek(pos)?;
        }
        Ok(source)
    })
}

/// Runs decoder code, turning a panic inside it into an error for this file only.
fn guard<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err("decoder crashed".into()))
}

/// Writes decoded audio and markers into the rings without ever blocking.
struct Writer {
    samples: Producer<f32>,
    markers: Producer<Marker>,
    /// Markers not yet in the marker ring. Samples at or after the first one's start wait.
    pending_markers: VecDeque<Marker>,
    staging: Vec<f32>,
    staged: usize,
    /// Frames handed to the writer since the engine started (written or staged).
    produced: u64,
    /// Frames pushed into the sample ring.
    written: u64,
}

impl Writer {
    fn new(samples: Producer<f32>, markers: Producer<Marker>) -> Self {
        Writer {
            samples,
            markers,
            pending_markers: VecDeque::new(),
            staging: Vec::new(),
            staged: 0,
            produced: 0,
            written: 0,
        }
    }

    fn free_frames(&self) -> usize {
        self.samples.slots() / 2
    }

    fn has_pending(&self) -> bool {
        self.staged < self.staging.len() || !self.pending_markers.is_empty()
    }

    fn pending_frames(&self) -> usize {
        (self.staging.len() - self.staged) / 2
    }

    /// Frames that may be written before the first marker still waiting for the marker ring.
    fn writable_frames(&self) -> usize {
        let frames = self.pending_frames();
        match self.pending_markers.front() {
            Some(m) => {
                frames.min(usize::try_from(m.start.saturating_sub(self.written)).unwrap_or(0))
            }
            None => frames,
        }
    }

    /// Whether `pump` would make worthwhile progress now.
    fn can_pump(&self) -> bool {
        if !self.pending_markers.is_empty() && self.markers.slots() > 0 {
            return true;
        }
        let frames = self.writable_frames();
        frames > 0 && self.free_frames() >= frames.min(1024)
    }

    fn append(&mut self, frames: &[f32]) {
        let frames = &frames[..frames.len() / 2 * 2];
        self.staging.extend_from_slice(frames);
        self.produced += (frames.len() / 2) as u64;
    }

    fn marker(&mut self, marker: Marker) {
        self.pending_markers.push_back(marker);
        self.push_markers();
    }

    fn push_markers(&mut self) {
        while let Some(&m) = self.pending_markers.front() {
            if self.markers.push(m).is_err() {
                break;
            }
            self.pending_markers.pop_front();
        }
    }

    /// Moves staged audio into the ring as space allows. Returns whether anything moved.
    fn pump(&mut self) -> bool {
        let before = self.pending_markers.len();
        self.push_markers();
        let frames = self.writable_frames().min(self.free_frames());
        if frames > 0 {
            let chunk = &self.staging[self.staged..self.staged + frames * 2];
            if self.samples.push_entire_slice(chunk).is_ok() {
                self.staged += frames * 2;
                self.written += frames as u64;
            }
        }
        if self.staged == self.staging.len() {
            self.staging.clear();
            self.staged = 0;
        }
        frames > 0 || self.pending_markers.len() < before
    }

    /// Drops everything not yet in the ring.
    fn discard(&mut self) {
        self.staging.clear();
        self.staged = 0;
        self.pending_markers.clear();
        self.produced = self.written;
    }

    /// Drops staged audio and pending markers at or after ring index `at`.
    fn truncate(&mut self, at: u64) {
        let keep_until = at.max(self.written);
        let keep = usize::try_from(keep_until - self.written).unwrap_or(usize::MAX);
        let end = (self.staged + keep * 2).min(self.staging.len());
        self.staging.truncate(end);
        self.produced = self.written + ((self.staging.len() - self.staged) / 2) as u64;
        self.pending_markers.retain(|m| m.start < keep_until);
    }
}
