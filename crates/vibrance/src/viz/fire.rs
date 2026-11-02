//! The campfire's music-driven state: its flicker clock, the salts on the fire, and when
//! sparks fly. The fire itself is simulated and drawn on the GPU (`campfire`).

use vibrance_audio::Analysis;

use super::campfire::{self, spectrum};

/// Pairs of salts thrown on the fire together (the music version changes pair every eight
/// bars). Natural fire uses none.
const SALTS: &[(&[f64], &[f64])] = &[
    (spectrum::COPPER_CHLORIDE, spectrum::COPPER),
    (spectrum::BORON, spectrum::BARIUM),
    (spectrum::CESIUM, spectrum::POTASSIUM),
    (spectrum::LITHIUM, spectrum::STRONTIUM),
    (spectrum::COPPER, spectrum::LITHIUM),
    (spectrum::INDIUM, spectrum::COPPER),
];

/// Seconds a change of salts takes to show.
const RECOLOR: f32 = 2.5;
/// Ring size of the GPU's spark buffer (sim.wgsl's `SPARKS`).
const SPARKS: u32 = 4096;

pub struct Fire {
    clock: f32,
    /// Flicker speed relative to a real fire.
    rate: f32,
    loud: f32,
    bands: [f32; 8],
    salt: usize,
    from: usize,
    recolor: f32,
    spawn: f32,
    spark_cursor: u32,
    time: f32,
    frame: u32,
    rng: u64,
}

/// What the fire needs from the rest of the engine each frame.
pub struct Music<'a> {
    pub analysis: Option<&'a Analysis>,
    pub bpm: f32,
    pub bass: f32,
    pub kick: f32,
    /// Strength of a beat that landed this frame.
    pub beat: Option<f32>,
    /// 1 while playing, fading to 0 after a pause.
    pub energy: f32,
}

impl Fire {
    pub fn new(seed: u64) -> Fire {
        let mut fire = Fire {
            clock: 0.0,
            rate: 1.0,
            loud: 0.0,
            bands: [0.0; 8],
            salt: 0,
            from: 0,
            recolor: 1.0,
            spawn: 0.0,
            spark_cursor: 0,
            time: 0.0,
            frame: 0,
            rng: seed | 1,
        };
        fire.salt = (fire.random() * SALTS.len() as f32) as usize % SALTS.len();
        fire.from = fire.salt;
        fire
    }

    fn random(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Throw different salts on the fire.
    pub fn recolor(&mut self) {
        self.from = self.salt;
        self.salt = (self.salt + 1 + (self.random() * (SALTS.len() - 1) as f32) as usize) % SALTS.len();
        self.recolor = 0.0;
    }

    /// Advance by a displayed frame. `amount` is how much of the fire shows (0 when another
    /// preset is up), `tempo` picks the flicker speed (0 a real fire's, 1 the music's), and
    /// `salts` how strongly salts colour the flames.
    pub fn update(&mut self, dt: f32, amount: f32, tempo: f32, salts: f32, music: &Music) -> campfire::Params {
        let smooth = |v: &mut f32, target: f32, attack: f32, release: f32| {
            let tau = if target > *v { attack } else { release };
            *v += (target - *v) * (1.0 - (-dt / tau).exp());
        };
        let loud = music.analysis.map_or(0.0, |a| ((a.bass + a.mid + a.treble) / 3.0).min(2.0));
        smooth(&mut self.loud, loud, 0.08, 0.6);
        if let Some(a) = music.analysis.filter(|a| !a.bars.is_empty()) {
            let n = a.bars.len();
            for (i, band) in self.bands.iter_mut().enumerate() {
                let group = &a.bars[i * n / 8..((i + 1) * n / 8).max(i * n / 8 + 1)];
                smooth(band, group.iter().sum::<f32>() / group.len() as f32, 0.06, 0.35);
            }
        } else {
            self.bands.iter_mut().for_each(|b| *b *= (-dt / 0.5).exp());
        }
        self.recolor = (self.recolor + dt / RECOLOR).min(1.0);
        self.time += dt;
        self.frame = self.frame.wrapping_add(1);

        // A real campfire puffs about twice a second whatever plays (that comes out of the
        // simulation by itself); the music version runs it slow for quiet, slow songs and
        // fast for loud, fast ones, surging on kicks.
        let lift = self.loud.min(1.5);
        let music_rate = (music.bpm / 120.0) * (0.35 + 0.65 * lift) + 1.0 * music.kick;
        self.rate = 1.0 + (music_rate - 1.0) * tempo;
        self.clock += dt * self.rate;

        // Sparks: a sparse trickle that grows with loudness, and a few more on strong beats.
        let mut count = 0;
        if amount > 0.001 {
            self.spawn += dt * (1.0 + 4.0 * lift) * amount * music.energy;
            count = self.spawn as u32;
            self.spawn -= count as f32;
            if let Some(strength) = music.beat.filter(|s| *s > 0.55) {
                count += (2.0 + 14.0 * (strength - 0.55)) as u32;
            }
        }
        let first = self.spark_cursor;
        self.spark_cursor = (self.spark_cursor + count) % SPARKS;

        let k = 0.5 - 0.5 * (std::f32::consts::PI * self.recolor).cos();
        let (a, b) = (SALTS[self.from], SALTS[self.salt]);
        let blend = |x: &[f64], y: &[f64]| {
            let (x, y) = (spectrum::lines(x), spectrum::lines(y));
            [0, 1, 2].map(|i| x[i] + (y[i] - x[i]) * k)
        };
        campfire::Params {
            clock: self.clock,
            sim_dt: super::STEP * self.rate.clamp(0.2, 2.5),
            frame_dt: dt,
            time: self.time,
            fuel: (3.0 + 1.2 * lift + 2.5 * music.kick) * music.energy,
            kick: music.kick,
            bands: self.bands,
            salts,
            salt_a: blend(a.0, b.0),
            salt_b: blend(a.1, b.1),
            salt_drift: self.clock * 0.05,
            coals: (0.55 + 0.3 * music.bass.min(1.5)) * music.energy.max(0.15),
            spark_first: first,
            spark_count: count,
            frame: self.frame,
        }
    }

    #[cfg(test)]
    pub fn rate(&self) -> f32 {
        self.rate
    }
}
