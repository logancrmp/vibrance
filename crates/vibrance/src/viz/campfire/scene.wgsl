// Campfire: rendering the scene (SPEC 4.4). A night sky (an HDR photo), photo-scanned stones,
// logs and ground in physically based shading, lit by the fire itself (the simulation's glow
// summed into point lights, with soft shadows from a cube map), and the flames as a volume:
// ray-marched glowing soot (Kirchhoff: it emits as much as it absorbs, by Planck's law at its
// temperature), the blue of the burning fuel, salt colours, and firelit smoke.

struct View {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    // The shadow map's six faces around the fire, as view-projections.
    faces: array<mat4x4<f32>, 6>,
    eye: vec4<f32>,       // camera position; exposure (before the camera adapts it)
    sim_origin: vec4<f32>, // simulation grid's min corner; the detail grid's cell size (m)
    sim_size: vec4<f32>,  // simulation extent (m); light count
    shadow: vec4<f32>,    // shadow light position; its radius (m)
    sky: vec4<f32>,       // sky brightness, ambient, yaw (turns), time (s)
    flame: vec4<f32>,     // soot absorption (1/m), salts, blue glow, coal glow
    salt_a: vec4<f32>,
    salt_b: vec4<f32>,
    screen: vec4<f32>,    // width, height, frame, smoke scattering
    shade_near: vec4<f32>, // the fine baked-shadow grid's min corner; voxel size (m)
    shade_far: vec4<f32>,  // the coarse one's
}

struct Light {
    pos: vec4<f32>,   // position, luminance
    color: vec4<f32>, // radiant colour
}

@group(0) @binding(0) var<uniform> view: View;
// The fire's lights, then the camera's adapted exposure (sim.wgsl's `expose`).
@group(0) @binding(1) var<storage, read> lights: array<Light>;
@group(0) @binding(2) var gas: texture_3d<f32>;
@group(0) @binding(3) var blackbody_lut: texture_3d<f32>;
@group(0) @binding(4) var lin: sampler;
@group(0) @binding(5) var shadow_map: texture_depth_2d_array;
@group(0) @binding(6) var shadow_cmp: sampler_comparison;
@group(0) @binding(7) var sky_tex: texture_2d<f32>;
@group(0) @binding(8) var tex_samp: sampler;
@group(0) @binding(9) var<storage, read> spark_buf: array<vec4<f32>>;
// Per 8³ block of the gas grid, 1 if anything in it shows (sim.wgsl's detail passes).
@group(0) @binding(10) var<storage, read> occupied: array<u32>;
// How much of the fire's light reaches each point, baked (bake_shadows): fine around the pit,
// coarse out past the big log.
@group(0) @binding(11) var shade_near: texture_3d<f32>;
@group(0) @binding(12) var shade_far: texture_3d<f32>;

const PI: f32 = 3.14159265;
const AMBIENT: f32 = 0.29;

fn hash_u(x: u32) -> u32 {
    let state = x * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn rand2(p: vec2<u32>, k: u32) -> f32 {
    return f32(hash_u(p.x ^ hash_u(p.y ^ hash_u(k))) >> 8u) / 16777216.0;
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
    let x00 = mix(lattice(i, f), lattice(i + vec3(1, 0, 0), f - vec3(1.0, 0.0, 0.0)), w.x);
    let x10 = mix(lattice(i + vec3(0, 1, 0), f - vec3(0.0, 1.0, 0.0)), lattice(i + vec3(1, 1, 0), f - vec3(1.0, 1.0, 0.0)), w.x);
    let x01 = mix(lattice(i + vec3(0, 0, 1), f - vec3(0.0, 0.0, 1.0)), lattice(i + vec3(1, 0, 1), f - vec3(1.0, 0.0, 1.0)), w.x);
    let x11 = mix(lattice(i + vec3(0, 1, 1), f - vec3(0.0, 1.0, 1.0)), lattice(i + vec3(1, 1, 1), f - vec3(1.0, 1.0, 1.0)), w.x);
    return mix(mix(x00, x10, w.y), mix(x01, x11, w.y), w.z);
}

fn cell_point(c: vec2<i32>) -> vec3<f32> {
    let q = bitcast<vec2<u32>>(c);
    let h = hash_u(q.x ^ hash_u(q.y ^ 91u));
    return vec3(f32(h & 0xffffu), f32(h >> 16u), f32(hash_u(h) & 0xffffu)) / 65535.0;
}

// Cellular noise: distance to the nearest and second-nearest points, and the nearest's id.
fn voronoi(p: vec2<f32>) -> vec3<f32> {
    let i = vec2<i32>(floor(p));
    let f = fract(p);
    var d1 = 9.0;
    var d2 = 9.0;
    var id = 0.0;
    for (var y = -1; y <= 1; y++) {
        for (var x = -1; x <= 1; x++) {
            let o = vec2(x, y);
            let k = cell_point(i + o);
            let r = vec2<f32>(o) + k.xy - f;
            let dd = dot(r, r);
            if dd < d1 {
                d2 = d1;
                d1 = dd;
                id = k.z;
            } else if dd < d2 {
                d2 = dd;
            }
        }
    }
    return vec3(sqrt(d1), sqrt(d2), id);
}

fn exposure() -> f32 {
    return view.eye.w * lights[u32(view.sim_size.w)].pos.x;
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3(0.2126, 0.7152, 0.0722));
}

// Blackbody radiance at a temperature in kK (1500 K has luminance 1).
fn blackbody(temp: f32) -> vec3<f32> {
    let u = clamp((temp * 1000.0 - 500.0) / 2100.0, 0.0, 1.0);
    return textureSampleLevel(blackbody_lut, lin, vec3(u, 0.5, 0.5), 0.0).rgb;
}

fn sim_uvw(p: vec3<f32>) -> vec3<f32> {
    return (p - view.sim_origin.xyz) / view.sim_size.xyz;
}

fn gas_at(p: vec3<f32>) -> vec4<f32> {
    let q = sim_uvw(p);
    if any(q < vec3(0.0)) || any(q > vec3(1.0)) {
        return vec4(AMBIENT, 0.0, 0.0, 0.0);
    }
    return textureSampleLevel(gas, lin, q, 0.0);
}

// ---- the sky ----

struct FullOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) ndc: vec2<f32>,
}

@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> FullOut {
    var corners = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    var out: FullOut;
    out.pos = vec4(corners[i], 0.0, 1.0);
    out.ndc = corners[i];
    return out;
}

fn ray_dir(ndc: vec2<f32>) -> vec3<f32> {
    let far = view.inv_view_proj * vec4(ndc, 1.0, 1.0);
    return normalize(far.xyz / far.w - view.eye.xyz);
}

@fragment
fn fs_sky(in: FullOut) -> @location(0) vec4<f32> {
    let dir = ray_dir(in.ndc);
    let u = atan2(dir.z, dir.x) / (2.0 * PI) + 0.5 + view.sky.z;
    let v = acos(clamp(dir.y, -1.0, 1.0)) / PI;
    let c = textureSampleLevel(sky_tex, tex_samp, vec2(u, v), 0.0).rgb;
    // Below the horizon is our own ground, dark beyond the firelight, not the photo's.
    let above = smoothstep(-0.01, 0.03, dir.y);
    return vec4(c * view.sky.x * exposure() * above, 1.0);
}

// ---- meshes ----

struct Draw {
    model: mat4x4<f32>,
    params: vec4<f32>, // kind (0 the pit, 1 firewood, 2 ground), uv scale, char, seed
}

@group(1) @binding(0) var<uniform> draw: Draw;
@group(1) @binding(1) var albedo_tex: texture_2d<f32>;
@group(1) @binding(2) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var arm_tex: texture_2d<f32>;

struct MeshIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
}

struct MeshOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) local: vec3<f32>,
}

@vertex
fn vs_mesh(v: MeshIn) -> MeshOut {
    var out: MeshOut;
    let world = draw.model * vec4(v.pos, 1.0);
    out.world = world.xyz;
    out.normal = normalize((draw.model * vec4(v.normal, 0.0)).xyz);
    out.uv = v.uv * draw.params.y;
    out.local = v.pos;
    out.clip = view.view_proj * world;
    return out;
}

// GGX specular with Smith visibility and Schlick Fresnel.
fn specular(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, rough: f32) -> f32 {
    let h = normalize(v + l);
    let a = max(rough * rough, 0.002);
    let a2 = a * a;
    let nh = max(dot(n, h), 0.0);
    let nv = max(dot(n, v), 1e-3);
    let nl = max(dot(n, l), 1e-3);
    let dd = nh * nh * (a2 - 1.0) + 1.0;
    let ndf = a2 / (PI * dd * dd);
    let k = a * 0.5;
    let vis = 1.0 / ((nv * (1.0 - k) + k) * (nl * (1.0 - k) + k) * 4.0);
    let fresnel = 0.04 + 0.96 * pow(1.0 - max(dot(v, h), 0.0), 5.0);
    return ndf * vis * fresnel;
}

// One shadow-map test: is the world point `q` lit from the fire's centre?
fn shadow_tap(q: vec3<f32>) -> f32 {
    let d = q - view.shadow.xyz;
    let m = abs(d);
    var face = 0u;
    if m.x >= m.y && m.x >= m.z {
        face = select(1u, 0u, d.x > 0.0);
    } else if m.y >= m.z {
        face = select(3u, 2u, d.y > 0.0);
    } else {
        face = select(5u, 4u, d.z > 0.0);
    }
    let clip = view.faces[face] * vec4(q, 1.0);
    let ndc = clip.xyz / clip.w;
    let uv = vec2(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
    return textureSampleCompareLevel(shadow_map, shadow_cmp, uv, face, ndc.z - 0.00002);
}

// How much of the fire's light reaches `q`. The fire is no point: it's half a metre across,
// so shadows soften the farther they fall from what casts them (the taps, a Vogel disk, spread
// with distance from the light).
fn fire_visibility(q: vec3<f32>, taps: i32) -> f32 {
    let away = q - view.shadow.xyz;
    let dir = normalize(away);
    let t1 = normalize(cross(dir, select(vec3(0.0, 1.0, 0.0), vec3(1.0, 0.0, 0.0), abs(dir.y) > 0.9)));
    let t2 = cross(dir, t1);
    let spread = view.shadow.w * length(away) * 0.6;
    var sum = 0.0;
    for (var i = 0; i < taps; i++) {
        let a = f32(i) * 2.399963;
        let r = sqrt((f32(i) + 0.5) / f32(taps)) * spread;
        sum += shadow_tap(q + (t1 * cos(a) + t2 * sin(a)) * r);
    }
    return sum / f32(taps);
}

// ---- the fire's shadows, baked ----
// The point shadows are cast from and everything that casts them never move, so how much of
// the fire's light reaches any point never changes. It's baked once (when the meshes arrive)
// into two 3D grids, from far more taps than a pixel could afford, and the meshes read it
// with one filtered tap.

struct BakeBox {
    origin: vec4<f32>, // min corner (m); voxel size (m)
}

@group(1) @binding(0) var<uniform> bake_box: BakeBox;
@group(1) @binding(1) var bake_out: texture_storage_3d<rgba8unorm, write>;

@compute @workgroup_size(8, 8, 4)
fn bake_shadows(@builtin(global_invocation_id) gid: vec3<u32>) {
    if any(gid >= textureDimensions(bake_out)) {
        return;
    }
    let p = bake_box.origin.xyz + (vec3<f32>(gid) + 0.5) * bake_box.origin.w;
    // Moved toward the fire along its own ray, which changes nothing in the way, but lifts
    // voxels just inside a lit surface out of it so the surface doesn't shadow itself.
    let q = p + normalize(view.shadow.xyz - p) * bake_box.origin.w * 1.5;
    textureStore(bake_out, gid, vec4(fire_visibility(q, 64)));
}

fn baked_visibility(p: vec3<f32>) -> f32 {
    let near = (p - view.shade_near.xyz) / (vec3<f32>(textureDimensions(shade_near)) * view.shade_near.w);
    if all(near >= vec3(0.0)) && all(near <= vec3(1.0)) {
        return textureSampleLevel(shade_near, lin, near, 0.0).r;
    }
    let far = (p - view.shade_far.xyz) / (vec3<f32>(textureDimensions(shade_far)) * view.shade_far.w);
    if all(far >= vec3(0.0)) && all(far <= vec3(1.0)) {
        return textureSampleLevel(shade_far, lin, far, 0.0).r;
    }
    return 1.0;
}

fn decode_normal(raw: vec3<f32>, n: vec3<f32>, world: vec3<f32>, uv: vec2<f32>) -> vec3<f32> {
    // Cotangent frame from screen-space derivatives (Schüler), so meshes need no tangents.
    let dp1 = dpdx(world);
    let dp2 = dpdy(world);
    let duv1 = dpdx(uv);
    let duv2 = dpdy(uv);
    let dp2perp = cross(dp2, n);
    let dp1perp = cross(n, dp1);
    let t = dp2perp * duv1.x + dp1perp * duv2.x;
    let b = dp2perp * duv1.y + dp1perp * duv2.y;
    let inv = inverseSqrt(max(max(dot(t, t), dot(b, b)), 1e-20));
    // OpenGL normal maps: green points up the texture, which is -v in glTF's UVs.
    let m = raw * 2.0 - 1.0;
    return normalize(t * inv * m.x - b * inv * m.y + n * m.z);
}

@fragment
fn fs_mesh(in: MeshOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    let kind = u32(draw.params.x + 0.5);
    var albedo = textureSample(albedo_tex, tex_samp, in.uv).rgb;
    let arm = textureSample(arm_tex, tex_samp, in.uv).rgb;
    let raw_n = textureSample(normal_tex, tex_samp, in.uv).rgb;
    var geo_n = normalize(in.normal);
    if !front {
        geo_n = -geo_n;
    }
    let n = decode_normal(raw_n, geo_n, in.world, in.uv);
    var ao = arm.r;
    var rough = arm.g;
    var emit = vec3(0.0);
    let p = in.world;
    let t = view.sky.w;
    let r_pit = length(p.xz);

    if kind == 1u {
        // Charred firewood: blackened most where the fire is, crevices in the bark glowing
        // where the flames touch and low down near the coals; pale ash on the tops.
        let char = draw.params.z * (1.0 - smoothstep(0.35, 0.6, r_pit));
        let grain = noise(in.local * vec3(6.0, 30.0, 30.0) + draw.params.w);
        albedo *= mix(1.0, 0.08 + 0.06 * grain, char);
        rough = mix(rough, 0.9, char);
        let ash = smoothstep(0.55, 0.85, noise(p * 22.0 + draw.params.w) * 0.5 + 0.5 + n.y * 0.25) * char * smoothstep(0.2, 0.7, n.y);
        albedo = mix(albedo, vec3(0.32, 0.31, 0.3), ash);
        let crevice = pow(clamp(1.0 - arm.r, 0.0, 1.0), 1.5) + 0.35 * smoothstep(0.65, 0.95, noise(in.local * 40.0 + draw.params.w));
        let air = gas_at(p + n * 0.012).x;
        let near_coals = 1.0 - smoothstep(0.04, 0.3, p.y);
        let pulse = 0.75 + 0.25 * noise(vec3(p.xz * 8.0, t * 0.4));
        let heat = clamp(smoothstep(0.6, 1.3, air) + near_coals * view.flame.w * pulse * 0.8, 0.0, 1.0) * char;
        emit = blackbody(0.9 + 0.55 * heat) * clamp(crevice, 0.0, 1.0) * heat;
    } else if kind == 0u && r_pit < 0.48 && p.y < -0.06 {
        // The pit's floor is a bed of ash and coals: glowing chunks, hottest in the middle.
        let cells = voronoi(p.xz * 22.0);
        let chunk = 1.0 - smoothstep(0.05, 0.42, cells.x);
        let gap = smoothstep(0.02, 0.1, cells.y - cells.x);
        let breathe = 0.6 + 0.4 * noise(vec3(cells.z * 37.0, t * 0.5, 1.0));
        let centre = 1.0 - smoothstep(0.08, 0.5, r_pit);
        let heat = clamp(centre * breathe * view.flame.w, 0.0, 1.2);
        let bed = 1.0 - smoothstep(0.38, 0.48, r_pit);
        albedo = mix(albedo, vec3(0.05, 0.045, 0.04) + 0.25 * albedo * (1.0 - chunk), bed);
        emit = blackbody(0.75 + 0.45 * heat * chunk) * chunk * gap * heat * bed * 0.8;
    }

    let v = normalize(view.eye.xyz - p);
    let visible = baked_visibility(p);
    var light = vec3(0.0);
    let count = u32(view.sim_size.w);
    for (var i = 0u; i < count; i++) {
        let li = lights[i];
        if li.pos.w <= 0.0 {
            continue;
        }
        let d = li.pos.xyz - p;
        let dist2 = dot(d, d) + 0.01;
        let l = d * inverseSqrt(dist2);
        let nl = max(dot(n, l), 0.0);
        // Flames above the pit's rim see over the stones that shadow the low fire.
        let seen = mix(visible, 1.0, smoothstep(0.45, 0.7, li.pos.y));
        light += li.color.rgb * (albedo / PI + specular(n, v, l, rough)) * nl / dist2 * seen;
    }
    // Moonless night: a faint blue fill from the sky.
    let sky_fill = vec3(0.35, 0.45, 0.7) * view.sky.y * (0.5 + 0.5 * n.y);
    let c = light + albedo * sky_fill * ao + emit;
    return vec4(c * exposure(), 1.0);
}

// ---- the flames: a ray-marched volume ----

const BLOCK: u32 = 8u;

@group(1) @binding(0) var scene_depth: texture_depth_2d;

// Where a ray enters and leaves the simulation's box.
fn box_range(o: vec3<f32>, dir: vec3<f32>) -> vec2<f32> {
    let lo = view.sim_origin.xyz;
    let hi = lo + view.sim_size.xyz;
    let inv = 1.0 / dir;
    let t0 = (lo - o) * inv;
    let t1 = (hi - o) * inv;
    let near = max(max(min(t0.x, t1.x), min(t0.y, t1.y)), min(t0.z, t1.z));
    let far = min(min(max(t0.x, t1.x), max(t0.y, t1.y)), max(t0.z, t1.z));
    return vec2(max(near, 0.0), far);
}

// The fire's light at a point, from its summed lights (for smoke in-scattering).
fn firelight_at(p: vec3<f32>) -> vec3<f32> {
    var sum = vec3(0.0);
    let count = u32(view.sim_size.w);
    for (var i = 0u; i < count; i += 4u) {
        let li = lights[i];
        let d = li.pos.xyz - p;
        sum += li.color.rgb * 4.0 / (dot(d, d) + 0.02);
    }
    return sum / (4.0 * PI);
}

@fragment
fn fs_volume(in: FullOut) -> @location(0) vec4<f32> {
    let pixel = vec2<u32>(in.pos.xy);
    let dir = ray_dir(in.ndc);
    let o = view.eye.xyz;
    var range = box_range(o, dir);
    // Stop at whatever solid thing is in front.
    let depth = textureLoad(scene_depth, pixel, 0);
    if depth < 1.0 {
        let hit = view.inv_view_proj * vec4(in.ndc, depth, 1.0);
        range.y = min(range.y, distance(hit.xyz / hit.w, o));
    }
    if range.y <= range.x {
        return vec4(0.0, 0.0, 0.0, 1.0);
    }
    let step = view.sim_origin.w * 0.6;
    // Interleaved gradient noise (Jimenez) staggers where each pixel's steps fall, so the
    // steps don't band, with less grain than white noise.
    let ign = fract(52.9829189 * fract(dot(in.pos.xy + f32(u32(view.screen.z) % 64u) * 5.588238, vec2(0.06711056, 0.00583715))));
    var t = range.x + step * ign;
    var radiance = vec3(0.0);
    var through = 1.0;
    let blocks = textureDimensions(gas) / BLOCK;
    let block_size = view.sim_size.xyz / vec3<f32>(blocks);
    let inv_dir = 1.0 / dir;
    let ambient_smoke = firelight_at(o + dir * (range.x + range.y) * 0.5);
    loop {
        if t >= range.y || through < 0.004 {
            break;
        }
        let p = o + dir * t;
        let q = sim_uvw(p);
        let b = min(vec3<u32>(max(q, vec3(0.0)) * vec3<f32>(blocks)), blocks - 1u);
        if (occupied[b.x + blocks.x * (b.y + blocks.y * b.z)] & 1u) == 0u {
            // Nothing in this block: jump to where the ray leaves it, back onto this pixel's
            // staggered steps so block edges don't show.
            let lo = view.sim_origin.xyz + vec3<f32>(b) * block_size;
            let exits = max((lo - o) * inv_dir, (lo + block_size - o) * inv_dir);
            let leave = min(min(exits.x, exits.y), exits.z);
            t = max(range.x + step * (ign + ceil((leave - range.x) / step - ign)), t + step);
            continue;
        }
        let g = textureSampleLevel(gas, lin, q, 0.0);
        let temp = g.x;
        let fuel = g.y;
        let soot = g.z;
        // Hot soot glows; cooled soot is smoke, which absorbs and scatters firelight.
        let sigma = soot * view.flame.x;
        let absorbed = 1.0 - exp(-sigma * step);
        let hot = smoothstep(0.75, 1.0, temp);
        radiance += through * absorbed * blackbody(temp) * hot;
        // The blue of burning fuel at the base of flames, where there's little soot yet.
        let burning = fuel * smoothstep(0.9, 1.3, temp);
        radiance += through * step * burning * vec3(0.12, 0.25, 1.0) * view.flame.z;
        // Salts glow in the hot burned gas.
        let excited = smoothstep(1.3, 1.9, temp) * (temp - AMBIENT);
        radiance += through * step * excited * view.flame.y * mix(view.salt_a.rgb, view.salt_b.rgb, g.w);
        let cold = soot * (1.0 - hot);
        radiance += through * (1.0 - exp(-cold * view.screen.w * step)) * ambient_smoke;
        through *= exp(-(sigma + cold * view.screen.w) * step);
        t += step;
    }
    return vec4(radiance * exposure(), through);
}

// ---- each frame: the scene as last drawn, and the sparks over it ----
// The scene only changes with the simulation (60 times a second), so it's drawn into a cache
// then; sparks fly at the display's rate on top.

@group(1) @binding(0) var cached: texture_2d<f32>;

@fragment
fn fs_cached(in: FullOut) -> @location(0) vec4<f32> {
    return textureLoad(cached, vec2<i32>(in.pos.xy), 0);
}

// ---- sparks ----

struct SparkOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) across: f32,
}

@vertex
fn vs_spark(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> SparkOut {
    var out: SparkOut;
    let pos = spark_buf[ii * 2u];
    let vel = spark_buf[ii * 2u + 1u];
    if pos.w <= 0.0 {
        out.clip = vec4(2.0, 2.0, 2.0, 1.0);
        return out;
    }
    // A streak over a 1/60 s shutter, at least a pixel and a half wide.
    let head = view.view_proj * vec4(pos.xyz, 1.0);
    let tail = view.view_proj * vec4(pos.xyz - vel.xyz / 60.0, 1.0);
    let res = view.screen.xy;
    let hs = head.xy / head.w * res * 0.5;
    let ts = tail.xy / tail.w * res * 0.5;
    var along = hs - ts;
    if length(along) < 0.5 {
        along = vec2(0.0, 0.5);
    }
    let dir = normalize(along);
    let side = vec2(-dir.y, dir.x);
    let width = max(0.0012 * res.y / head.w, 1.5);
    var corners = array<vec2<f32>, 6>(vec2(0.0, -1.0), vec2(0.0, 1.0), vec2(1.0, -1.0), vec2(0.0, 1.0), vec2(1.0, 1.0), vec2(1.0, -1.0));
    let corner = corners[vi];
    let at = mix(ts - dir * width * 0.5, hs + dir * width * 0.5, corner.x) + side * width * corner.y;
    let w = mix(tail.w, head.w, corner.x);
    let z = mix(tail.z / tail.w, head.z / head.w, corner.x);
    out.clip = vec4(at / res * 2.0 * w, z * w, w);
    // Shorter streaks hold their light in less area, so they look brighter.
    let spread = max(length(hs - ts) / width, 1.0);
    out.color = blackbody(vel.w) * 3.0 / sqrt(spread);
    out.across = corner.y;
    return out;
}

@fragment
fn fs_spark(in: SparkOut) -> @location(0) vec4<f32> {
    let fade = 1.0 - in.across * in.across;
    return vec4(in.color * fade * exposure(), 0.0);
}
