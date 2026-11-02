// Campfire: a 3D fire simulation (SPEC 4.4).
//
// Stam's "Stable Fluids" on the GPU (GPU Gems ch. 38), in 3D with MacCormack advection as in
// GPU Gems 3 ch. 30, plus fuel, combustion, soot, buoyancy and vorticity confinement after
// Nguyen, Fedkiw & Jensen, "Physically Based Modeling and Animation of Fire" (SIGGRAPH 2002).
// Fuel bleeds from the logs and the coal bed and burns into heat and soot; hot gas rises and
// curls; soot glows while hot, burns away higher up, and what survives drifts off as smoke.
//
// World units are metres with y up; temperatures are in thousands of kelvin. Fields:
//   velocity  rgba16f  xyz in m/s; how fast the flow spins there (1/s)
//   scalars   rgba16f  temperature, fuel, soot, salt (which of two salts, 0..1)
//   curl      rgba16f  vorticity xyz and its length
//   pressure  r32f
//   divergence rg32f   divergence, and 1 where the cell is inside a log
//   detail    rgba16f  the scalars again on a grid twice as fine, which is what's drawn

struct Sim {
    grid: vec4<f32>,     // cells x, y, z; cell size (m)
    origin: vec4<f32>,   // the grid's min corner (m); time step (s)
    forces: vec4<f32>,   // buoyancy (m/s² per kK), vorticity confinement, cooling (1/s), turbulence
    burn: vec4<f32>,     // fuel rate (1/s), burn rate (1/s), heat per fuel (kK), soot yield
    flow: vec4<f32>,     // flicker clock (s), kick, salts (0 none), salt drift
    coals: vec4<f32>,    // coal bed radius (m), glow, light gain, log count
    sparks: vec4<f32>,   // first spark to spawn, how many, frame time (s), frame seed
    salt_a: vec4<f32>,   // the two salts' colours, times their strength (per metre)
    salt_b: vec4<f32>,
    detail: vec4<f32>,   // the detail grid's cells x, y, z; cell size (m)
    subgrid: vec4<f32>,  // eddy strength, noise tiles per metre, eddies' rise (m/s), max eddy (m/s)
    expose: vec4<f32>,   // seconds since the last exposure, reference light, how fully to adapt, time constant (s)
    bands: array<vec4<f32>, 2>,
    // Per log: end a and radius, then end b.
    logs: array<vec4<f32>, 24>,
}

@group(0) @binding(0) var<uniform> s: Sim;
@group(0) @binding(1) var a: texture_3d<f32>;
@group(0) @binding(2) var b: texture_3d<f32>;
@group(0) @binding(3) var c: texture_3d<f32>;
@group(0) @binding(4) var d: texture_3d<f32>;
@group(0) @binding(5) var lin: sampler;
@group(0) @binding(6) var out4: texture_storage_3d<rgba16float, write>;
@group(0) @binding(7) var out1: texture_storage_3d<r32float, write>;
@group(0) @binding(8) var out2: texture_storage_3d<rg32float, write>;
@group(0) @binding(9) var<storage, read_write> buf: array<vec4<f32>>;
@group(0) @binding(10) var eddies: texture_3d<f32>;
@group(0) @binding(11) var wrap: sampler;
// Per 8³ block of the detail grid: 2 if it runs this step, plus 1 if anything in it shows.
@group(0) @binding(12) var<storage, read_write> occupied: array<atomic<u32>>;
// The fire's static shape (bake_shape): where fuel can bleed from, the distance to the
// nearest solid, and to the nearest log (m, negative inside).
@group(0) @binding(13) var shape: texture_3d<f32>;

const AMBIENT: f32 = 0.29;
const X: vec3<i32> = vec3(1, 0, 0);
const Y: vec3<i32> = vec3(0, 1, 0);
const Z: vec3<i32> = vec3(0, 0, 1);
/// Light bricks: the fire is summed into this many point lights.
const BRICKS: vec3<u32> = vec3(4u, 6u, 4u);
const COAL_LIGHTS: u32 = 8u;

fn dims() -> vec3<i32> {
    return vec3<i32>(s.grid.xyz);
}

fn world(cell: vec3<i32>) -> vec3<f32> {
    return s.origin.xyz + (vec3<f32>(cell) + 0.5) * s.grid.w;
}

fn uvw(p: vec3<f32>) -> vec3<f32> {
    return (p - s.origin.xyz) / (s.grid.xyz * s.grid.w);
}

fn inside(cell: vec3<i32>) -> bool {
    return all(cell >= vec3(0)) && all(cell < dims());
}

fn load4(t: texture_3d<f32>, cell: vec3<i32>) -> vec4<f32> {
    return textureLoad(t, clamp(cell, vec3(0), dims() - 1), 0);
}

// ---- noise ----

fn hash_u(x: u32) -> u32 {
    let state = x * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn rand(seed: u32) -> f32 {
    return f32(hash_u(seed) >> 8u) / 16777216.0;
}

fn lattice(i: vec3<i32>, f: vec3<f32>) -> f32 {
    let q = bitcast<vec3<u32>>(i);
    let h = hash_u(q.x ^ hash_u(q.y ^ hash_u(q.z)));
    let g = vec3(f32(h & 255u), f32((h >> 8u) & 255u), f32((h >> 16u) & 255u)) / 127.5 - 1.0;
    return dot(g, f);
}

fn noise(p: vec3<f32>) -> f32 {
    let i = vec3<i32>(floor(p));
    let f = fract(p);
    let w = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let x00 = mix(lattice(i, f), lattice(i + X, f - vec3(1.0, 0.0, 0.0)), w.x);
    let x10 = mix(lattice(i + Y, f - vec3(0.0, 1.0, 0.0)), lattice(i + X + Y, f - vec3(1.0, 1.0, 0.0)), w.x);
    let x01 = mix(lattice(i + Z, f - vec3(0.0, 0.0, 1.0)), lattice(i + X + Z, f - vec3(1.0, 0.0, 1.0)), w.x);
    let x11 = mix(lattice(i + Y + Z, f - vec3(0.0, 1.0, 1.0)), lattice(i + X + Y + Z, f - vec3(1.0, 1.0, 1.0)), w.x);
    return mix(mix(x00, x10, w.y), mix(x01, x11, w.y), w.z);
}

// ---- the logs ----

fn log_count() -> i32 {
    return i32(s.coals.w);
}

// Distance from a log's surface, and its outward normal.
fn log_sdf(p: vec3<f32>, i: i32) -> vec4<f32> {
    let ar = s.logs[i * 2];
    let bb = s.logs[i * 2 + 1].xyz;
    let ba = bb - ar.xyz;
    let h = clamp(dot(p - ar.xyz, ba) / dot(ba, ba), 0.0, 1.0);
    let v = p - ar.xyz - ba * h;
    let l = max(length(v), 1e-6);
    return vec4(l - ar.w, v / l);
}

// The pit is a dug bowl, 15 cm deep in the middle, inside a rim of stones.
fn floor_y(r: f32) -> f32 {
    return -0.151 + 0.25 * r * r;
}

// Whether a cell is inside a log or the ground (from the baked shape).
fn solid(cell: vec3<i32>) -> bool {
    return textureLoad(shape, clamp(cell, vec3(0), dims() - 1), 0).y < 0.0;
}

// Distance to the nearest log's surface (m), baked.
fn log_distance(cell: vec3<i32>) -> f32 {
    return textureLoad(shape, clamp(cell, vec3(0), dims() - 1), 0).z;
}

fn band(i: u32) -> f32 {
    return s.bands[i / 4u][i % 4u];
}

// Where fuel bleeds out (rate 0..1) and which salt it carries. It comes off the tops and
// sides of the logs inside the pit, and off the coal bed (all baked into `shape`), in patches
// that wander slowly. The spectrum maps around the pit: bass at the front, treble at the back.
fn fuel_source(p: vec3<f32>, site: f32) -> vec2<f32> {
    if site <= 0.0 {
        return vec2(0.0);
    }
    let t = s.flow.x;
    let patches = smoothstep(0.15, 0.65, 0.5 + 0.8 * noise(vec3(p * 9.0) + vec3(0.0, 0.0, t * 0.2)));
    let angle = atan2(p.z, p.x) / 6.2831853 + 0.5;
    let f = angle * 7.999;
    let i = u32(f);
    let music = mix(band(i), band((i + 1u) % 8u), fract(f));
    let rate = site * patches * (0.5 + 0.8 * music);
    let salt = smoothstep(-0.35, 0.35, noise(vec3(p.x * 5.0, p.z * 5.0, s.flow.w)));
    return vec2(rate, salt);
}

@compute @workgroup_size(8, 8, 4)
fn bake_shape(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if !inside(cell) {
        return;
    }
    let p = world(cell);
    let r = length(p.xz);
    var rate = 0.0;
    var logs = 1e9;
    for (var i = 0; i < log_count(); i++) {
        let l = log_sdf(p, i);
        let shell = smoothstep(5.0 * s.grid.w, 0.0, l.x) * step(0.0, l.x);
        rate = max(rate, shell * smoothstep(-0.6, 0.3, l.w));
        logs = min(logs, l.x);
    }
    let dist = min(min(p.y - floor_y(r), max(0.56 - r, p.y - 0.15)), logs);
    let bed = (1.0 - smoothstep(s.coals.x * 0.6, s.coals.x, r)) * (1.0 - smoothstep(0.0, 3.0 * s.grid.w, p.y - floor_y(r)));
    let pit = 1.0 - smoothstep(s.coals.x * 0.85, s.coals.x * 1.1, r);
    textureStore(out4, cell, vec4(max(rate, bed) * pit, clamp(dist, -0.1, 0.1), clamp(logs, -0.1, 0.1), 0.0));
}

// ---- velocity ----

fn vel(cell: vec3<i32>) -> vec3<f32> {
    return load4(a, cell).xyz;
}

@compute @workgroup_size(8, 8, 4)
fn curl(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if !inside(cell) {
        return;
    }
    let h2 = 2.0 * s.grid.w;
    let dx = (vel(cell + X) - vel(cell - X)) / h2;
    let dy = (vel(cell + Y) - vel(cell - Y)) / h2;
    let dz = (vel(cell + Z) - vel(cell - Z)) / h2;
    let w = vec3(dy.z - dz.y, dz.x - dx.z, dx.y - dy.x);
    textureStore(out4, cell, vec4(w, length(w)));
}

fn spin(cell: vec3<i32>) -> f32 {
    return load4(b, cell).w;
}

// Forces: vorticity confinement restores the small curls the grid smooths away, hot gas
// rises (soot weighs it down a little), and fine turbulence keeps the flow from settling.
@compute @workgroup_size(8, 8, 4)
fn forces(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if !inside(cell) {
        return;
    }
    let p = world(cell);
    if solid(cell) {
        textureStore(out4, cell, vec4(0.0));
        return;
    }
    let h = s.grid.w;
    let w = load4(b, cell).xyz;
    let grad = vec3(spin(cell + X) - spin(cell - X), spin(cell + Y) - spin(cell - Y), spin(cell + Z) - spin(cell - Z));
    let n = grad / (length(grad) + 1e-5);
    let confine = s.forces.y * h * cross(n, w);
    let sc = load4(c, cell);
    let lift = vec3(0.0, s.forces.x * (sc.x - AMBIENT) - 2.0 * sc.z, 0.0);
    let t = s.flow.x;
    let q = p * 6.0 + vec3(0.0, -t * 2.2, t * 0.4);
    let swirl = vec3(noise(q), noise(q + vec3(31.7, 0.0, 0.0)), noise(q + vec3(0.0, 17.3, 0.0)));
    let turbulence = swirl * s.forces.w * smoothstep(0.5, 1.2, sc.x);
    // Wood gas jets out of the burning surfaces before it mixes and burns, which is what
    // gives flames their body.
    let d = log_distance(cell);
    let normal = vec3(
        log_distance(cell + X) - log_distance(cell - X),
        log_distance(cell + Y) - log_distance(cell - Y),
        log_distance(cell + Z) - log_distance(cell - Z),
    );
    let jet = normal / max(length(normal), 1e-6) * smoothstep(5.0 * h, 0.0, d) * 3.0;
    let dt = s.origin.w;
    // Air drag, stronger toward the open edges so nothing reflects back in, and a cap well
    // above any real fire's speeds.
    let edge = min(min(p.x - s.origin.x, s.origin.x + s.grid.x * h - p.x), min(p.z - s.origin.z, s.origin.z + s.grid.z * h - p.z));
    let sponge = 0.3 + 4.0 * (1.0 - smoothstep(0.0, 0.12, edge));
    var v = (vel(cell) + dt * (confine + lift + turbulence + jet)) / (1.0 + sponge * dt);
    let speed = length(v);
    if speed > 6.0 {
        v *= 6.0 / speed;
    }
    textureStore(out4, cell, vec4(v, spin(cell)));
}

// A neighbour's velocity, with the boundaries: logs and the ground are walls, the sides and
// top are open to the night air.
fn wall_vel(cell: vec3<i32>, mid: vec3<f32>) -> vec3<f32> {
    if cell.y < 0 {
        return vec3(0.0);
    }
    if !inside(cell) {
        return mid;
    }
    if solid(cell) {
        return vec3(0.0);
    }
    return vel(cell);
}

@compute @workgroup_size(8, 8, 4)
fn divergence(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if !inside(cell) {
        return;
    }
    let is_solid = solid(cell);
    let mid = vel(cell);
    let div = (wall_vel(cell + X, mid).x - wall_vel(cell - X, mid).x + wall_vel(cell + Y, mid).y - wall_vel(cell - Y, mid).y
        + wall_vel(cell + Z, mid).z - wall_vel(cell - Z, mid).z) / (2.0 * s.grid.w);
    textureStore(out2, cell, vec4(select(div, 0.0, is_solid), select(0.0, 1.0, is_solid), 0.0, 0.0));
}

// ---- pressure: a multigrid Poisson solve ----
// Plain Jacobi iteration barely moves the large scales on a grid this size, so the solve is
// a multigrid V-cycle: smooth, hand the leftover error to a grid half as fine, solve it there
// (recursively), add the correction back, smooth again. `c` is a level's pressure, `d` its
// right-hand side (the divergence) and whether each cell is inside a log.

fn inside_of(cell: vec3<i32>, size: vec3<i32>) -> bool {
    return all(cell >= vec3(0)) && all(cell < size);
}

// A level's cell size.
fn level_h(size: vec3<i32>) -> f32 {
    return s.grid.w * s.grid.x / f32(size.x);
}

// A neighbour's pressure: ambient (zero) at the faces of the open sides and top, mirrored at
// the ground and the logs. Putting zero exactly on the face (not on the ghost cell's
// centre) keeps every multigrid level's boundary in the same place.
fn p_at(cell: vec3<i32>, mid: f32, size: vec3<i32>) -> f32 {
    if cell.y < 0 {
        return mid;
    }
    if !inside_of(cell, size) {
        return -mid;
    }
    if textureLoad(d, cell, 0).y > 0.5 {
        return mid;
    }
    return textureLoad(c, cell, 0).x;
}

fn neighbours(cell: vec3<i32>, mid: f32, size: vec3<i32>) -> f32 {
    return p_at(cell + X, mid, size) + p_at(cell - X, mid, size) + p_at(cell + Y, mid, size) + p_at(cell - Y, mid, size)
        + p_at(cell + Z, mid, size) + p_at(cell - Z, mid, size);
}

// For the projection on the finest level.
fn pressure_at(cell: vec3<i32>, mid: f32) -> f32 {
    return p_at(cell, mid, dims());
}

// One weighted-Jacobi smoothing sweep on any level.
@compute @workgroup_size(8, 8, 4)
fn jacobi(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec3<i32>(textureDimensions(c));
    let cell = vec3<i32>(gid);
    if !inside_of(cell, size) {
        return;
    }
    let rhs = textureLoad(d, cell, 0);
    if rhs.y > 0.5 {
        textureStore(out1, cell, vec4(0.0));
        return;
    }
    let mid = textureLoad(c, cell, 0).x;
    let h = level_h(size);
    let solved = (neighbours(cell, mid, size) - rhs.x * h * h) / 6.0;
    textureStore(out1, cell, vec4(mix(mid, solved, 0.8), 0.0, 0.0, 0.0));
}

// The leftover error on a fine level, averaged onto the next coarser one.
@compute @workgroup_size(8, 8, 4)
fn coarsen(@builtin(global_invocation_id) gid: vec3<u32>) {
    let coarse = vec3<i32>(textureDimensions(out2));
    let cc = vec3<i32>(gid);
    if !inside_of(cc, coarse) {
        return;
    }
    let size = vec3<i32>(textureDimensions(c));
    let h = level_h(size);
    var sum = 0.0;
    var n = 0.0;
    for (var k = 0; k < 8; k++) {
        let f = cc * 2 + vec3(k & 1, (k >> 1) & 1, (k >> 2) & 1);
        if !inside_of(f, size) {
            continue;
        }
        let rhs = textureLoad(d, f, 0);
        n += 1.0;
        if rhs.y > 0.5 {
            continue;
        }
        let mid = textureLoad(c, f, 0).x;
        sum += rhs.x - (neighbours(f, mid, size) - 6.0 * mid) / (h * h);
    }
    textureStore(out2, cc, vec4(sum / max(n, 1.0), 0.0, 0.0, 0.0));
}

// A coarse level starts from zero.
@compute @workgroup_size(8, 8, 4)
fn clear(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if inside_of(cell, vec3<i32>(textureDimensions(out1))) {
        textureStore(out1, cell, vec4(0.0));
    }
}

// Add the coarse level's correction, interpolated, back onto the fine one.
@compute @workgroup_size(8, 8, 4)
fn prolong(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = vec3<i32>(textureDimensions(c));
    let cell = vec3<i32>(gid);
    if !inside_of(cell, size) {
        return;
    }
    let coarse = vec3<i32>(textureDimensions(d));
    let g = (vec3<f32>(cell) + 0.5) * 0.5 - 0.5;
    let i0 = vec3<i32>(floor(g));
    let f = g - floor(g);
    var correction = 0.0;
    for (var k = 0; k < 8; k++) {
        let o = vec3(k & 1, (k >> 1) & 1, (k >> 2) & 1);
        let w = mix(1.0 - f, f, vec3<f32>(o));
        let q = clamp(i0 + o, vec3(0), coarse - 1);
        correction += textureLoad(d, q, 0).x * w.x * w.y * w.z;
    }
    textureStore(out1, cell, vec4(textureLoad(c, cell, 0).x + correction, 0.0, 0.0, 0.0));
}

// Subtract the pressure gradient, leaving the flow incompressible.
@compute @workgroup_size(8, 8, 4)
fn project(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if !inside(cell) {
        return;
    }
    if textureLoad(d, cell, 0).y > 0.5 {
        textureStore(out4, cell, vec4(0.0));
        return;
    }
    let mid = textureLoad(c, cell, 0).x;
    let grad = vec3(
        pressure_at(cell + X, mid) - pressure_at(cell - X, mid),
        pressure_at(cell + Y, mid) - pressure_at(cell - Y, mid),
        pressure_at(cell + Z, mid) - pressure_at(cell - Z, mid),
    ) / (2.0 * s.grid.w);
    textureStore(out4, cell, vec4(vel(cell) - grad, load4(a, cell).w));
}

// ---- advection (MacCormack) ----
// Whatever flows in through the open sides and top is still night air: at rest, at ambient
// temperature, with no fuel or soot. (Copying the edge cells instead, as clamped sampling
// would, lets a downdraft at the top feed itself until it pins the fire down.)

const STILL_AIR: vec4<f32> = vec4(0.0);
const NIGHT_AIR: vec4<f32> = vec4(AMBIENT, 0.0, 0.0, 0.0);

fn outside(q: vec3<f32>) -> bool {
    return any(q < vec3(0.0)) || any(q > vec3(1.0));
}

// The forward semi-Lagrangian step: what flows into each cell from upstream.
fn advected(cell: vec3<i32>, air: vec4<f32>) -> vec4<f32> {
    let back = uvw(world(cell) - vel(cell) * s.origin.w);
    if outside(back) {
        return air;
    }
    return textureSampleLevel(b, lin, back, 0.0);
}

@compute @workgroup_size(8, 8, 4)
fn advect_velocity(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if inside(cell) {
        textureStore(out4, cell, advected(cell, STILL_AIR));
    }
}

@compute @workgroup_size(8, 8, 4)
fn advect_scalars(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if inside(cell) {
        textureStore(out4, cell, advected(cell, NIGHT_AIR));
    }
}

// The MacCormack correction: step the forward result back again, take half the error out,
// and clamp to the cells it came from so it can't ring. Much sharper than plain advection.
// Air that came in from outside is left as it is.
fn corrected(cell: vec3<i32>) -> vec4<f32> {
    let p = world(cell);
    let v = vel(cell) * s.origin.w;
    let hat = textureLoad(b, cell, 0);
    if outside(uvw(p - v)) {
        return hat;
    }
    let again = textureSampleLevel(b, lin, uvw(p + v), 0.0);
    let original = textureLoad(c, cell, 0);
    let fixed = hat + 0.5 * (original - again);
    let g = uvw(p - v) * s.grid.xyz - 0.5;
    let i0 = vec3<i32>(floor(g));
    var lo = vec4(1e9);
    var hi = vec4(-1e9);
    for (var k = 0; k < 8; k++) {
        let n = load4(c, i0 + vec3(k & 1, (k >> 1) & 1, (k >> 2) & 1));
        lo = min(lo, n);
        hi = max(hi, n);
    }
    return clamp(fixed, lo, hi);
}

@compute @workgroup_size(8, 8, 4)
fn correct_velocity(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if !inside(cell) {
        return;
    }
    var v = corrected(cell);
    if solid(cell) {
        v = vec4(0.0);
    }
    textureStore(out4, cell, v);
}

// The chemistry: fuel bleeds out and burns into heat and soot, soot burns off where it's
// hottest, and everything cools (radiating hardest when hottest). `q` is the gas at `p` after
// advection: temperature, fuel, soot, salt.
fn chemistry(p: vec3<f32>, q: vec4<f32>) -> vec4<f32> {
    var temp = max(q.x, AMBIENT);
    var fuel = q.y;
    var soot = q.z;
    var salt = q.w;
    let dt = s.origin.w;
    let here = textureSampleLevel(shape, lin, uvw(p), 0.0);
    let src = fuel_source(p, here.x);
    let added = src.x * s.burn.x * dt;
    fuel += added;
    salt = mix(salt, src.y, clamp(added * 4.0, 0.0, 1.0));
    // The surfaces it bleeds from are burning, so fresh fuel is already alight.
    temp = max(temp, mix(temp, 1.15, src.x));
    let burn = fuel * (1.0 - exp(-s.burn.y * dt)) * smoothstep(0.7, 1.0, temp);
    fuel -= burn;
    // Wood gas that escapes the flames unburnt thins out in the night air, too lean to burn.
    fuel *= exp(-dt * 2.0 * (1.0 - smoothstep(0.5, 0.8, temp)));
    temp += burn * s.burn.z;
    soot += burn * s.burn.w;
    // Soot burns away at the hot tips; what cools first drifts off as thinning smoke.
    soot -= soot * (1.0 - exp(-dt * 1.5 * smoothstep(1.35, 1.85, temp)));
    soot *= exp(-dt * (0.15 + 2.5 * (1.0 - smoothstep(0.6, 1.0, temp))));
    let hot = temp - AMBIENT;
    temp = AMBIENT + hot * exp(-s.forces.z * dt) - dt * 0.06 * (pow(temp, 4.0) - pow(AMBIENT, 4.0));
    if here.y < 0.0 {
        fuel = 0.0;
        soot = 0.0;
    }
    return vec4(max(temp, AMBIENT), fuel, soot, salt);
}

@compute @workgroup_size(8, 8, 4)
fn correct_scalars(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if inside(cell) {
        textureStore(out4, cell, chemistry(world(cell), max(corrected(cell), vec4(0.0))));
    }
}

// ---- detail: the flames on a grid twice as fine ----
// The grid above carries the fire's motion, but its 6 mm cells (and the filtering between
// them) blur every flame edge. So the scalars run again on a grid twice as fine, as in Kim et
// al., "Wavelet Turbulence for Fluid Simulation" (SIGGRAPH 2008): carried by the coarse flow
// plus the eddies too small for it to hold, burning by the same chemistry, and drawn from
// there. Its gas doesn't push back on the flow; the coarse grid's own scalars do that.

fn detail_dims() -> vec3<i32> {
    return vec3<i32>(s.detail.xyz);
}

fn detail_world(cell: vec3<i32>) -> vec3<f32> {
    return s.origin.xyz + (vec3<f32>(cell) + 0.5) * s.detail.w;
}

// The coarse flow, smoothly: a tricubic B-spline from eight trilinear taps (Sigg & Hadwiger,
// GPU Gems 2 ch. 20). Trilinear's kinks at every coarse cell would crease the finer gas.
fn coarse_flow(p: vec3<f32>) -> vec4<f32> {
    let coord = uvw(p) * s.grid.xyz - 0.5;
    let i = floor(coord);
    let f = coord - i;
    let f2 = f * f;
    let f3 = f2 * f;
    let w0 = (1.0 - 3.0 * f + 3.0 * f2 - f3) / 6.0;
    let w1 = (4.0 - 6.0 * f2 + 3.0 * f3) / 6.0;
    let w2 = (1.0 + 3.0 * f + 3.0 * f2 - 3.0 * f3) / 6.0;
    let w3 = f3 / 6.0;
    let g0 = w0 + w1;
    let g1 = w2 + w3;
    let h0 = (i - 0.5 + w1 / g0) / s.grid.xyz;
    let h1 = (i + 1.5 + w3 / g1) / s.grid.xyz;
    var sum = vec4(0.0);
    for (var k = 0; k < 8; k++) {
        let o = vec3(f32(k & 1), f32((k >> 1) & 1), f32((k >> 2) & 1));
        let w = mix(g0, g1, o);
        sum += textureSampleLevel(a, lin, mix(h0, h1, o), 0.0) * (w.x * w.y * w.z);
    }
    return sum;
}

// The coarse flow at `p`, plus eddies smaller than its cells: curl noise (divergence-free, so
// it stirs without piling gas up) in two octaves, as strong as the coarse flow spins there
// (Kolmogorov: an octave down is 2^-1/3 as strong), drifting up with the rising gas.
fn detail_vel(p: vec3<f32>) -> vec3<f32> {
    let coarse = coarse_flow(p);
    let rise = vec3(0.0, s.flow.x * s.subgrid.z, 0.0);
    let k = s.subgrid.y;
    let n1 = textureSampleLevel(eddies, wrap, (p - rise) * k, 0.0).xyz;
    let n2 = textureSampleLevel(eddies, wrap, (p - rise * 1.6) * k * 2.03 + vec3(0.37, 0.11, 0.71), 0.0).xyz;
    let strength = min(s.subgrid.x * coarse.w * s.grid.w, s.subgrid.w);
    return coarse.xyz + (n1 + 0.79 * n2) * strength;
}

// Which blocks of the detail grid run this step: wherever the coarse gas (already stepped)
// has anything in it, within a coarse cell, or fuel could bleed. The rest is night air,
// written once more on each of the two steps after a block stops running (one for each
// ping-pong texture), then left alone. Bits: 2 runs now, 4 ran last step, 8 the one before.
// One workgroup per block, one invocation per coarse cell it covers (4³, and one more each
// way: 6³).
var<workgroup> live: atomic<u32>;

@compute @workgroup_size(6, 6, 6)
fn detail_blocks(@builtin(workgroup_id) block: vec3<u32>, @builtin(local_invocation_id) local: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let blocks = vec3<u32>(detail_dims()) / 8u;
    let per = dims() / vec3<i32>(blocks);
    let cell = clamp(vec3<i32>(block) * per + vec3<i32>(local) - 1, vec3(0), dims() - 1);
    let g = textureLoad(a, cell, 0);
    if g.z > 0.01 || g.x > 0.7 || (g.y > 1e-3 && g.x > 0.5) || textureLoad(shape, cell, 0).x > 0.0 {
        atomicOr(&live, 1u);
    }
    workgroupBarrier();
    if li == 0u {
        let index = block.x + blocks.x * (block.y + blocks.y * block.z);
        let ran = atomicLoad(&occupied[index]);
        atomicStore(&occupied[index], select(0u, 2u, atomicLoad(&live) != 0u) | ((ran & 6u) << 1u));
    }
}

var<workgroup> runs: u32;
const RUNS: u32 = 2u;
const RAN: u32 = 12u;
var<workgroup> seen: atomic<u32>;

fn block_index(wg: vec3<u32>) -> u32 {
    // Workgroups are 8×8×4 cells: two to a block.
    let blocks = vec3<u32>(detail_dims()) / 8u;
    return wg.x + blocks.x * (wg.y + blocks.y * (wg.z / 2u));
}

// This workgroup's block's bits (the same for all of it).
fn block_bits(wg: vec3<u32>, li: u32) -> u32 {
    if li == 0u {
        runs = atomicLoad(&occupied[block_index(wg)]);
    }
    return workgroupUniformLoad(&runs);
}

@compute @workgroup_size(8, 8, 4)
fn detail_advect(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let cell = vec3<i32>(gid);
    let bits = block_bits(wg, li);
    if (bits & RUNS) == 0u {
        if (bits & RAN) != 0u {
            textureStore(out4, cell, NIGHT_AIR);
        }
        return;
    }
    let p = detail_world(cell);
    let back = uvw(p - detail_vel(p) * s.origin.w);
    textureStore(out4, cell, select(textureSampleLevel(b, lin, back, 0.0), NIGHT_AIR, outside(back)));
}

// MacCormack on the detail grid, then the chemistry; and note which blocks have anything to
// draw, so the renderer can skip the empty air.
@compute @workgroup_size(8, 8, 4)
fn detail_correct(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let cell = vec3<i32>(gid);
    let size = detail_dims();
    let bits = block_bits(wg, li);
    if (bits & RUNS) == 0u {
        if (bits & RAN) != 0u {
            textureStore(out4, cell, NIGHT_AIR);
        }
        return;
    }
    let p = detail_world(cell);
    let v = detail_vel(p) * s.origin.w;
    var q = textureLoad(b, cell, 0);
    let back = uvw(p - v);
    if !outside(back) {
        let again = textureSampleLevel(b, lin, uvw(p + v), 0.0);
        let fixed = q + 0.5 * (textureLoad(c, cell, 0) - again);
        let i0 = vec3<i32>(floor(back * vec3<f32>(size) - 0.5));
        var lo = vec4(1e9);
        var hi = vec4(-1e9);
        for (var k = 0; k < 8; k++) {
            let n = textureLoad(c, clamp(i0 + vec3(k & 1, (k >> 1) & 1, (k >> 2) & 1), vec3(0), size - 1), 0);
            lo = min(lo, n);
            hi = max(hi, n);
        }
        q = clamp(fixed, lo, hi);
    }
    let gas = chemistry(p, max(q, vec4(0.0)));
    textureStore(out4, cell, gas);
    if gas.z > 0.01 || gas.x > 0.9 {
        atomicOr(&seen, 1u);
    }
    workgroupBarrier();
    if li == 0u && atomicLoad(&seen) != 0u {
        atomicOr(&occupied[block_index(wg)], 1u);
    }
}

// ---- the eddies' noise, made once ----
// A tile of curl noise that repeats every EDDY_TILE texels: gradient noise on a lattice that
// wraps, three channels of it as a vector potential, and its curl.

const EDDY_TILE: i32 = 128;
const EDDY_CELL: i32 = 8;
const EDDY_GAIN: f32 = 1.0;

fn tiled_noise(p: vec3<f32>, seed: u32) -> f32 {
    let period = vec3(EDDY_TILE / EDDY_CELL);
    let i = vec3<i32>(floor(p));
    let f = fract(p);
    let w = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    var corners: array<f32, 8>;
    for (var k = 0; k < 8; k++) {
        let o = vec3(k & 1, (k >> 1) & 1, (k >> 2) & 1);
        let q = bitcast<vec3<u32>>(((i + o) % period + period) % period);
        let h = hash_u(seed ^ hash_u(q.x ^ hash_u(q.y ^ hash_u(q.z))));
        let g = vec3(f32(h & 255u), f32((h >> 8u) & 255u), f32((h >> 16u) & 255u)) / 127.5 - 1.0;
        corners[k] = dot(g, f - vec3<f32>(o));
    }
    let x0 = mix(mix(corners[0], corners[1], w.x), mix(corners[2], corners[3], w.x), w.y);
    let x1 = mix(mix(corners[4], corners[5], w.x), mix(corners[6], corners[7], w.x), w.y);
    return mix(x0, x1, w.z);
}

@compute @workgroup_size(8, 8, 4)
fn eddy_potential(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if all(cell < vec3(EDDY_TILE)) {
        let p = (vec3<f32>(cell) + 0.5) / f32(EDDY_CELL);
        textureStore(out4, cell, vec4(tiled_noise(p, 1u), tiled_noise(p, 2u), tiled_noise(p, 3u), 0.0));
    }
}

fn potential(cell: vec3<i32>) -> vec3<f32> {
    return textureLoad(b, (cell + EDDY_TILE) % EDDY_TILE, 0).xyz;
}

// Scaled to about 1 m/s RMS per component.
@compute @workgroup_size(8, 8, 4)
fn eddy_curl(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if all(cell < vec3(EDDY_TILE)) {
        let dx = potential(cell + X) - potential(cell - X);
        let dy = potential(cell + Y) - potential(cell - Y);
        let dz = potential(cell + Z) - potential(cell - Z);
        let w = vec3(dy.z - dz.y, dz.x - dx.z, dx.y - dy.x) * 0.5 * f32(EDDY_CELL) * EDDY_GAIN;
        textureStore(out4, cell, vec4(w, 0.0));
    }
}

// ---- light from the fire ----

// Glowing soot by temperature (b is the blackbody table, 500–2600 K).
fn glow(temp: f32) -> vec3<f32> {
    let u = clamp((temp * 1000.0 - 500.0) / 2100.0, 0.0, 1.0);
    return textureSampleLevel(b, lin, vec3(u, 0.5, 0.5), 0.0).rgb;
}

var<workgroup> power: array<vec4<f32>, 256>;
var<workgroup> centre: array<vec4<f32>, 256>;

// Sum the fire's glow into a few dozen point lights (one per brick of the grid), so the
// scene can be lit by the flames themselves. Writes two vec4s per light: position and
// luminance, then colour. The coal bed adds a ring of lights of its own.
@compute @workgroup_size(256)
fn lights(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let size = vec3<u32>(dims()) / BRICKS;
    let start = wg * size;
    // Every other cell along each axis is plenty for lighting.
    let half_size = max(size / 2u, vec3(1u));
    let count = half_size.x * half_size.y * half_size.z;
    var e = vec4(0.0);
    var at = vec4(0.0);
    for (var i = li; i < count; i += 256u) {
        let local = vec3(i % half_size.x, (i / half_size.x) % half_size.y, i / (half_size.x * half_size.y)) * 2u;
        let cell = vec3<i32>(start + local);
        let sc = textureLoad(a, cell, 0);
        let salt = mix(s.salt_a.rgb, s.salt_b.rgb, sc.w) * smoothstep(1.3, 1.9, sc.x) * (sc.x - AMBIENT) / s.coals.z * 0.04;
        let light = glow(sc.x) * sc.z + salt;
        let lum = dot(light, vec3(0.2126, 0.7152, 0.0722));
        e += vec4(light, lum);
        at += vec4(world(cell) * lum, 0.0);
    }
    power[li] = e;
    centre[li] = at;
    workgroupBarrier();
    for (var stride = 128u; stride > 0u; stride >>= 1u) {
        if li < stride {
            power[li] += power[li + stride];
            centre[li] += centre[li + stride];
        }
        workgroupBarrier();
    }
    if li == 0u {
        let index = wg.x + BRICKS.x * (wg.y + BRICKS.y * wg.z);
        let total = power[0];
        let mid = vec3<f32>(start + size / 2u);
        let pos = select(world(vec3<i32>(mid)), centre[0].xyz / max(total.w, 1e-9), total.w > 1e-9);
        // Each sample stands for 8 cells.
        let gain = s.coals.z * 8.0 * pow(s.grid.w, 3.0);
        buf[index * 2u] = vec4(pos, total.w * gain);
        buf[index * 2u + 1u] = vec4(total.rgb * gain, 0.0);
        if index == 0u {
            let n = BRICKS.x * BRICKS.y * BRICKS.z;
            let coal = glow(0.95 + 0.15 * s.coals.y) * s.coals.y * s.coals.z * 2e-4;
            for (var k = 0u; k < COAL_LIGHTS; k++) {
                let ang = f32(k) / f32(COAL_LIGHTS) * 6.2831853;
                let r = s.coals.x * 0.45;
                buf[(n + k) * 2u] = vec4(cos(ang) * r, floor_y(r) + 0.04, sin(ang) * r, dot(coal, vec3(0.2126, 0.7152, 0.0722)));
                buf[(n + k) * 2u + 1u] = vec4(coal, 0.0);
            }
        }
    }
}

// The camera's exposure, like a real one adapting to how bright the fire burns: the fire's
// total light against a reference (a natural fire's), partly made up for, and eased in over
// a second or so, so flicker shows but a fire the music keeps pumping doesn't burn out the
// picture. Kept after the lights, as one more light's position.x.
@compute @workgroup_size(128)
fn expose(@builtin(local_invocation_index) li: u32) {
    let count = BRICKS.x * BRICKS.y * BRICKS.z + COAL_LIGHTS;
    power[li] = vec4(select(0.0, buf[li * 2u].w, li < count));
    workgroupBarrier();
    for (var stride = 64u; stride > 0u; stride >>= 1u) {
        if li < stride {
            power[li] += power[li + stride];
        }
        workgroupBarrier();
    }
    if li == 0u {
        let aim = clamp(pow(s.expose.y / max(power[0].x, 1e-6), s.expose.z), 0.35, 1.3);
        let now = buf[count * 2u].x;
        let ease = 1.0 - exp(-s.expose.x / s.expose.w);
        buf[count * 2u] = vec4(mix(now, aim, ease), power[0].x, 0.0, 0.0);
    }
}

// ---- sparks ----
// Each spark is two vec4s: position and life left (s), then velocity and temperature (kK).
// Sparks ride the simulated air, so they swirl and loop with the flames, and cool as they go.

const SPARKS: u32 = 4096u;

@compute @workgroup_size(256)
fn sparks(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= SPARKS {
        return;
    }
    let dt = s.sparks.z;
    var pos = buf[i * 2u];
    var vel = buf[i * 2u + 1u];
    let first = u32(s.sparks.x);
    let count = u32(s.sparks.y);
    let born = (i + SPARKS - first) % SPARKS < count;
    if born {
        let seed = i * 9781u + u32(s.sparks.w) * 6271u;
        let ang = rand(seed) * 6.2831853;
        let r = sqrt(rand(seed + 1u)) * s.coals.x * 0.7;
        pos = vec4(cos(ang) * r, floor_y(r) + 0.04 + rand(seed + 2u) * 0.3, sin(ang) * r, 0.8 + rand(seed + 3u) * 1.8);
        let up = 1.2 + rand(seed + 4u) * 2.2;
        vel = vec4((rand(seed + 5u) - 0.5) * 1.2, up, (rand(seed + 6u) - 0.5) * 1.2, 1.55 + rand(seed + 7u) * 0.35);
    } else if pos.w > 0.0 {
        let air = select(vec3(0.0), textureSampleLevel(a, lin, uvw(pos.xyz), 0.0).xyz, all(uvw(pos.xyz) > vec3(0.0)) && all(uvw(pos.xyz) < vec3(1.0)));
        // Light sparks follow the air quickly; gravity barely matters for them.
        let follow = 1.0 - exp(-dt * 6.0);
        let jitter = vec3(rand(i * 31u + u32(s.sparks.w)) - 0.5, 0.0, rand(i * 17u + u32(s.sparks.w) + 3u) - 0.5) * 2.5;
        let v = vel.xyz + (air - vel.xyz) * follow + (vec3(0.0, -1.5, 0.0) + jitter) * dt;
        pos = vec4(pos.xyz + v * dt, pos.w - dt);
        let temp = vel.w * exp(-dt * 0.55);
        vel = vec4(v, temp);
        if temp < 0.85 || pos.y < floor_y(length(pos.xz)) {
            pos.w = 0.0;
        }
    }
    buf[i * 2u] = pos;
    buf[i * 2u + 1u] = vel;
}
