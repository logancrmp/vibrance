//! Audio features for the visualizers: spectrum bars, band levels, beats and tempo.
//!
//! The spectrum is taken once per rendered frame from the latest 2048 samples. Beat detection
//! runs at a fixed hop over the tap's sample timeline instead, so it does not depend on the
//! frame rate.

use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::Arc;
use std::time::Duration;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::{Analysis, Tap};

const FFT_SIZE: usize = 2048;
const BINS: usize = FFT_SIZE / 2 + 1;
/// Onset detection hop, in samples (~10.7 ms at 48 kHz).
const HOP: u64 = 512;
const WAVEFORM_LEN: usize = 512;

const BAR_LOW_HZ: f32 = 40.0;
const BAR_HIGH_HZ: f32 = 16_000.0;
/// Bar energy (dB relative to a full-scale sine) mapped to 0 and 1.
const BAR_FLOOR_DB: f32 = -70.0;
const BAR_CEIL_DB: f32 = -12.0;
/// Bars rise almost at once and fall over a few frames.
const BAR_RISE_S: f32 = 0.012;
const BAR_FALL_S: f32 = 0.16;

const BANDS: [(f32, f32); 3] = [(20.0, 250.0), (250.0, 4_000.0), (4_000.0, 16_000.0)];
/// Time constant of the long-term average the bands are normalized against.
const BAND_AVERAGE_S: f32 = 8.0;
const ATT_RISE_S: f32 = 0.03;
const ATT_FALL_S: f32 = 0.35;
/// Below this band amplitude (about -80 dBFS) a band counts as silent.
const SILENCE: f32 = 1e-4;

/// Onset strength history used for the adaptive threshold (~1.5 s).
const FLUX_HISTORY: usize = 140;
/// Onset envelope kept for the tempo estimate (~6 s).
const TEMPO_HISTORY: usize = 560;
/// Re-estimate the tempo every this many hops (~0.34 s).
const TEMPO_EVERY: u32 = 32;
const MIN_BEAT_GAP_S: f32 = 0.22;
const BPM_RANGE: (f32, f32) = (60.0, 200.0);

pub(crate) struct State {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Scales |X|^2 so a full-scale sine sums to 1 over its bins.
    power_scale: f32,
    samples: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    power: Vec<f32>,
    rate: u32,
    bars: usize,
    /// Bar edges in fractional bins.
    edges: Vec<f32>,
    band_avg: [f32; 3],
    /// Seconds of sound each band's average has seen, capped once the average is settled.
    band_age: [f32; 3],
    band_att: [f32; 3],
    beats: Beats,
    last_hop: Option<u64>,
    out: Analysis,
}

impl State {
    pub fn new(bars: usize) -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / FFT_SIZE as f32).cos())
            .collect();
        let sum_sq: f32 = window.iter().map(|w| w * w).sum();
        let scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
        State {
            fft,
            power_scale: 4.0 / (FFT_SIZE as f32 * sum_sq),
            window,
            samples: vec![0.0; FFT_SIZE],
            spectrum: vec![Complex::default(); FFT_SIZE],
            scratch,
            power: vec![0.0; BINS],
            rate: 0,
            bars,
            edges: Vec::new(),
            band_avg: [0.0; 3],
            band_age: [0.0; 3],
            band_att: [0.0; 3],
            beats: Beats::new(),
            last_hop: None,
            out: Analysis {
                bars: vec![0.0; bars],
                waveform: vec![0.0; WAVEFORM_LEN],
                ..Default::default()
            },
        }
    }

    pub fn update(&mut self, tap: &Tap, dt: Duration) -> &Analysis {
        let dt = dt.as_secs_f32().clamp(0.0, 0.25);
        let rate = tap.sample_rate();
        if rate != self.rate {
            self.set_rate(rate);
        }
        let end = tap.position();

        self.out.beat = false;
        self.out.beat_strength = 0.0;
        self.track_beats(tap, end);

        tap.read_at(end, &mut self.samples);
        let start = FFT_SIZE - WAVEFORM_LEN;
        self.out.waveform.copy_from_slice(&self.samples[start..]);
        self.analyze_window();
        self.update_bars(dt);
        self.update_bands(dt);
        self.out.bpm = self.beats.bpm;
        &self.out
    }

    fn set_rate(&mut self, rate: u32) {
        self.rate = rate.max(1);
        let bin_hz = self.rate as f32 / FFT_SIZE as f32;
        let high = BAR_HIGH_HZ.min(self.rate as f32 / 2.0);
        let ratio = high / BAR_LOW_HZ;
        self.edges = (0..=self.bars)
            .map(|i| BAR_LOW_HZ * ratio.powf(i as f32 / self.bars.max(1) as f32) / bin_hz)
            .collect();
        self.beats.set_rate(self.rate);
        self.last_hop = None;
    }

    /// Windows `self.samples`, transforms them, and fills `self.power`.
    fn analyze_window(&mut self) {
        for ((c, s), w) in self.spectrum.iter_mut().zip(&self.samples).zip(&self.window) {
            *c = Complex::new(s * w, 0.0);
        }
        self.fft.process_with_scratch(&mut self.spectrum, &mut self.scratch);
        for (p, c) in self.power.iter_mut().zip(&self.spectrum) {
            *p = c.norm_sqr() * self.power_scale;
        }
    }

    /// Energy between two fractional bin positions, each bin covering +-0.5 around its center.
    fn band_energy(&self, lo: f32, hi: f32) -> f32 {
        let first = (lo + 0.5).floor().max(0.0) as usize;
        let last = ((hi + 0.5).ceil() as usize).min(BINS);
        (first..last)
            .map(|k| {
                let (a, b) = (k as f32 - 0.5, k as f32 + 0.5);
                let overlap = (b.min(hi) - a.max(lo)).max(0.0);
                self.power[k] * overlap
            })
            .sum()
    }

    fn update_bars(&mut self, dt: f32) {
        let rise = smoothing(dt, BAR_RISE_S);
        let fall = smoothing(dt, BAR_FALL_S);
        for i in 0..self.bars {
            let energy = self.band_energy(self.edges[i], self.edges[i + 1]);
            let db = 10.0 * (energy + 1e-12).log10();
            let target = ((db - BAR_FLOOR_DB) / (BAR_CEIL_DB - BAR_FLOOR_DB)).clamp(0.0, 1.0);
            let bar = &mut self.out.bars[i];
            *bar += (target - *bar) * if target > *bar { rise } else { fall };
        }
    }

    fn update_bands(&mut self, dt: f32) {
        let bin_hz = self.rate as f32 / FFT_SIZE as f32;
        let average = smoothing(dt, BAND_AVERAGE_S);
        let rise = smoothing(dt, ATT_RISE_S);
        let fall = smoothing(dt, ATT_FALL_S);
        let mut instant = [0.0f32; 3];
        for (i, (lo, hi)) in BANDS.iter().enumerate() {
            let hi = hi.min(self.rate as f32 / 2.0);
            let level = self.band_energy(lo / bin_hz, hi / bin_hz).sqrt();
            if level > SILENCE {
                // A plain running mean at first, so the average is right from the start, then
                // an exponential one that follows the music.
                let age = &mut self.band_age[i];
                *age = (*age + dt).min(BAND_AVERAGE_S);
                let weight = if *age > 0.0 { (dt / *age).max(average) } else { 1.0 };
                let avg = &mut self.band_avg[i];
                *avg += (level - *avg) * weight;
                instant[i] = level / avg.max(SILENCE);
            }
            let att = &mut self.band_att[i];
            *att += (instant[i] - *att) * if instant[i] > *att { rise } else { fall };
        }
        [self.out.bass, self.out.mid, self.out.treble] = instant;
        [self.out.bass_att, self.out.mid_att, self.out.treble_att] = self.band_att;
    }

    /// Runs onset detection for every hop that became audible since the last update.
    fn track_beats(&mut self, tap: &Tap, end: u64) {
        let Some(mut last) = self.last_hop else {
            self.last_hop = Some(end);
            return;
        };
        if end < last {
            self.last_hop = Some(end);
            return;
        }
        // After a long stall only the recent past matters.
        let max_hops = 16;
        if end - last > HOP * max_hops {
            last = end - HOP * max_hops;
        }
        while last + HOP <= end {
            last += HOP;
            tap.read_at(last, &mut self.samples);
            self.analyze_window();
            if let Some(strength) = self.beats.hop(&self.power) {
                self.out.beat = true;
                self.out.beat_strength = self.out.beat_strength.max(strength);
            }
        }
        self.last_hop = Some(last);
    }
}

/// Fraction of the way to move toward a target this frame, for time constant `tau`.
fn smoothing(dt: f32, tau: f32) -> f32 {
    1.0 - (-dt / tau).exp()
}

/// Spectral-flux onset detection with an adaptive threshold, plus a tempo estimate.
struct Beats {
    hop_s: f32,
    /// Bin range used for the flux.
    bins: (usize, usize),
    prev: Vec<f32>,
    flux: VecDeque<f32>,
    envelope: VecDeque<f32>,
    /// The last two flux values, for one-hop-delayed peak picking.
    recent: [f32; 2],
    since_beat: u32,
    since_tempo: u32,
    bpm: Option<f32>,
    candidate: Option<(f32, u32)>,
}

impl Beats {
    fn new() -> Self {
        Beats {
            hop_s: HOP as f32 / 48_000.0,
            bins: (1, BINS - 1),
            prev: vec![0.0; BINS],
            flux: VecDeque::with_capacity(FLUX_HISTORY),
            envelope: VecDeque::with_capacity(TEMPO_HISTORY),
            recent: [0.0; 2],
            since_beat: u32::MAX / 2,
            since_tempo: 0,
            bpm: None,
            candidate: None,
        }
    }

    fn set_rate(&mut self, rate: u32) {
        self.hop_s = HOP as f32 / rate as f32;
        let bin_hz = rate as f32 / FFT_SIZE as f32;
        let lo = ((30.0 / bin_hz) as usize).max(1);
        let hi = ((8_000.0 / bin_hz) as usize).min(BINS - 1);
        self.bins = (lo, hi.max(lo));
    }

    /// Feeds one hop's power spectrum. Returns a beat strength if the previous hop was a beat.
    fn hop(&mut self, power: &[f32]) -> Option<f32> {
        let (lo, hi) = self.bins;
        let mut flux = 0.0;
        for (p, prev) in power[lo..=hi].iter().zip(&mut self.prev[lo..=hi]) {
            let level = (1.0 + 1000.0 * p.sqrt()).ln();
            flux += (level - *prev).max(0.0);
            *prev = level;
        }
        flux /= (hi - lo + 1) as f32;

        let (mean, std) = mean_std(&self.flux);
        let threshold = (mean + 1.2 * std).max(0.02);
        if self.flux.len() == FLUX_HISTORY {
            self.flux.pop_front();
        }
        self.flux.push_back(flux);
        if self.envelope.len() == TEMPO_HISTORY {
            self.envelope.pop_front();
        }
        self.envelope.push_back((flux - mean).max(0.0));

        // The previous hop is a beat if it is a local peak above the threshold.
        let [before, candidate] = self.recent;
        self.recent = [candidate, flux];
        self.since_beat = self.since_beat.saturating_add(1);
        let min_gap = (MIN_BEAT_GAP_S / self.hop_s) as u32;
        let beat = candidate > threshold
            && candidate >= before
            && candidate > flux
            && self.since_beat > min_gap
            && self.flux.len() > FLUX_HISTORY / 4;

        self.since_tempo += 1;
        if self.since_tempo >= TEMPO_EVERY {
            self.since_tempo = 0;
            self.estimate_tempo();
        }

        if !beat {
            return None;
        }
        self.since_beat = 1;
        let ratio = candidate / threshold;
        Some((1.0 - 1.0 / (ratio * ratio)).clamp(0.1, 1.0))
    }

    fn estimate_tempo(&mut self) {
        let env: &[f32] = self.envelope.make_contiguous();
        let n = env.len();
        let min_lag = (60.0 / (BPM_RANGE.1 * self.hop_s)).floor() as usize;
        let max_lag = (60.0 / (BPM_RANGE.0 * self.hop_s)).ceil() as usize;
        if n < max_lag * 3 {
            return;
        }
        let energy: f32 = env.iter().map(|v| v * v).sum::<f32>() / n as f32;
        if energy <= 1e-9 {
            self.bpm = None;
            self.candidate = None;
            return;
        }
        let ac = |lag: usize| -> f32 {
            env[lag..].iter().zip(env).map(|(a, b)| a * b).sum::<f32>() / (n - lag) as f32
        };
        let values: Vec<f32> = (min_lag - 1..=max_lag + 1).map(ac).collect();
        let mut best = None;
        for (i, &v) in values.iter().enumerate().skip(1).take(values.len() - 2) {
            let lag = (min_lag - 1 + i) as f32;
            let bpm = 60.0 / (lag * self.hop_s);
            // Mild preference for common tempos, to settle octave ambiguity.
            let prior = (-0.5 * (bpm / 120.0).log2().powi(2)).exp();
            let score = v * prior;
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, i));
            }
        }
        let Some((_, i)) = best else {
            return;
        };
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        let peak = values[i];
        if peak < mean * 1.25 || peak < energy * 0.15 {
            self.candidate = None;
            return;
        }
        // Refine the peak between lags.
        let (a, b, c) = (values[i - 1], peak, values[i + 1]);
        let denom = a - 2.0 * b + c;
        let shift = if denom.abs() > 1e-12 { (0.5 * (a - c) / denom).clamp(-0.5, 0.5) } else { 0.0 };
        let lag = (min_lag - 1 + i) as f32 + shift;
        let bpm = 60.0 / (lag * self.hop_s);

        // Report only once consecutive estimates agree.
        let agrees = |other: f32| (bpm / other - 1.0).abs() < 0.04;
        self.candidate = match self.candidate {
            Some((prev, count)) if agrees(prev) => Some(((prev + bpm) / 2.0, count + 1)),
            _ => Some((bpm, 1)),
        };
        if let Some((value, count)) = self.candidate
            && count >= 3
        {
            self.bpm = Some(match self.bpm {
                Some(current) if agrees(current) => current + (value - current) * 0.3,
                _ => value,
            });
        }
    }
}

fn mean_std(values: &VecDeque<f32>) -> (f32, f32) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let n = values.len() as f32;
    let mean = values.iter().sum::<f32>() / n;
    let var = values.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / n;
    (mean, var.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::Shared;
    use crate::tap::TapBuffer;

    const RATE: u32 = 48_000;
    const FPS: f32 = 120.0;

    struct Feed {
        tap: Tap,
        pos: u64,
        analyzer: State,
    }

    impl Feed {
        fn new(bars: usize) -> Feed {
            let shared = Arc::new(Shared::new(RATE, false));
            let tap = Tap { buf: Arc::new(TapBuffer::new(shared)) };
            Feed { tap, pos: 0, analyzer: State::new(bars) }
        }

        /// Pushes one frame's worth of samples from `signal` (time in seconds) and updates.
        fn frame(&mut self, signal: &mut impl FnMut(f32) -> f32) -> Analysis {
            let n = (RATE as f32 / FPS) as u64;
            let first = self.pos;
            for i in first..first + n {
                self.tap.buf.store(i, signal(i as f32 / RATE as f32));
            }
            self.pos += n;
            self.tap.buf.commit(first, self.pos, 0, true);
            self.analyzer.update(&self.tap, Duration::from_secs_f32(1.0 / FPS)).clone()
        }

        fn run(&mut self, secs: f32, mut signal: impl FnMut(f32) -> f32) -> Vec<Analysis> {
            (0..(secs * FPS) as usize).map(|_| self.frame(&mut signal)).collect()
        }
    }

    fn sine(freq: f32, amp: f32) -> impl Fn(f32) -> f32 {
        move |t| amp * (2.0 * PI * freq * t).sin()
    }

    fn noise(seed: u64) -> impl FnMut(f32) -> f32 {
        let mut s = seed;
        move |_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    fn bar_center(bars: usize, i: usize) -> f32 {
        let ratio = BAR_HIGH_HZ / BAR_LOW_HZ;
        BAR_LOW_HZ * ratio.powf((i as f32 + 0.5) / bars as f32)
    }

    #[test]
    fn a_tone_lights_its_own_bar() {
        let mut feed = Feed::new(28);
        let last = feed.run(0.5, sine(1_000.0, 0.5)).pop().unwrap();
        let (peak, value) = last
            .bars
            .iter()
            .enumerate()
            .fold((0, 0.0), |best, (i, &v)| if v > best.1 { (i, v) } else { best });
        let center = bar_center(28, peak);
        assert!((center / 1_000.0).log2().abs() < 0.25, "peak bar at {center} Hz");
        assert!(value > 0.8, "peak bar only {value}");
        for (i, &v) in last.bars.iter().enumerate() {
            let octaves = (bar_center(28, i) / 1_000.0).log2().abs();
            if octaves > 1.5 {
                assert!(v < 0.15, "bar {i} at {} Hz is {v}", bar_center(28, i));
            }
        }
        assert!(last.bars.iter().all(|v| (0.0..=1.0).contains(v)));
    }

    #[test]
    fn bars_rise_fast_and_fall_slower() {
        let mut feed = Feed::new(28);
        let rise = feed.run(0.2, sine(1_000.0, 0.5));
        let top = rise.last().unwrap().bars.iter().cloned().fold(0.0, f32::max);
        let bar = rise.last().unwrap().bars.iter().position(|&v| v == top).unwrap();
        // Once the tone fills the 2048-sample window (~5 frames), the bar is almost up.
        assert!(rise[6].bars[bar] > 0.9 * top, "slow rise: {}", rise[6].bars[bar]);
        let fall = feed.run(1.0, |_| 0.0);
        let level: Vec<f32> = fall.iter().map(|a| a.bars[bar]).collect();
        assert!(level[6] > 0.5 * top, "fell at once: {}", level[6]);
        assert!(level.windows(2).all(|w| w[1] <= w[0] + 1e-6), "rose during silence: {level:?}");
        assert!(level.last().unwrap() < &0.01, "did not fall to zero");
    }

    #[test]
    fn bands_hover_around_one_whatever_the_loudness() {
        for amp in [0.5, 0.02] {
            let mut feed = Feed::new(28);
            let (bass, mid, treble) = (sine(80.0, amp), sine(1_000.0, amp), sine(8_000.0, amp));
            let mut noise = noise(7);
            let frames = feed.run(6.0, |t| bass(t) + mid(t) + treble(t) + 0.2 * amp * noise(t));
            let tail = &frames[frames.len() / 2..];
            let mean = |f: fn(&Analysis) -> f32| tail.iter().map(f).sum::<f32>() / tail.len() as f32;
            for (name, value) in [
                ("bass", mean(|a| a.bass)),
                ("mid", mean(|a| a.mid)),
                ("treble", mean(|a| a.treble)),
                ("bass_att", mean(|a| a.bass_att)),
                ("mid_att", mean(|a| a.mid_att)),
                ("treble_att", mean(|a| a.treble_att)),
            ] {
                assert!((0.8..1.25).contains(&value), "amp {amp}: {name} averages {value}");
            }
        }
    }

    /// Kick-like bursts at `bpm` over quiet noise.
    fn kicks(bpm: f32) -> impl FnMut(f32) -> f32 {
        let period = 60.0 / bpm;
        let mut noise = noise(3);
        move |t| {
            let since = t % period;
            let env = (-since / 0.06).exp();
            0.8 * env * (2.0 * PI * 60.0 * since).sin() + 0.3 * env * noise(t) + 0.01 * noise(t)
        }
    }

    #[test]
    fn beats_and_tempo_follow_a_steady_kick() {
        for bpm in [120.0, 95.0, 140.0] {
            let mut feed = Feed::new(28);
            let frames = feed.run(14.0, kicks(bpm));
            let tail = &frames[frames.len() - (8.0 * FPS) as usize..];
            let beats = tail.iter().filter(|a| a.beat).count() as f32;
            let expected = 8.0 * bpm / 60.0;
            assert!((beats - expected).abs() <= 2.0, "{bpm} BPM: {beats} beats in 8 s");
            assert!(tail.iter().filter(|a| a.beat).all(|a| a.beat_strength > 0.0));
            assert!(tail.iter().filter(|a| !a.beat).all(|a| a.beat_strength == 0.0));
            let estimate = frames.last().unwrap().bpm.expect("tempo found");
            assert!((estimate - bpm).abs() < 3.0, "{bpm} BPM estimated as {estimate}");
        }
    }

    #[test]
    fn silence_is_calm() {
        let mut feed = Feed::new(28);
        for a in feed.run(3.0, |_| 0.0) {
            assert!(!a.beat && a.bpm.is_none());
            assert_eq!([a.bass, a.mid, a.treble], [0.0; 3]);
            assert!(a.bars.iter().all(|&v| v == 0.0));
        }
    }

    #[test]
    fn waveform_is_the_latest_samples() {
        let mut feed = Feed::new(8);
        let last = feed.run(0.1, |t| t).pop().unwrap();
        let end = feed.pos;
        assert_eq!(last.waveform.len(), WAVEFORM_LEN);
        for (i, &v) in last.waveform.iter().enumerate() {
            let t = (end - WAVEFORM_LEN as u64 + i as u64) as f32 / RATE as f32;
            assert!((v - t).abs() < 1e-6);
        }
    }
}
