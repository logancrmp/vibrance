// Feedback visualizer.
//
// Each 60 Hz step warps the previous frame and draws shapes and scenes into it. The warp
// follows MilkDrop 2's per-vertex equations (zoom/zoomexp, rotation, the four-term animated
// warp, dx/dy), as documented in its BSD-licensed source and ported in Butterchurn (MIT).
//
// Every displayed frame then views the feedback in 3D (flat with a lens, a tunnel, or a
// corridor), lights it as a glossy surface whose height is its own brightness, adds bloom,
// and tone maps to the screen.

struct Uniforms {
    frame: vec4<f32>,   // res.x, res.y, time (s), aspect
    motion: vec4<f32>,  // zoom, rotation, swirl, decay (per 60 Hz step)
    warp: vec4<f32>,    // amount, scale, clock, blur
    sym: vec4<f32>,     // folds, mirror, dx, dy
    color: vec4<f32>,   // hue shift, flash, plasma, palette mix
    comp: vec4<f32>,    // echo alpha, echo zoom, chromatic aberration, echo orientation (0-3)
    center: vec4<f32>,  // centre x, centre y, bass, beat pulse
    shape: vec4<f32>,   // zoomexp, vortex, saddle, bias (subtract per step)
    fx: vec4<f32>,      // sharpen, warp chroma, mirror-wrap edges, darken centre
    gamma: vec4<f32>,   // per-channel gamma, limiter step (luminance per frame)
    safety: vec4<f32>,  // reduce flashing (0/1), sRGB target (0/1), output sharpening
    pal_a: vec4<f32>,
    pal_b: vec4<f32>,
    pal_c: vec4<f32>,
    pal_d: vec4<f32>,
    depth: vec4<f32>,   // tunnel weight, corridor weight, travel, fog
    view: vec4<f32>,    // twist, spin, vanishing point x, y
    light: vec4<f32>,   // relief, gloss, bloom, lens
    fire: vec4<f32>,    // how much of the campfire shows
}

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
// Limiter inputs. Each pass's bind group layout only declares the bindings it uses.
@group(0) @binding(3) var measured_tex: texture_2d<f32>;
@group(0) @binding(4) var limited_prev: texture_2d<f32>;
@group(0) @binding(5) var measured_map: texture_2d<f32>;
@group(0) @binding(6) var limited_map: texture_2d<f32>;
@group(0) @binding(8) var fire_tex: texture_2d<f32>;
@group(0) @binding(11) var fire_samp: sampler;

const PI: f32 = 3.14159265;
/// Mip levels below the full-size one (matches `gpu::MIPS`).
const MAX_LOD: f32 = 5.0;

struct FullOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> FullOut {
    var corners = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    let p = corners[i];
    var out: FullOut;
    out.pos = vec4(p, 0.0, 1.0);
    out.uv = vec2(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
    return out;
}

fn hue_rotate(c: vec3<f32>, angle: f32) -> vec3<f32> {
    let k = vec3(0.57735);
    let ca = cos(angle);
    return c * ca + cross(k, c) * sin(angle) + k * dot(k, c) * (1.0 - ca);
}

fn palette(t: f32) -> vec3<f32> {
    return u.pal_a.xyz + u.pal_b.xyz * cos(6.2831853 * (u.pal_c.xyz * t + u.pal_d.xyz));
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3(0.2126, 0.7152, 0.0722));
}

fn tap(uv: vec2<f32>) -> vec3<f32> {
    return textureSampleLevel(src, samp, uv, 0.0).rgb;
}

// Domain-warped sine plasma (after Inigo Quilez's domain warping), 0..1.
fn plasma(uv: vec2<f32>, t: f32) -> f32 {
    var p = (uv - vec2(0.5)) * vec2(u.frame.w, 1.0) * 3.0;
    for (var i = 0; i < 4; i++) {
        let fi = f32(i);
        p += vec2(sin(p.y * 1.7 + t * 0.21 + fi), cos(p.x * 1.3 - t * 0.17 + fi * 1.7)) * 0.55;
    }
    return 0.5 + 0.5 * sin(p.x + p.y);
}

// ---- noise ----

// PCG integer hash (Jarzynski & Olano, "Hash Functions for GPU Rendering").
fn hash_u(x: u32) -> u32 {
    let state = x * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn hash_f(p: vec2<u32>) -> f32 {
    return f32(hash_u(p.x ^ hash_u(p.y)) >> 8u) / 16777216.0;
}

// ---- feedback step ----

// Catmull-Rom bicubic from five bilinear taps (after Jimenez and MJP): keeps the feedback
// crisp where repeated bilinear resampling would blur it a little more every step.
fn sample_sharp(uv: vec2<f32>) -> vec3<f32> {
    let size = vec2<f32>(textureDimensions(src));
    let sp = uv * size - 0.5;
    let tc = floor(sp) + 0.5;
    let f = sp - floor(sp);
    let w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    let w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    let w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    let w3 = f * f * (-0.5 + 0.5 * f);
    let w12 = w1 + w2;
    let t0 = (tc - 1.0) / size;
    let t3 = (tc + 2.0) / size;
    let t12 = (tc + w2 / w12) / size;
    let c = tap(vec2(t12.x, t0.y)) * (w12.x * w0.y)
        + tap(vec2(t0.x, t12.y)) * (w0.x * w12.y)
        + tap(t12) * (w12.x * w12.y)
        + tap(vec2(t3.x, t12.y)) * (w3.x * w12.y)
        + tap(vec2(t12.x, t3.y)) * (w12.x * w3.y);
    let total = w12.x * w0.y + w0.x * w12.y + w12.x * w12.y + w3.x * w12.y + w12.x * w3.y;
    return max(c / total, vec3(0.0));
}

@fragment
fn fs_warp(in: FullOut) -> @location(0) vec4<f32> {
    let aspect = u.frame.w;
    let centre = u.center.xy;
    // Centred coordinates, y in [-1, 1], x scaled by aspect (MilkDrop's x, y).
    var p = (in.uv - vec2(0.5) - centre) * 2.0;
    p.x *= aspect;
    let r = length(p) * 0.5;
    var a = atan2(p.y, p.x);
    let folds = u.sym.x;
    if folds > 1.5 {
        let seg = 6.2831853 / folds;
        a = abs((a - seg * floor(a / seg)) - 0.5 * seg);
    }
    // Zoom with zoomexp: the rim moves faster than the centre when zoomexp > 1.
    let zoom = pow(u.motion.x, pow(u.shape.x, r * 2.0 - 1.0));
    a += u.motion.y + u.motion.z * (0.6 - r) + u.shape.y / (r + 0.2);
    p = vec2(cos(a), sin(a)) * length(p) / zoom;

    // Saddle field (Flexi): dx = k·x·y, dy = k/2·(x² − y²).
    p += u.shape.z * vec2(p.x * p.y, 0.5 * (p.x * p.x - p.y * p.y));

    // MilkDrop's animated warp.
    let t = u.warp.z;
    let s = 1.0 / max(u.warp.y, 0.05);
    let f0 = 11.68 + 4.0 * cos(1.413 * t + 10.0);
    let f1 = 8.77 + 3.0 * cos(1.113 * t + 7.0);
    let f2 = 10.54 + 3.0 * cos(1.233 * t + 3.0);
    let f3 = 11.49 + 4.0 * cos(0.933 * t + 5.0);
    let x = p.x * 0.5 / aspect;
    let y = p.y * 0.5;
    let amt = u.warp.x * 0.0035 * 2.0;
    p.x += amt * aspect * (sin(0.333 * t + s * (x * f0 - y * f3)) + cos(0.753 * t - s * (x * f1 - y * f2)));
    p.y += amt * (cos(0.375 * t - s * (x * f2 + y * f1)) + sin(0.825 * t + s * (x * f0 + y * f3)));

    p += u.sym.zw * 2.0;
    if u.sym.y > 0.5 {
        p.x = abs(p.x);
    }
    p.x /= aspect;
    let q = p * 0.5 + vec2(0.5) + centre;

    // Warp chroma: red and blue sample at slightly different zooms, fringing edges in rainbow.
    let cw = u.fx.y;
    let d = q - vec2(0.5);
    var c0: vec3<f32>;
    if cw > 0.0001 {
        c0 = vec3(sample_sharp(vec2(0.5) + d * (1.0 + cw)).r, sample_sharp(q).g, sample_sharp(vec2(0.5) + d * (1.0 - cw)).b);
    } else {
        c0 = sample_sharp(q);
    }
    // Blur and sharpening work at a fixed scale (1/900 of the height) at any resolution.
    let px = max(1.0 / u.frame.xy, vec2(1.0 / (900.0 * aspect), 1.0 / 900.0));
    let cb = (tap(q + vec2(px.x, 0.0)) + tap(q - vec2(px.x, 0.0)) + tap(q + vec2(0.0, px.y)) + tap(q - vec2(0.0, px.y))) * 0.25;
    // Blur softens; sharpening (an unsharp mask) grows reaction-diffusion patterns.
    var c = mix(c0, cb, u.warp.w) + (c0 - cb) * u.fx.x;
    c = hue_rotate(c, u.color.x) * u.motion.w - vec3(u.shape.w);

    // Outside the buffer: black, or mirrored when edges wrap (the sampler mirror-repeats).
    let inside = step(0.0, q.x) * step(q.x, 1.0) * step(0.0, q.y) * step(q.y, 1.0);
    c *= mix(inside, 1.0, u.fx.z);
    // A soft dark spot keeps the zoom centre from burning out (MilkDrop's darken_center).
    c *= 1.0 - u.fx.w * (1.0 - smoothstep(0.0, 0.06, r));
    let n = plasma(in.uv, u.frame.z);
    c += palette(n + u.frame.z * 0.02) * (u.color.z * (0.6 + 0.4 * u.center.z));

    // Cap at white like MilkDrop's 8-bit buffers, so additive layers can't pile up into a wash.
    return vec4(clamp(c, vec3(0.0), vec3(1.0)), 1.0);
}

// Shapes arrive in aspect space (x within ±aspect/2, y within ±0.5) so they fit any size.
struct ShapeIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
    // Position across the shape (-1..1) and softness: 0 for crisp lines, 1 for round glows.
    @location(2) edge: vec3<f32>,
}

struct ShapeOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) edge: vec3<f32>,
}

@vertex
fn vs_shape(v: ShapeIn) -> ShapeOut {
    var out: ShapeOut;
    out.pos = vec4(v.pos.x / (u.frame.w * 0.5), v.pos.y / 0.5, 0.0, 1.0);
    out.color = v.color;
    out.edge = v.edge;
    return out;
}

@fragment
fn fs_shape(v: ShapeOut) -> @location(0) vec4<f32> {
    let d = length(v.edge.xy);
    // A one-pixel anti-aliased rim, whatever the line's width.
    let crisp = clamp((1.0 - d) / max(fwidth(d), 1e-4), 0.0, 1.0);
    let soft = pow(max(1.0 - d * d, 0.0), 2.0);
    return vec4(v.color.rgb, v.color.a * mix(crisp, soft, v.edge.z));
}

// ---- mip chains: feedback level of detail, bloom ----

// 13-tap downsample (Jimenez, "Next Generation Post Processing in Call of Duty: AW").
@fragment
fn fs_down(in: FullOut) -> @location(0) vec4<f32> {
    let t = 1.0 / vec2<f32>(textureDimensions(src));
    let uv = in.uv;
    let a = tap(uv + t * vec2(-2.0, -2.0));
    let b = tap(uv + t * vec2(0.0, -2.0));
    let c = tap(uv + t * vec2(2.0, -2.0));
    let d = tap(uv + t * vec2(-2.0, 0.0));
    let e = tap(uv);
    let f = tap(uv + t * vec2(2.0, 0.0));
    let g = tap(uv + t * vec2(-2.0, 2.0));
    let h = tap(uv + t * vec2(0.0, 2.0));
    let i = tap(uv + t * vec2(2.0, 2.0));
    let j = tap(uv + t * vec2(-1.0, -1.0));
    let k = tap(uv + t * vec2(1.0, -1.0));
    let l = tap(uv + t * vec2(-1.0, 1.0));
    let m = tap(uv + t * vec2(1.0, 1.0));
    let sum = e * 0.125 + (a + c + g + i) * 0.03125 + (b + d + f + h) * 0.0625 + (j + k + l + m) * 0.125;
    return vec4(sum, 1.0);
}

// 3×3 tent upsample, added onto the next larger level.
@fragment
fn fs_up(in: FullOut) -> @location(0) vec4<f32> {
    let t = 1.0 / vec2<f32>(textureDimensions(src));
    let uv = in.uv;
    var sum = tap(uv) * 4.0;
    sum += (tap(uv + vec2(t.x, 0.0)) + tap(uv - vec2(t.x, 0.0)) + tap(uv + vec2(0.0, t.y)) + tap(uv - vec2(0.0, t.y))) * 2.0;
    sum += tap(uv + t) + tap(uv - t) + tap(uv + vec2(t.x, -t.y)) + tap(uv + vec2(-t.x, t.y));
    return vec4(sum / 16.0, 0.0);
}

// ---- 3D view of the feedback ----

fn base_color(tuv: vec2<f32>, lod: f32) -> vec3<f32> {
    let c = textureSampleLevel(src, samp, tuv, lod).rgb;
    let lum = luma(min(c, vec3(1.0)));
    return mix(c, palette(lum * 0.9 + u.frame.z * 0.012) * min(lum * 1.7, 1.2), u.color.w);
}

// The feedback as a lit, glossy surface whose height is its own brightness.
fn surface(tuv: vec2<f32>, lod: f32) -> vec3<f32> {
    let c = base_color(tuv, lod);
    let relief = u.light.x;
    if relief <= 0.001 {
        return c;
    }
    let size = vec2<f32>(textureDimensions(src));
    let e = max(1.0 / size, vec2(1.0 / (900.0 * u.frame.w), 1.0 / 900.0)) * exp2(lod);
    let hx = luma(textureSampleLevel(src, samp, tuv + vec2(e.x, 0.0), lod).rgb) - luma(textureSampleLevel(src, samp, tuv - vec2(e.x, 0.0), lod).rgb);
    let hy = luma(textureSampleLevel(src, samp, tuv + vec2(0.0, e.y), lod).rgb) - luma(textureSampleLevel(src, samp, tuv - vec2(0.0, e.y), lod).rgb);
    let n = normalize(vec3(-hx, -hy, 0.35 / relief));
    // Light from the upper left (uv's y points down), viewer straight ahead.
    let l = normalize(vec3(-0.5, -0.6, 0.62));
    let diffuse = max(dot(n, l), 0.0) / l.z;
    let h = normalize(l + vec3(0.0, 0.0, 1.0));
    let spec = pow(max(dot(n, h), 0.0), 60.0);
    return c * mix(1.0, diffuse, min(relief, 1.0)) + spec * u.light.y * (0.15 + 0.85 * c);
}

fn plane_view(uv: vec2<f32>) -> vec3<f32> {
    let aspect = u.frame.w;
    let d = uv - vec2(0.5);
    let r2 = dot(d * vec2(aspect, 1.0), d * vec2(aspect, 1.0)) * 4.0;
    // Lens: the centre shows more, smaller, so the outward flow seems to come from far away.
    let k = 1.0 + u.light.w * (1.0 / (1.0 + 2.0 * r2) - 1.0 / 3.0);
    var c = surface(vec2(0.5) + d * k, 0.0);
    // Video echo: a zoomed, optionally flipped copy (MilkDrop's echo_zoom / echo_orient).
    if u.comp.x > 0.001 {
        let orient = u32(u.comp.w + 0.5);
        var flip = vec2(1.0, 1.0);
        if (orient & 1u) != 0u {
            flip.x = -1.0;
        }
        if (orient & 2u) != 0u {
            flip.y = -1.0;
        }
        c = mix(c, surface(d * k / u.comp.y * flip + vec2(0.5), 0.0), u.comp.x);
    }
    return c;
}

// Far away fades into a haze of the picture's own average colour.
fn haze(c: vec3<f32>, depth: f32, near: f32) -> vec3<f32> {
    let fog = exp(-max(depth - near, 0.0) * u.depth.w);
    let far = textureSampleLevel(src, samp, vec2(0.5), MAX_LOD).rgb * 0.5;
    return mix(far, c, fog);
}

// Flying down a tube lined with the feedback.
fn tunnel_view(uv: vec2<f32>) -> vec3<f32> {
    let aspect = u.frame.w;
    let p = (uv - vec2(0.5)) * vec2(aspect, 1.0) - u.view.zw;
    let r = max(length(p), 1e-4);
    let depth = 0.3 / r;
    let a = atan2(p.y, p.x) + u.view.x * depth + u.view.y;
    // Around the tube the texture runs -1..1, which the mirroring sampler joins seamlessly.
    let tuv = vec2(a / PI, depth * 1.2 + u.depth.z);
    // Texels per pixel picks the mip level: the far end is tiny and would shimmer.
    let fu = (1.0 + u.view.x * 0.3 / r) / (PI * r) * aspect;
    let fv = 0.36 / (r * r);
    let lod = clamp(log2(max(fu, fv)), 0.0, MAX_LOD);
    return haze(surface(tuv, lod), depth, 0.6);
}

// Flying between a floor and a ceiling lined with the feedback.
fn corridor_view(uv: vec2<f32>) -> vec3<f32> {
    let aspect = u.frame.w;
    var p = (uv - vec2(0.5)) * vec2(aspect, 1.0) - u.view.zw;
    let roll = 0.12 * sin(u.view.y);
    p = vec2(p.x * cos(roll) - p.y * sin(roll), p.x * sin(roll) + p.y * cos(roll));
    let y = max(abs(p.y), 1e-3);
    let depth = 0.18 / y;
    let side = select(1.0, -1.0, p.y < 0.0);
    let tuv = vec2(0.5 + p.x * depth * 0.8 * side, depth * 0.9 + u.depth.z);
    let fu = 0.8 * depth * aspect;
    let fv = 0.162 / (y * y);
    let lod = clamp(log2(max(fu, fv)), 0.0, MAX_LOD);
    return haze(surface(tuv, lod), depth, 0.4);
}

@fragment
fn fs_scene(in: FullOut) -> @location(0) vec4<f32> {
    let tunnel = u.depth.x;
    let corridor = u.depth.y;
    let plane = max(1.0 - tunnel - corridor, 0.0);
    var c = vec3(0.0);
    if plane > 0.001 {
        c += plane * plane_view(in.uv);
    }
    if tunnel > 0.001 {
        c += tunnel * tunnel_view(in.uv);
    }
    if corridor > 0.001 {
        c += corridor * corridor_view(in.uv);
    }
    // The campfire is its own 3D scene (campfire/), rendered at this size.
    if u.fire.x > 0.001 {
        c = mix(c, textureSampleLevel(fire_tex, fire_samp, in.uv, 0.0).rgb, u.fire.x);
    }
    return vec4(c, 1.0);
}

// ---- to the screen ----

// A filmic curve: a toe keeps blacks deep, mid-tones lift, and highlights roll off softly
// instead of clipping flat. Then the preset's gamma and flash.
fn display(c: vec3<f32>) -> vec3<f32> {
    var x = max(c, vec3(0.0));
    let k = 0.7;
    x = select(x, k + (1.0 - k) * (1.0 - exp(-(x - k) / (1.0 - k))), x > vec3(k));
    x = pow(x * x / (x + 0.015), vec3(0.8));
    x = max(mix(vec3(luma(x)), x, 1.12), vec3(0.0));
    x = pow(x, max(u.gamma.xyz, vec3(0.05)));
    return x + vec3(u.color.y * 0.05);
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3(2.4)), c / 12.92, c <= vec3(0.04045));
}

// ---- photosensitivity limiter (WCAG 2.3.1) ----
// A 3×3 grid of tile luminances is measured from the scene's smallest mip; each tile's
// shown luminance may only move a fixed amount per second (`fs_limit`), and the final
// pass applies the resulting gain map.

@fragment
fn fs_measure(in: FullOut) -> @location(0) vec4<f32> {
    let tile = floor(in.pos.xy);
    var total = 0.0;
    for (var j = 0; j < 6; j++) {
        for (var i = 0; i < 6; i++) {
            let uv = (tile + (vec2(f32(i), f32(j)) + 0.5) / 6.0) / 3.0;
            total += luma(display(textureSampleLevel(src, samp, uv, MAX_LOD).rgb));
        }
    }
    return vec4(total / 36.0, 0.0, 0.0, 1.0);
}

fn finite(x: f32) -> bool {
    return (bitcast<u32>(x) & 0x7f800000u) != 0x7f800000u;
}

@fragment
fn fs_limit(in: FullOut) -> @location(0) vec4<f32> {
    let tile = vec2<i32>(floor(in.pos.xy));
    let actual = textureLoad(measured_tex, tile, 0).r;
    let prev = textureLoad(limited_prev, tile, 0);
    if prev.g < 0.5 {
        return vec4(actual, 1.0, 0.0, 1.0);
    }
    // A bad sample (NaN or infinity) must not stick: the running value would never recover.
    if !finite(actual) {
        return prev;
    }
    if !finite(prev.r) {
        return vec4(actual, 1.0, 0.0, 1.0);
    }
    let step = u.gamma.w;
    return vec4(prev.r + clamp(actual - prev.r, -step, step), 1.0, 0.0, 1.0);
}

@fragment
fn fs_final(in: FullOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    let dir = uv - vec2(0.5);
    let ca = u.comp.z;
    var c = vec3(tap(uv + dir * ca).r, tap(uv).g, tap(uv - dir * ca).b);
    // Light sharpening restores edge contrast lost to resampling.
    let t = 1.0 / vec2<f32>(textureDimensions(src));
    let around = (tap(uv + vec2(t.x, 0.0)) + tap(uv - vec2(t.x, 0.0)) + tap(uv + vec2(0.0, t.y)) + tap(uv - vec2(0.0, t.y))) * 0.25;
    c = max(c + (c - around) * u.safety.z, vec3(0.0));
    // Bloom only adds light: bright areas glow into dark ones without losing their detail.
    let bloom = textureSampleLevel(src, samp, uv, 1.0).rgb * 0.2;
    c += max(bloom - c, vec3(0.0)) * u.light.z;
    c = display(c);

    // Gain map from the limiter: never brighten more than 2×, never dim below a quarter.
    let measured = textureSampleLevel(measured_map, samp, uv, 0.0).r;
    let limited = textureSampleLevel(limited_map, samp, uv, 0.0).r;
    var gain = clamp((limited + 0.03) / (measured + 0.03), 0.25, 2.0);
    if !finite(gain) {
        gain = 1.0;
    }
    c *= mix(1.0, gain, u.safety.x);

    let v = dir * vec2(u.frame.w * 0.6, 1.0);
    c *= 1.0 - 0.3 * dot(v, v);
    // Triangular dither hides banding in dark gradients.
    let pix = vec2<u32>(in.pos.xy);
    c += (hash_f(pix) + hash_f(pix + vec2(7919u, 104729u)) - 1.0) / 255.0;
    c = clamp(c, vec3(0.0), vec3(1.0));
    // The values above are display values; an sRGB target expects linear light.
    if u.safety.y > 0.5 {
        c = srgb_to_linear(c);
    }
    return vec4(c, 1.0);
}
