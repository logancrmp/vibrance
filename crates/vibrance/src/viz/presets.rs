//! Built-in presets. Motion is per beat at the song's tempo; decay is a half-life in seconds.
//!
//! Several recreate techniques from well-loved MilkDrop presets (Geiss, Rovastar, Flexi,
//! martin, Aderrasi): the parameters are ours, no preset files are copied.

#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub name: &'static str,
    /// Zoom factor per beat (>1 flows outward).
    pub zoom: f32,
    /// MilkDrop zoomexp: >1 makes the rim stream faster than the centre.
    pub zoomexp: f32,
    /// Rotation per beat, radians.
    pub rot: f32,
    /// MilkDrop warp amount (0.3–1.5 typical).
    pub warp: f32,
    /// MilkDrop warpscale: smaller means finer ripples.
    pub warp_scale: f32,
    /// Warp animation speed relative to the tempo.
    pub warp_speed: f32,
    /// Twist that fades toward the rim, radians per beat.
    pub swirl: f32,
    /// Hurricane-style rotation ∝ 1/(radius + 0.2), radians per beat.
    pub vortex: f32,
    /// Saddle field strength per step; its sign follows the bar.
    pub saddle: f32,
    /// Seconds for the feedback to fall to half.
    pub half_life: f32,
    /// Subtracted per 60 Hz step (MilkDrop 2's `ret -= 0.004`).
    pub bias: f32,
    /// Hue rotation per beat, turns.
    pub hue: f32,
    pub folds: f32,
    pub mirror: f32,
    /// Fold the picture back in at the edges instead of fading to black.
    pub wrap: f32,
    pub drift: f32,
    /// Vertical flow per second.
    pub fall: f32,
    pub blur: f32,
    /// Unsharp mask strength; grows reaction-diffusion patterns.
    pub sharpen: f32,
    /// Extra zoom per second at double the average bass.
    pub bass_zoom: f32,
    pub mid_rot: f32,
    pub treble_warp: f32,
    /// Zoom burst per second on a beat.
    pub kick: f32,
    /// Zoom breathing over each bar (log-zoom per beat).
    pub wobble: f32,
    pub wave: f32,
    /// 0 ring, 1 horizontal line.
    pub wave_mode: f32,
    pub wave_scale: f32,
    pub wave_y: f32,
    pub shape: f32,
    pub sides: f32,
    /// Radius the shape orbits at.
    pub orbit: f32,
    pub sparks: f32,
    /// Hue-cycling border drawn into the feedback each step (MilkDrop's ob).
    pub border: f32,
    pub hue_offset: f32,
    pub echo: f32,
    pub echo_zoom: f32,
    /// 0 none, 1 flip x, 2 flip y, 3 both.
    pub echo_orient: f32,
    pub chroma: f32,
    pub chroma_warp: f32,
    pub gamma: [f32; 3],
    /// Plasma injected into the feedback per 60 Hz step.
    pub plasma: f32,
    /// Map brightness through a cosine palette (0 keeps raw hues).
    pub palette: f32,
    pub palette_index: f32,
    /// How far the zoom centre wanders.
    pub wander: f32,
    pub darken_center: f32,
    /// How much of the view is a tunnel lined with the picture (0 flat).
    pub tunnel: f32,
    /// How much is a corridor: floor and ceiling lined with it.
    pub corridor: f32,
    /// Flight speed through the tunnel or corridor, per beat.
    pub fly: f32,
    /// Tunnel twist, radians per unit of depth.
    pub twist: f32,
    /// How fast the distance fades into haze.
    pub fog: f32,
    /// Bump-map the picture by its brightness, lit from the upper left (0 flat).
    pub relief: f32,
    /// Specular shine on that relief.
    pub gloss: f32,
    /// Glow spilling from bright areas into dark ones.
    pub bloom: f32,
    /// Shrinks the centre so the outward flow seems to come from far away.
    pub lens: f32,
    /// The campfire scene (0 none, 1 full).
    pub fire: f32,
    /// The fire's flicker: 0 at a real fire's pace, 1 at the music's.
    pub fire_tempo: f32,
    /// Salts thrown on the fire colour its flames (0 keeps it fire-coloured).
    pub salts: f32,
}

const BASE: Preset = Preset {
    name: "",
    zoom: 1.03,
    zoomexp: 1.0,
    rot: 0.0,
    warp: 0.6,
    warp_scale: 1.0,
    warp_speed: 1.0,
    swirl: 0.0,
    vortex: 0.0,
    saddle: 0.0,
    half_life: 0.6,
    bias: 0.0,
    hue: 0.008,
    folds: 0.0,
    mirror: 0.0,
    wrap: 0.0,
    drift: 0.02,
    fall: 0.0,
    blur: 0.4,
    sharpen: 0.0,
    bass_zoom: 0.12,
    mid_rot: 0.02,
    treble_warp: 0.8,
    kick: 0.4,
    wobble: 0.0,
    wave: 0.9,
    wave_mode: 0.0,
    wave_scale: 1.0,
    wave_y: 0.0,
    shape: 0.0,
    sides: 6.0,
    orbit: 0.0,
    sparks: 0.0,
    border: 0.0,
    hue_offset: 0.0,
    echo: 0.0,
    echo_zoom: 1.0,
    echo_orient: 0.0,
    chroma: 0.002,
    chroma_warp: 0.0,
    gamma: [1.0, 1.0, 1.0],
    plasma: 0.0,
    palette: 0.0,
    palette_index: 0.0,
    wander: 1.0,
    darken_center: 0.0,
    tunnel: 0.0,
    corridor: 0.0,
    fly: 0.25,
    twist: 0.0,
    fog: 0.8,
    relief: 0.45,
    gloss: 0.6,
    bloom: 0.5,
    lens: 0.25,
    fire: 0.0,
    fire_tempo: 0.0,
    salts: 0.0,
};

/// The campfire: a still camera, and a quick fade so its embers leave only short trails.
const CAMPFIRE: Preset = Preset {
    zoom: 1.0,
    warp: 0.12,
    warp_scale: 0.5,
    half_life: 0.035,
    hue: 0.0,
    drift: 0.0,
    blur: 0.0,
    bass_zoom: 0.0,
    mid_rot: 0.0,
    treble_warp: 0.3,
    kick: 0.0,
    wave: 0.0,
    chroma: 0.0,
    wander: 0.0,
    relief: 0.0,
    gloss: 0.0,
    bloom: 0.25,
    lens: 0.0,
    fire: 1.0,
    ..BASE
};

pub const PRESETS: &[Preset] = &[
    // The campfires lead the menu (SPEC 4.4).
    Preset { name: "Campfire · Music", fire_tempo: 1.0, salts: 1.0, ..CAMPFIRE },
    Preset { name: "Campfire · Natural", ..CAMPFIRE },
    Preset { name: "Liquid Prism", zoom: 1.035, rot: 0.03, warp: 0.9, blur: 0.5, half_life: 0.8, chroma_warp: 0.0015, ..BASE },
    Preset { name: "Desert Bloom", relief: 0.9, gloss: 0.9, zoom: 1.02, wander: 4.0, sharpen: 0.25, bias: 0.012, blur: 0.0, half_life: 1.5, gamma: [0.5, 0.8, 1.7], wave: 0.5, hue: 0.004, ..BASE },
    Preset { name: "Fractal Mirror", zoom: 1.0, wrap: 1.0, saddle: 0.006, half_life: 0.25, hue: 0.01, ..BASE },
    Preset { name: "Hyperspace Kaleido", lens: 0.6, zoom: 1.02, zoomexp: 1.8, kick: 0.8, folds: 8.0, bias: 0.006, half_life: 0.5, sparks: 1.0, ..BASE },
    Preset { name: "Hurricane", zoom: 1.03, vortex: 0.35, half_life: 0.6, wave: 0.8, mid_rot: 0.05, darken_center: 0.6, ..BASE },
    Preset { name: "Tokamak", saddle: 0.01, warp: 0.4, half_life: 0.9, folds: 4.0, plasma: 0.002, palette: 0.5, palette_index: 2.0, ..BASE },
    Preset { name: "Shimmer", warp: 1.6, warp_scale: 0.35, warp_speed: 1.6, half_life: 0.5, blur: 0.3, wave_mode: 1.0, ..BASE },
    Preset { name: "Counter-Spiral", zoom: 0.97, rot: 0.25, half_life: 0.8, wave: 1.0, shape: 1.0, sides: 7.0, ..BASE },
    Preset { name: "Neon Orbit", zoom: 1.0, shape: 1.0, sides: 3.0, orbit: 0.22, sparks: 0.6, half_life: 0.7, bias: 0.002, ..BASE },
    Preset { name: "Infinity Frames", zoom: 0.96, border: 0.012, echo: 0.5, echo_orient: 3.0, half_life: 1.2, wave: 0.7, ..BASE },
    Preset { name: "Songflower", echo: 1.0, echo_orient: 1.0, border: 0.006, kick: 0.6, drift: 0.06, wave: 1.0, wave_mode: 1.0, ..BASE },
    Preset { name: "Plasma Lake", zoom: 1.01, plasma: 0.006, palette: 0.5, palette_index: 4.0, hue: 0.03, half_life: 1.0, warp: 0.5, ..BASE },
    Preset { name: "Mandala Breath", folds: 12.0, wobble: 0.03, rot: 0.03, half_life: 1.0, shape: 1.0, sides: 12.0, palette: 0.4, ..BASE },
    Preset { name: "Chromatic Drift", zoom: 1.02, chroma_warp: 0.004, warp: 0.5, half_life: 1.0, ..BASE },
    Preset { name: "Oil Slick", zoom: 1.01, palette: 1.0, palette_index: 3.0, warp: 0.6, warp_speed: 0.4, plasma: 0.005, half_life: 1.4, wander: 2.0, blur: 0.6, wave: 0.5, wave_mode: 1.0, ..BASE },
    Preset { name: "Frame Tunnel", lens: 0.45, zoom: 0.96, border: 0.01, half_life: 1.4, hue: 0.01, wave: 0.6, ..BASE },
    Preset { name: "Starflow", lens: 0.7, zoom: 1.06, sparks: 1.0, half_life: 0.15, kick: 0.8, wave: 0.4, ..BASE },
    Preset { name: "Moiré Rings", zoom: 1.03, rot: 0.017, wave: 1.0, half_life: 1.2, blur: 0.2, gamma: [1.2, 1.2, 1.2], ..BASE },
    Preset { name: "Bass Tunnel", tunnel: 1.0, twist: 0.3, fly: 0.45, zoom: 1.03, zoomexp: 1.3, bass_zoom: 0.3, kick: 0.8, shape: 1.0, half_life: 0.5, wave_scale: 0.8, hue_offset: 0.3, ..BASE },
    Preset { name: "Event Horizon", lens: 0.5, zoom: 1.04, rot: 0.06, swirl: 0.2, shape: 1.0, sides: 40.0, half_life: 0.9, chroma: 0.004, darken_center: 0.4, hue_offset: 0.75, ..BASE },
    Preset { name: "Velvet Vortex", zoom: 1.05, rot: 0.08, swirl: 0.3, palette: 1.0, palette_index: 1.0, half_life: 1.0, sparks: 0.4, wave: 0.7, ..BASE },
    Preset { name: "Wormhole", tunnel: 1.0, twist: 0.5, fly: 0.35, zoom: 1.02, rot: 0.04, warp: 0.9, plasma: 0.003, palette: 0.6, half_life: 0.8, fog: 0.7, relief: 0.6, ..BASE },
    Preset { name: "Neon Corridor", corridor: 1.0, fly: 0.4, zoom: 1.0, warp: 0.5, wave_mode: 1.0, border: 0.008, half_life: 1.0, fog: 0.6, relief: 0.5, ..BASE },
    Preset { name: "Liquid Chrome", relief: 1.0, gloss: 1.4, plasma: 0.006, palette: 0.85, palette_index: 5.0, warp: 0.8, warp_speed: 0.5, half_life: 1.4, blur: 0.5, zoom: 1.01, wave: 0.4, lens: 0.3, ..BASE },
    Preset { name: "Fractal Dive", zoom: 1.07, zoomexp: 1.5, folds: 6.0, lens: 0.7, relief: 0.6, darken_center: 0.2, kick: 0.8, half_life: 0.6, bias: 0.004, wave: 0.7, ..BASE },
    Preset { name: "Acid Rain", zoom: 1.0, fall: 0.05, warp: 1.0, warp_scale: 0.6, hue: 0.02, wave_mode: 1.0, wave_y: 0.3, sparks: 1.0, half_life: 0.5, hue_offset: 0.45, ..BASE },
];

/// Cosine palettes (a, b, c, d): colour(t) = a + b·cos(2π(c·t + d)) (Inigo Quilez).
pub const PALETTES: &[[[f32; 3]; 4]] = &[
    [[0.5, 0.5, 0.5], [0.5, 0.5, 0.5], [1.0, 1.0, 1.0], [0.0, 0.33, 0.67]],
    [[0.5, 0.5, 0.5], [0.5, 0.5, 0.5], [1.0, 1.0, 1.0], [0.0, 0.1, 0.2]],
    [[0.5, 0.5, 0.5], [0.5, 0.5, 0.5], [1.0, 1.0, 0.5], [0.8, 0.9, 0.3]],
    [[0.5, 0.5, 0.5], [0.5, 0.5, 0.5], [2.0, 1.0, 0.0], [0.5, 0.2, 0.25]],
    [[0.5, 0.5, 0.5], [0.5, 0.5, 0.5], [1.0, 0.7, 0.4], [0.0, 0.15, 0.2]],
    [[0.6, 0.4, 0.6], [0.4, 0.4, 0.4], [1.0, 1.0, 1.0], [0.9, 0.6, 0.3]],
];

impl Preset {
    /// Blend toward `to` with MilkDrop's cosine ease; switches (folds, mirror, wrap, sides,
    /// palette, echo orientation) flip halfway.
    pub fn lerp(&self, to: &Preset, k: f32) -> Preset {
        let m = |a: f32, b: f32| a + (b - a) * k;
        let flip = |a: f32, b: f32| if k < 0.5 { a } else { b };
        Preset {
            name: if k < 0.5 { self.name } else { to.name },
            zoom: m(self.zoom, to.zoom),
            zoomexp: m(self.zoomexp, to.zoomexp),
            rot: m(self.rot, to.rot),
            warp: m(self.warp, to.warp),
            warp_scale: m(self.warp_scale, to.warp_scale),
            warp_speed: m(self.warp_speed, to.warp_speed),
            swirl: m(self.swirl, to.swirl),
            vortex: m(self.vortex, to.vortex),
            saddle: m(self.saddle, to.saddle),
            half_life: m(self.half_life, to.half_life),
            bias: m(self.bias, to.bias),
            hue: m(self.hue, to.hue),
            folds: flip(self.folds, to.folds),
            mirror: flip(self.mirror, to.mirror),
            wrap: flip(self.wrap, to.wrap),
            drift: m(self.drift, to.drift),
            fall: m(self.fall, to.fall),
            blur: m(self.blur, to.blur),
            sharpen: m(self.sharpen, to.sharpen),
            bass_zoom: m(self.bass_zoom, to.bass_zoom),
            mid_rot: m(self.mid_rot, to.mid_rot),
            treble_warp: m(self.treble_warp, to.treble_warp),
            kick: m(self.kick, to.kick),
            wobble: m(self.wobble, to.wobble),
            wave: m(self.wave, to.wave),
            wave_mode: m(self.wave_mode, to.wave_mode),
            wave_scale: m(self.wave_scale, to.wave_scale),
            wave_y: m(self.wave_y, to.wave_y),
            shape: m(self.shape, to.shape),
            sides: flip(self.sides, to.sides),
            orbit: m(self.orbit, to.orbit),
            sparks: m(self.sparks, to.sparks),
            border: m(self.border, to.border),
            hue_offset: m(self.hue_offset, to.hue_offset),
            echo: m(self.echo, to.echo),
            echo_zoom: m(self.echo_zoom, to.echo_zoom),
            echo_orient: flip(self.echo_orient, to.echo_orient),
            chroma: m(self.chroma, to.chroma),
            chroma_warp: m(self.chroma_warp, to.chroma_warp),
            gamma: [m(self.gamma[0], to.gamma[0]), m(self.gamma[1], to.gamma[1]), m(self.gamma[2], to.gamma[2])],
            plasma: m(self.plasma, to.plasma),
            palette: m(self.palette, to.palette),
            palette_index: flip(self.palette_index, to.palette_index),
            wander: m(self.wander, to.wander),
            darken_center: m(self.darken_center, to.darken_center),
            tunnel: m(self.tunnel, to.tunnel),
            corridor: m(self.corridor, to.corridor),
            fly: m(self.fly, to.fly),
            twist: m(self.twist, to.twist),
            fog: m(self.fog, to.fog),
            relief: m(self.relief, to.relief),
            gloss: m(self.gloss, to.gloss),
            bloom: m(self.bloom, to.bloom),
            lens: m(self.lens, to.lens),
            fire: m(self.fire, to.fire),
            fire_tempo: m(self.fire_tempo, to.fire_tempo),
            salts: m(self.salts, to.salts),
        }
    }
}
