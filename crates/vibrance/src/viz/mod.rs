//! MilkDrop-style feedback visualizer (SPEC 4.4).
//!
//! The engine runs on the UI thread once per displayed frame: it follows the music (bands,
//! beats, tempo), drifts and blends presets, and produces uniforms plus shape vertices. The
//! GPU side (`gpu`) warps the previous frame, adds the shapes, and composites to the screen.
//!
//! The feedback loop advances in fixed 60 Hz steps (MilkDrop's own rate), so motion looks the
//! same at any refresh rate; the composite still runs every displayed frame. Motion is
//! defined per beat of the song's tempo, never per frame.
//!
//! The warp equations follow MilkDrop 2 (BSD, © Nullsoft) as ported by Butterchurn (MIT,
//! © Jordan Berg); see `viz.wgsl`. Each displayed frame then views the feedback in 3D and
//! lights it (SPEC 4.4).

pub mod campfire;
mod fire;
pub mod gpu;
mod presets;

use std::f32::consts::TAU;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use vibrance_audio::Analysis;

use fire::{Fire, Music};

pub use presets::{PRESETS, Preset};

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Pod, Zeroable)]
pub struct Uniforms {
    pub frame: [f32; 4],
    pub motion: [f32; 4],
    pub warp: [f32; 4],
    pub sym: [f32; 4],
    pub color: [f32; 4],
    pub comp: [f32; 4],
    pub center: [f32; 4],
    pub shape: [f32; 4],
    pub fx: [f32; 4],
    pub gamma: [f32; 4],
    pub safety: [f32; 4],
    pub pal_a: [f32; 4],
    pub pal_b: [f32; 4],
    pub pal_c: [f32; 4],
    pub pal_d: [f32; 4],
    pub depth: [f32; 4],
    pub view: [f32; 4],
    pub light: [f32; 4],
    /// How much of the campfire shows.
    pub fire: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 2],
    pub color: [f32; 4],
    /// Position across the shape (-1..1) and softness: 0 for crisp lines, 1 for round glows.
    pub edge: [f32; 3],
}

/// One displayed frame for the GPU: how many 60 Hz feedback steps to run, and with what.
#[derive(Debug, Default)]
pub struct Frame {
    pub id: u64,
    pub steps: u32,
    pub uniforms: Uniforms,
    pub vertices: Vec<Vertex>,
    pub campfire: campfire::Params,
}

const STEP: f32 = 1.0 / 60.0;
/// Seconds of motion kept after pausing, so trails fade instead of freezing.
const FADE_OUT: f32 = 2.0;
/// Overall pace on top of the tempo.
const PACE: f32 = 0.8;
/// Largest luminance change per second the flash limiter allows a screen tile (WCAG 2.3.1).
const LUMA_SLEW: f32 = 0.6;

pub struct Engine {
    current: usize,
    from: Option<Preset>,
    mix: f32,
    blend_beats: f32,
    pub locked: bool,
    /// Limit full-screen brightness changes (photosensitivity), on by default.
    pub reduce_flashing: bool,
    time: f32,
    tick: f32,
    /// Beats since start, phase-locked to detected onsets.
    phase: f32,
    bars_left: u32,
    bpm: f32,
    bass: f32,
    mid: f32,
    treble: f32,
    kick: f32,
    flash: f32,
    warp_clock: f32,
    hue: f32,
    rng: u64,
    seed: f32,
    idle: f32,
    frame: u64,
    waveform: Vec<f32>,
    /// When the preset last changed, for the name toast.
    changed_at: f32,
    /// Distance flown down the 3D tunnel and corridor, wrapped to the texture's period.
    travel: f32,
    bars: u32,
    fire: Fire,
}

impl Default for Engine {
    fn default() -> Engine {
        let rng = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0x2545_F491, |d| d.as_nanos() as u64) | 1;
        let mut e = Engine {
            current: 0,
            from: None,
            mix: 1.0,
            blend_beats: 8.0,
            locked: false,
            reduce_flashing: true,
            time: 0.0,
            tick: 0.0,
            phase: 0.0,
            bars_left: 16,
            bpm: 120.0,
            bass: 0.0,
            mid: 0.0,
            treble: 0.0,
            kick: 0.0,
            flash: 0.0,
            warp_clock: 0.0,
            hue: 0.0,
            rng,
            seed: 0.0,
            idle: FADE_OUT,
            frame: 0,
            waveform: vec![0.0; 512],
            changed_at: -100.0,
            travel: 0.0,
            bars: 0,
            fire: Fire::new(rng.rotate_left(17)),
        };
        e.seed = e.random() * 100.0;
        e.current = (e.random() * PRESETS.len() as f32) as usize % PRESETS.len();
        // Hue drifts slowly (a cycle takes about a minute), so start each session somewhere new.
        e.hue = e.random() * TAU;
        e
    }
}

impl Engine {
    pub fn new(reduce_flashing: bool) -> Engine {
        Engine { reduce_flashing, ..Engine::default() }
    }

    pub fn preset_name(&self) -> &'static str {
        PRESETS[self.current].name
    }

    /// Seconds since the preset changed, for fading its name in and out.
    pub fn since_change(&self) -> f32 {
        self.time - self.changed_at
    }

    /// Still moving (playing, or trails fading after a pause).
    pub fn active(&self) -> bool {
        self.idle < FADE_OUT
    }

    fn random(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Index of the preset showing (or being morphed to).
    pub fn index(&self) -> usize {
        self.current
    }

    /// Jump straight to a preset by name (tests and screenshots).
    pub fn select(&mut self, name: &str) {
        if let Some(i) = PRESETS.iter().position(|p| p.name.eq_ignore_ascii_case(name)) {
            self.current = i;
            self.from = None;
            self.mix = 1.0;
            self.changed_at = self.time;
        }
    }

    /// Morph to preset `index` over `beats`; automatic changes then wait 12–24 bars again.
    fn switch(&mut self, index: usize, beats: f32) {
        self.from = Some(self.blended());
        self.current = index % PRESETS.len();
        self.mix = 0.0;
        self.blend_beats = beats;
        self.changed_at = self.time;
        self.bars_left = 12 + (self.random() * 12.0) as u32;
    }

    /// Automatic change: a random other preset, morphing over two bars.
    fn shuffle(&mut self) {
        let mut next = self.current;
        while next == self.current && PRESETS.len() > 1 {
            next = (self.random() * PRESETS.len() as f32) as usize % PRESETS.len();
        }
        self.switch(next, 8.0);
    }

    /// The next preset in the list (a click, N): quick, and keeps the lock as it is.
    pub fn next_preset(&mut self) {
        self.switch(self.current + 1, 2.0);
    }

    /// Show one preset and stay on it (the preset menu).
    pub fn pick(&mut self, index: usize) {
        if index != self.current {
            self.switch(index, 2.0);
        }
        self.locked = true;
    }

    fn blended(&self) -> Preset {
        let to = PRESETS[self.current];
        match &self.from {
            Some(from) if self.mix < 1.0 => from.lerp(&to, 0.5 - 0.5 * (std::f32::consts::PI * self.mix).cos()),
            _ => to,
        }
    }

    /// Advance by `dt` seconds of display time. `analysis` is `None` while paused or stopped.
    pub fn update(&mut self, dt: f32, analysis: Option<&Analysis>, aspect: f32) -> Arc<Frame> {
        let dt = dt.clamp(0.0, 0.1);
        self.frame += 1;
        self.time += dt;
        if analysis.is_some() {
            self.idle = 0.0;
        } else {
            self.idle += dt;
        }
        let energy = if analysis.is_some() { 1.0 } else { (1.0 - self.idle / FADE_OUT).max(0.0) };

        // Bands: fast attack, slow release, so motion follows the music without jitter.
        let (bass, mid, treble) = analysis.map_or((0.0, 0.0, 0.0), |a| (a.bass.min(3.0), a.mid.min(3.0), a.treble.min(3.0)));
        let smooth = |v: &mut f32, target: f32| {
            let tau = if target > *v { 0.03 } else { 0.4 };
            *v += (target - *v) * (1.0 - (-dt / tau).exp());
        };
        smooth(&mut self.bass, bass);
        smooth(&mut self.mid, mid);
        smooth(&mut self.treble, treble);

        // Tempo: fold octaves into 75–150 BPM, then glide toward it.
        if let Some(mut b) = analysis.and_then(|a| a.bpm).filter(|b| *b > 1.0) {
            while b > 150.0 {
                b /= 2.0;
            }
            while b < 75.0 {
                b *= 2.0;
            }
            self.bpm += (b - self.bpm) * (1.0 - (-dt / 2.0).exp());
        }
        let beats_per_sec = self.bpm / 60.0;

        // Beat clock, phase-locked to onsets; preset changes land on bar boundaries.
        let before = self.phase;
        self.phase += dt * beats_per_sec * energy;
        if let Some(a) = analysis.filter(|a| a.beat) {
            let err = self.phase.round() - self.phase;
            if err.abs() < 0.2 {
                self.phase += 0.15 * err;
            }
            self.kick = self.kick.max(a.beat_strength.clamp(0.3, 1.0));
            self.flash = self.flash.max(a.beat_strength.clamp(0.0, 1.0));
        }
        let beat = analysis.filter(|a| a.beat).map(|a| a.beat_strength.clamp(0.0, 1.0));
        if (self.phase / 4.0).floor() > (before / 4.0).floor() {
            self.bars_left = self.bars_left.saturating_sub(1);
            if self.bars_left == 0 && !self.locked {
                self.shuffle();
            }
            // The fire takes on a new colour every eight bars.
            self.bars += 1;
            if self.bars.is_multiple_of(8) {
                self.fire.recolor();
            }
        }
        self.kick *= (-dt / 0.18).exp();
        self.flash *= (-dt / 0.1).exp();
        if self.mix < 1.0 {
            self.mix = (self.mix + dt * beats_per_sec / self.blend_beats).min(1.0);
        }

        // Fixed 60 Hz feedback steps.
        self.tick += dt;
        let steps = ((self.tick / STEP).floor() as u32).min(4);
        self.tick = if steps == 4 { 0.0 } else { self.tick - steps as f32 * STEP };

        let p = self.blended();
        let drift = |k: f32| 0.6 * (self.time * 0.29 + self.seed + k).sin() + 0.4 * (self.time * 0.47 + self.seed * 1.7 + k * 2.1).sin();

        // Per-step values. Audio speeds clocks up and down; it never jumps positions.
        let beats = STEP * beats_per_sec * PACE * energy;
        let bar_wave = (TAU * self.phase / 4.0).sin();
        let zoom_log = p.zoom.ln() * beats
            + p.wobble * bar_wave * beats
            + (p.bass_zoom * (self.bass - 1.0).max(-0.5) + p.kick * self.kick) * STEP * energy;
        let rot = (p.rot + p.mid_rot * (self.mid - 1.0)) * beats + 0.03 * drift(2.0) * STEP * energy * p.wander.min(1.0);
        self.warp_clock += dt * p.warp_speed * (beats_per_sec / 2.0) * (0.6 + 0.4 * self.bass.min(2.5)) * energy;
        let warp = p.warp * (0.7 + p.treble_warp * (self.treble - 0.7).max(0.0)) * energy;
        let hue_step = p.hue * beats * TAU * (0.8 + 0.2 * self.mid.min(2.0));
        self.hue += hue_step * steps as f32;
        let decay = 0.5f32.powf(STEP / p.half_life.max(0.05));
        // Flight speed follows the tempo and surges on kicks. The textures repeat (mirrored)
        // every 2, so wrapping there keeps precision over long sessions.
        self.travel = (self.travel + dt * p.fly * beats_per_sec * PACE * energy * (1.0 + 1.5 * self.kick)).rem_euclid(2.0);

        let mut u = Uniforms {
            frame: [0.0, 0.0, self.time, aspect],
            motion: [zoom_log.exp(), rot, p.swirl * beats, decay],
            warp: [warp, p.warp_scale, self.warp_clock, p.blur],
            sym: [p.folds.round(), p.mirror.round(), p.drift * drift(5.0) * STEP, p.drift * drift(6.0) * STEP - p.fall * STEP],
            color: [hue_step, self.flash.min(1.0) * energy * if self.reduce_flashing { 0.6 } else { 1.0 }, p.plasma * energy, p.palette],
            comp: [p.echo, p.echo_zoom.max(0.2), p.chroma * (1.0 + 0.5 * self.kick), p.echo_orient.round()],
            center: [0.05 * drift(7.0) * p.wander, 0.04 * drift(8.0) * p.wander, self.bass, self.kick],
            shape: [p.zoomexp, p.vortex * beats, p.saddle * bar_wave, p.bias],
            fx: [p.sharpen, p.chroma_warp, p.wrap.round(), p.darken_center],
            gamma: [p.gamma[0], p.gamma[1], p.gamma[2], LUMA_SLEW * dt],
            safety: [if self.reduce_flashing { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
            depth: [p.tunnel, p.corridor, self.travel, p.fog],
            // The vanishing point wanders a little, so the flight seems to steer.
            view: [p.twist, self.time * 0.05 * TAU + self.seed, 0.06 * drift(9.0) * p.wander, 0.04 * drift(10.0) * p.wander],
            light: [p.relief, p.gloss, p.bloom, p.lens],
            ..Default::default()
        };
        let pal = presets::PALETTES[p.palette_index.round() as usize % presets::PALETTES.len()];
        u.pal_a = [pal[0][0], pal[0][1], pal[0][2], 0.0];
        u.pal_b = [pal[1][0], pal[1][1], pal[1][2], 0.0];
        u.pal_c = [pal[2][0], pal[2][1], pal[2][2], 0.0];
        u.pal_d = [pal[3][0], pal[3][1], pal[3][2], 0.0];

        let music = Music { analysis, bpm: self.bpm, bass: self.bass, kick: self.kick, beat, energy };
        let campfire = self.fire.update(dt, p.fire, p.fire_tempo, p.salts, &music);
        u.fire = [p.fire, 0.0, 0.0, 0.0];

        if let Some(a) = analysis.filter(|a| !a.waveform.is_empty()) {
            let n = self.waveform.len();
            for (i, w) in self.waveform.iter_mut().enumerate() {
                let src = a.waveform[i * a.waveform.len() / n];
                *w += (src - *w) * 0.5;
            }
        } else {
            self.waveform.iter_mut().for_each(|w| *w *= (-dt * 6.0).exp());
        }

        // Shapes are drawn into every feedback step, so their light is per 60 Hz step.
        let mut vertices = Vec::with_capacity(4096);
        if steps > 0 && energy > 0.0 {
            self.draw_wave(&p, aspect, energy, &mut vertices);
            self.draw_shape(&p, energy, &mut vertices);
            self.draw_border(&p, aspect, energy, &mut vertices);
            self.draw_sparks(&p, aspect, energy, &mut vertices);
        }

        Arc::new(Frame { id: self.frame, steps, uniforms: u, vertices, campfire })
    }

    fn color(&self, offset: f32, saturation: f32, alpha: f32) -> [f32; 4] {
        let h = (self.hue / TAU + offset).rem_euclid(1.0);
        let [r, g, b] = hsv(h, saturation, 1.0);
        [r, g, b, alpha]
    }

    fn draw_wave(&self, p: &Preset, aspect: f32, energy: f32, out: &mut Vec<Vertex>) {
        if p.wave <= 0.01 {
            return;
        }
        let n = 160;
        let mut pts = Vec::with_capacity(n + 1);
        for i in 0..=n {
            let f = (i % n) as f32 / n as f32;
            let w = self.waveform[(f * (self.waveform.len() - 1) as f32) as usize] * p.wave_scale;
            let r = 0.17 + 0.04 * self.bass.min(2.0) + 0.1 * w;
            let a = f * TAU + self.time * 0.2;
            let ring = (a.cos() * r, a.sin() * r);
            let lf = (i.min(n - 1)) as f32 / (n - 1) as f32;
            let line = ((lf - 0.5) * aspect * 0.92, p.wave_y + 0.22 * w);
            let m = p.wave_mode.clamp(0.0, 1.0);
            pts.push((ring.0 + (line.0 - ring.0) * m, ring.1 + (line.1 - ring.1) * m, f));
        }
        let thick = 0.0025 + 0.002 * self.bass.min(2.0);
        let alpha = p.wave * (0.4 + 0.3 * self.bass.min(1.5) / 1.5) * energy;
        // A soft neon halo under a crisp core.
        strip(&pts, thick * 4.0, 1.0, |f| self.color(p.hue_offset + f * 0.3, 0.85, alpha * 0.3), out);
        strip(&pts, thick, 0.0, |f| self.color(p.hue_offset + f * 0.3, 0.85, alpha), out);
    }

    fn draw_shape(&self, p: &Preset, energy: f32, out: &mut Vec<Vertex>) {
        if p.shape <= 0.01 {
            return;
        }
        let sides = p.sides.round().max(3.0) as usize;
        let r = 0.05 + 0.18 * self.kick + 0.03 * self.bass.min(2.0);
        let spin = self.time * (0.15 + p.mid_rot * 2.0) + self.phase * 0.2;
        let orbit = TAU * self.phase / 8.0;
        let (ox, oy) = (orbit.cos() * p.orbit, orbit.sin() * p.orbit);
        let pts: Vec<(f32, f32, f32)> = (0..=sides)
            .map(|i| {
                let a = spin + i as f32 / sides as f32 * TAU;
                (ox + a.cos() * r, oy + a.sin() * r, i as f32 / sides as f32)
            })
            .collect();
        let alpha = p.shape * (0.25 + 0.45 * self.kick) * energy;
        strip(&pts, 0.016, 1.0, |_| self.color(p.hue_offset + 0.5, 0.9, alpha * 0.3), out);
        strip(&pts, 0.004, 0.0, |_| self.color(p.hue_offset + 0.5, 0.9, alpha), out);
    }

    fn draw_border(&self, p: &Preset, aspect: f32, energy: f32, out: &mut Vec<Vertex>) {
        if p.border <= 0.0 {
            return;
        }
        let (w, h, t) = (aspect * 0.5, 0.5, p.border);
        let color = self.color(p.hue_offset + 0.15, 0.8, 0.5 * energy);
        for (x0, y0, x1, y1) in [(-w, h - t, w, h), (-w, -h, w, -h + t), (-w, -h, -w + t, h), (w - t, -h, w, h)] {
            let v = |x: f32, y: f32| Vertex { pos: [x, y], color, edge: [0.0; 3] };
            out.extend_from_slice(&[v(x0, y0), v(x1, y0), v(x0, y1), v(x1, y0), v(x1, y1), v(x0, y1)]);
        }
    }

    fn draw_sparks(&mut self, p: &Preset, aspect: f32, energy: f32, out: &mut Vec<Vertex>) {
        if p.sparks <= 0.01 {
            return;
        }
        let alpha = p.sparks * (self.treble - 0.7).clamp(0.0, 1.0) * 0.7 * energy;
        if alpha <= 0.01 {
            return;
        }
        let size = 0.004 + 0.004 * self.treble.min(2.0);
        let color = self.color(p.hue_offset + 0.25, 0.55, alpha);
        for _ in 0..36 {
            let a = self.random() * TAU;
            let rr = 0.08 + self.random() * 0.42;
            let (x, y) = (a.cos() * rr * aspect * 0.55, a.sin() * rr);
            let c = |dx: f32, dy: f32| Vertex { pos: [x + dx * size, y + dy * size], color, edge: [dx, dy, 1.0] };
            out.extend_from_slice(&[c(-1.0, -1.0), c(1.0, -1.0), c(-1.0, 1.0), c(1.0, -1.0), c(1.0, 1.0), c(-1.0, 1.0)]);
        }
    }
}

/// A thick line through aspect-space points as triangles; `soft` 0 is crisp, 1 a glow.
fn strip(pts: &[(f32, f32, f32)], thick: f32, soft: f32, color: impl Fn(f32) -> [f32; 4], out: &mut Vec<Vertex>) {
    let edge: Vec<([f32; 2], [f32; 2], [f32; 4])> = (0..pts.len())
        .map(|i| {
            let a = pts[i.saturating_sub(1)];
            let b = pts[(i + 1).min(pts.len() - 1)];
            let (mut tx, mut ty) = (b.0 - a.0, b.1 - a.1);
            let len = (tx * tx + ty * ty).sqrt().max(1e-6);
            tx /= len;
            ty /= len;
            let p = pts[i];
            ([p.0 - ty * thick, p.1 + tx * thick], [p.0 + ty * thick, p.1 - tx * thick], color(p.2))
        })
        .collect();
    // Each side of the line is an edge, so the shader can anti-alias it.
    let (left, right) = ([0.0, -1.0, soft], [0.0, 1.0, soft]);
    for w in edge.windows(2) {
        let (a0, b0, c0) = w[0];
        let (a1, b1, c1) = w[1];
        out.extend_from_slice(&[
            Vertex { pos: a0, color: c0, edge: left },
            Vertex { pos: b0, color: c0, edge: right },
            Vertex { pos: a1, color: c1, edge: left },
            Vertex { pos: b0, color: c0, edge: right },
            Vertex { pos: b1, color: c1, edge: right },
            Vertex { pos: a1, color: c1, edge: left },
        ]);
    }
}

fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let f = |n: f32| {
        let k = (n + h * 6.0) % 6.0;
        v - v * s * k.min(4.0 - k).clamp(0.0, 1.0)
    };
    [f(5.0), f(3.0), f(1.0)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn music() -> Analysis {
        Analysis { bass: 1.0, mid: 1.0, treble: 1.0, bass_att: 1.0, mid_att: 1.0, treble_att: 1.0, bpm: Some(120.0), ..Analysis::default() }
    }

    #[test]
    fn steps_follow_a_fixed_60hz_tick() {
        let mut e = Engine::default();
        let a = music();
        let at_120: u32 = (0..120).map(|_| e.update(1.0 / 120.0, Some(&a), 1.6).steps).sum();
        assert!((59..=61).contains(&at_120), "{at_120}");
        let at_60: u32 = (0..60).map(|_| e.update(1.0 / 60.0, Some(&a), 1.6).steps).sum();
        assert!((59..=61).contains(&at_60), "{at_60}");
    }

    #[test]
    fn calm_default_pace() {
        // At 120 BPM a typical preset zooms well under 2× per second (the prototype's pace).
        let mut e = Engine::default();
        e.select("Liquid Prism");
        let a = music();
        let mut zoom = 1.0f32;
        for _ in 0..60 {
            let f = e.update(1.0 / 60.0, Some(&a), 1.6);
            zoom *= f.uniforms.motion[0].powi(f.steps as i32);
        }
        assert!(zoom < 1.2, "zoomed {zoom}× in a second");
    }

    #[test]
    fn next_goes_in_list_order_and_pick_locks() {
        let mut e = Engine::default();
        e.select(PRESETS[0].name);
        e.next_preset();
        assert_eq!(e.index(), 1);
        assert!(!e.locked, "stepping keeps the automatic changes going");
        e.select(PRESETS[PRESETS.len() - 1].name);
        e.next_preset();
        assert_eq!(e.index(), 0, "wraps around");
        e.pick(3);
        assert_eq!(e.index(), 3);
        assert!(e.locked);
    }

    #[test]
    fn natural_fire_ignores_the_tempo_and_music_fire_follows_it() {
        let rate = |name: &str, bpm: f32| {
            let mut e = Engine::default();
            e.select(name);
            let a = Analysis { bpm: Some(bpm), ..music() };
            (0..240).for_each(|_| drop(e.update(1.0 / 60.0, Some(&a), 1.6)));
            e.fire.rate()
        };
        assert!((rate("Campfire · Natural", 80.0) - 1.0).abs() < 1e-3);
        assert!((rate("Campfire · Natural", 140.0) - 1.0).abs() < 1e-3);
        assert!(rate("Campfire · Music", 140.0) > rate("Campfire · Music", 80.0) * 1.3);
    }

    #[test]
    fn stops_after_pause() {
        let mut e = Engine::default();
        let a = music();
        e.update(1.0 / 60.0, Some(&a), 1.6);
        for _ in 0..300 {
            e.update(1.0 / 60.0, None, 1.6);
        }
        assert!(!e.active());
    }
}
