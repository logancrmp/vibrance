//! Engine control, queue stepping, volume, normalization, and album art.

use super::*;
use crate::queue::Step;

impl App {
    /// Take over the engine from its start-up thread once ready; `wait` blocks for it.
    pub(super) fn adopt_engine(&mut self, wait: bool) {
        let Some(handle) = self.engine_init.take_if(|h| wait || h.is_finished()) else { return };
        match handle.join() {
            Ok(Ok(engine)) => {
                let engine = Arc::new(engine);
                engine.set_volume(self.volume);
                engine.set_muted(self.muted);
                self.channels.engine = Some(Channel::new(services::bridge(engine.events()), Message::Engine));
                self.tap = Some(engine.tap());
                self.analyzer = Some(Analyzer::new(SPECTRUM_BARS));
                self.start_mpris(engine.clone());
                self.engine = Some(engine);
            }
            Ok(Err(err)) => log::error!("audio engine unavailable: {err}"),
            Err(_) => log::error!("audio engine start-up panicked"),
        }
    }

    /// Position and state from the engine, once per frame and after MPRIS changes.
    pub(super) fn poll_status(&mut self) {
        let Some(engine) = &self.engine else { return };
        let status = engine.status();
        if self.play_state != PlayState::Stopped || status.state != PlayState::Stopped {
            self.play_state = status.state;
            self.position = status.position;
            if status.duration.is_some() {
                self.duration = status.duration;
            }
        }
    }

    pub(super) fn engine_event(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::Started { id, duration } => {
                let id = id as TrackId;
                self.failed.remove(&id);
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
                self.failed.insert(id as TrackId);
            }
        }
        self.poll_status();
    }

    /// Normalization gain for a track: its ReplayGain/measured gain, capped by its peak.
    pub(super) fn track_gain(&self, track: &Track) -> f32 {
        if !self.config.normalization {
            return 0.0;
        }
        let Some(gain) = track.gain_db else { return 0.0 };
        let headroom = track.peak.filter(|&p| p > 0.0).map_or(f32::INFINITY, |p| -20.0 * p.log10());
        gain.min(headroom)
    }

    pub(super) fn engine_track(&self, id: TrackId) -> Option<vibrance_audio::Track> {
        self.model.track(id).map(|t| {
            let mut track = vibrance_audio::Track::new(id as u64, t.path.clone());
            track.gain_db = self.track_gain(t);
            track
        })
    }

    pub(super) fn apply(&mut self, step: Step) {
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

    pub(super) fn stop(&mut self) {
        if let Some(engine) = &self.engine {
            engine.stop();
        }
        self.play_state = PlayState::Stopped;
        self.position = Duration::ZERO;
    }

    pub(super) fn play_pause(&mut self) {
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

    pub(super) fn play_ids(&mut self, ids: Vec<TrackId>) {
        self.play_ids_at(ids, 0);
    }

    /// Replace the queue and play from `index`, bringing that row into view.
    pub(super) fn play_ids_at(&mut self, ids: Vec<TrackId>, index: usize) {
        let step = self.queue.replace_at(ids, index);
        self.sel_queue.clear();
        self.offsets[Pane::Queue.ix()] = 0.0;
        self.apply(step);
        if let Some(current) = self.queue.current.filter(|&c| c > 0) {
            self.pending_scroll = Some((Pane::Queue, current));
        }
    }

    /// Keep the engine's gapless preload and the saved queue in step with the queue.
    pub(super) fn queue_changed(&mut self) {
        self.sync_next();
        self.persist.state("queue", self.queue.to_state());
    }

    fn sync_next(&self) {
        if let Some(engine) = &self.engine {
            engine.set_next(self.queue.upcoming().and_then(|id| self.engine_track(id)));
        }
    }

    pub(super) fn set_volume(&mut self, volume: f32, muted: bool) {
        self.volume = volume.clamp(0.0, 1.0);
        self.muted = muted;
        if let Some(engine) = &self.engine {
            engine.set_volume(self.volume);
            engine.set_muted(self.muted);
        }
        self.persist.state("volume", format!("{};{}", self.volume, u8::from(self.muted)));
    }

    pub(super) fn seek_commit(&mut self, fraction: f32, glide: bool) {
        let from = self.displayed_fraction();
        self.seek_preview = None;
        let Some(duration) = self.duration else { return };
        let target = duration.mul_f32(fraction.clamp(0.0, 1.0));
        self.seek_to(target);
        if glide {
            self.glide = Some(Glide { from, to: fraction, start: Instant::now() });
        }
    }

    pub(super) fn seek_to(&mut self, target: Duration) {
        let target = self.duration.map_or(target, |d| target.min(d));
        if self.play_state == PlayState::Stopped {
            self.resume_at = Some(target);
        } else if let Some(engine) = &self.engine {
            engine.seek(target);
            if let Some(mpris) = &self.mpris {
                mpris.seeked(target);
            }
        }
        self.position = target;
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

    pub(super) fn toggle_normalization(&mut self) {
        self.config.normalization = !self.config.normalization;
        self.save_config();
        if let Some(engine) = &self.engine {
            for id in [self.queue.now, self.queue.upcoming()].into_iter().flatten() {
                if let Some(t) = self.model.track(id) {
                    engine.set_gain(id as u64, self.track_gain(t));
                }
            }
        }
        if self.config.normalization {
            self.start_gain_worker();
        } else if let Some(cancel) = self.gain_worker.take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            self.channels.gain = None;
        }
    }

    /// A background loudness measurement finished (`id < 0`: the worker is done).
    pub(super) fn gain_measured(&mut self, id: TrackId, gain: f32, peak: f32) {
        if id < 0 {
            self.gain_worker = None;
            self.channels.gain = None;
            return;
        }
        if let Some(&i) = self.model.index.get(&id) {
            self.model.tracks[i].gain_db = Some(gain);
            self.model.tracks[i].peak = Some(peak);
        }
        let live = self.queue.now == Some(id) || self.queue.upcoming() == Some(id);
        if live
            && let (Some(engine), Some(t)) = (&self.engine, self.model.track(id))
        {
            engine.set_gain(id as u64, self.track_gain(t));
        }
    }

    /// Request art for the audible track when it changes. Called after every update.
    pub fn load_art(&mut self) -> Task<Message> {
        if self.marquee_track != self.queue.now {
            self.marquee_track = self.queue.now;
            self.marquee_phase = 0.0;
        }
        let Some(id) = self.queue.now else { return Task::none() };
        if self.art.as_ref().is_some_and(|a| a.track == id) || self.art_loading == Some(id) {
            return Task::none();
        }
        let Some(path) = self.model.track(id).map(|t| t.path.clone()) else { return Task::none() };
        self.art_loading = Some(id);
        let cache = self.paths.cache.join("art");
        Task::perform(
            async move { tokio::task::spawn_blocking(move || decode_art(id, &path, &cache)).await.ok().flatten() },
            move |art| Message::ArtLoaded(id, art),
        )
    }
}

fn decode_art(track: TrackId, path: &std::path::Path, cache: &std::path::Path) -> Option<Art> {
    let large = vibrance_library::load_art(path, 1024)?;
    let small = ::image::imageops::thumbnail(&large, 160, 160 * large.height() / large.width().max(1));
    let seed = crate::theme::art_seed(&large);

    // One file per album folder, for MPRIS clients that want a URL.
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.parent().hash(&mut h);
        h.finish()
    };
    let file = cache.join(format!("{key:016x}.jpg"));
    if !file.exists() {
        let _ = std::fs::create_dir_all(cache);
        let medium = ::image::imageops::thumbnail(&large, 512, 512 * large.height() / large.width().max(1));
        if let Err(err) = ::image::DynamicImage::ImageRgba8(medium).to_rgb8().save(&file) {
            log::debug!("cannot cache art: {err}");
        }
    }

    let handle = |img: ::image::RgbaImage| image::Handle::from_rgba(img.width(), img.height(), img.into_raw());
    Some(Art { track, small: handle(small), large: handle(large), seed, file: file.exists().then_some(file) })
}
