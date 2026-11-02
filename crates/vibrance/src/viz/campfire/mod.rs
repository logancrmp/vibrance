//! The campfire scene: a 3D fire simulation burning in a photo-scanned stone fire
//! pit at night, rendered physically.
//!
//! - `sim.wgsl`: the fire, as a gas on a 3D grid (fuel, combustion, soot, buoyancy, vorticity).
//! - `scene.wgsl`: the night sky, the pit, logs and ground lit by the fire with soft shadows,
//!   the flames ray-marched as glowing soot, and sparks riding the simulated air.
//! - `assets`: the photo-scanned models and textures (CC0, Poly Haven).
//!
//! Everything is sized generously; the expensive knobs are the constants below.

mod assets;
pub mod spectrum;

use glam::{Mat4, Quat, Vec3};
use wgpu::util::DeviceExt;

use bytemuck::{Pod, Zeroable};

/// Simulation grid, in cells, and its box in the world (metres): the pit's bowl and the air
/// above it.
const GRID: [u32; 3] = [192, 288, 192];
const SIM_ORIGIN: [f32; 3] = [-0.6, -0.16, -0.6];
const SIM_SIZE: [f32; 3] = [1.2, 1.8, 1.2];
/// The flames are drawn from a grid twice as fine (3 mm cells), carried by the coarse flow
/// plus finer eddies (sim.wgsl's detail passes), and skipped through in 8³ blocks where empty.
const DETAIL: [u32; 3] = [GRID[0] * 2, GRID[1] * 2, GRID[2] * 2];
const BLOCK: u32 = 8;
/// The eddies' curl-noise tile, in texels (sim.wgsl's `EDDY_TILE`), how many tiles span a
/// metre (its eddies are a sixteenth of a tile: 5 cm, and half that an octave down), and how
/// strong they are against the coarse flow's spin. Stronger or finer eddies shred the flames
/// into a crumpled, mottled texture.
const EDDY_TILE: u32 = 128;
const EDDY_TILES_PER_M: f32 = 1.25;
const EDDY_STRENGTH: f32 = 0.25;
/// The pressure solve: multigrid levels (each half the last), V-cycles per step (one is
/// plenty, starting from the last step's pressure), and smoothing sweeps per level on the
/// way down and up (more on the coarsest).
const LEVELS: usize = 5;
const V_CYCLES: usize = 1;
const SMOOTH: usize = 2;
const COARSEST_SMOOTH: usize = 16;
const SHADOW_SIZE: u32 = 2048;
const SPARKS: u32 = 4096;
/// The fire's glow summed into bricks of the grid (sim.wgsl's `BRICKS`), plus the coals.
const LIGHTS: u32 = 4 * 6 * 4 + 8;
/// Where shadows are cast from: the middle of the fire, above the logs.
const SHADOW_LIGHT: [f32; 3] = [0.0, 0.42, 0.0];
/// The baked shadows' grids (scene.wgsl's `bake_shadows`): min corner, voxel size (m) and
/// voxels. A fine one around the pit, and a coarse one out past the big log's shadow.
const SHADE_NEAR: ([f32; 3], f32, [u32; 3]) = ([-0.672, -0.2, -0.672], 0.008, [168, 88, 168]);
const SHADE_FAR: ([f32; 3], f32, [u32; 3]) = ([-2.704, -0.2, -2.704], 0.026, [208, 28, 208]);
const SHADOW_NEAR: f32 = 0.02;
const SHADOW_FAR: f32 = 40.0;

const HDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The fire's total light that the camera's exposure is set for (a natural fire's, on
/// average); brighter fires are partly exposed down (sim.wgsl's `expose`).
const EXPOSURE_REFERENCE: f32 = 1.2;
/// How brightly salts glow in the flames, per metre of hot gas.
const SALT_GLOW: f32 = 25.0;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// What the engine decides each frame: the music, the flicker clock, and the camera.
#[derive(Debug, Clone, Copy, Default)]
pub struct Params {
    /// Flicker clock (s): drives the simulation's noise and patches.
    pub clock: f32,
    /// Simulation time per 60 Hz step (s); the music version scales it.
    pub sim_dt: f32,
    /// Display time since the last frame (s), for sparks.
    pub frame_dt: f32,
    /// Seconds since the scene started, for the camera's drift.
    pub time: f32,
    /// How fast fuel bleeds from the logs (1/s).
    pub fuel: f32,
    pub kick: f32,
    pub bands: [f32; 8],
    /// Salt colours in the flames: 0 keeps the fire fire-coloured.
    pub salts: f32,
    pub salt_a: [f32; 3],
    pub salt_b: [f32; 3],
    pub salt_drift: f32,
    /// Coal and ember glow (0..1, follows the bass).
    pub coals: f32,
    pub spark_first: u32,
    pub spark_count: u32,
    pub frame: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SimUniform {
    grid: [f32; 4],
    origin: [f32; 4],
    forces: [f32; 4],
    burn: [f32; 4],
    flow: [f32; 4],
    coals: [f32; 4],
    sparks: [f32; 4],
    salt_a: [f32; 4],
    salt_b: [f32; 4],
    detail: [f32; 4],
    subgrid: [f32; 4],
    expose: [f32; 4],
    bands: [[f32; 4]; 2],
    logs: [[f32; 4]; 24],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ViewUniform {
    view_proj: [[f32; 4]; 4],
    inv_view_proj: [[f32; 4]; 4],
    faces: [[[f32; 4]; 4]; 6],
    eye: [f32; 4],
    sim_origin: [f32; 4],
    sim_size: [f32; 4],
    shadow: [f32; 4],
    sky: [f32; 4],
    flame: [f32; 4],
    salt_a: [f32; 4],
    salt_b: [f32; 4],
    screen: [f32; 4],
    shade_near: [f32; 4],
    shade_far: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct DrawUniform {
    model: [[f32; 4]; 4],
    params: [f32; 4],
}

/// Which asset a placed object uses.
#[derive(Clone, Copy, PartialEq)]
enum Model {
    Pit,
    Log,
    BigLog,
}

struct Placed {
    model: Model,
    transform: Mat4,
    /// Charred (1) or fresh (0).
    char: f32,
    /// The log's axis and radius in the world, if the fire treats it as a solid that burns.
    capsule: Option<(Vec3, Vec3, f32)>,
}

/// The firewood: a log-cabin base of four logs on the pit's floor, a teepee of five thinner
/// pieces leaning over it, and a big log beside the pit to sit on.
fn layout() -> Vec<Placed> {
    // The scanned trunk is 3.05 m long along x, its axis about 0.12 m up and -0.03 m in z,
    // about 0.14 m in radius.
    let trunk_axis = (Vec3::new(-1.5, 0.117, -0.032), Vec3::new(1.5, 0.117, -0.032));
    let floor = |r: f32| -0.151 + 0.25 * r * r;
    let log = |centre: Vec3, dir: Vec3, roll: f32, scale: f32| {
        let rot = Quat::from_axis_angle(dir.normalize(), roll) * Quat::from_rotation_arc(Vec3::X, dir.normalize());
        let offset = Vec3::new(0.013, 0.117, -0.032);
        let transform = Mat4::from_scale_rotation_translation(Vec3::splat(scale), rot, centre) * Mat4::from_translation(-offset);
        let a = transform.transform_point3(trunk_axis.0);
        let b = transform.transform_point3(trunk_axis.1);
        Placed { model: Model::Log, transform, char: 1.0, capsule: Some((a, b, 0.13 * scale)) }
    };
    let r = 0.036;
    let mut out = vec![
        Placed { model: Model::Pit, transform: Mat4::from_translation(Vec3::new(0.0, 0.042, 0.0)), char: 0.0, capsule: None },
        log(Vec3::new(0.0, floor(0.17) + r, 0.17), Vec3::new(1.0, 0.0, 0.08), 0.4, 0.26),
        log(Vec3::new(0.02, floor(0.18) + r, -0.18), Vec3::new(1.0, 0.0, -0.05), 2.1, 0.26),
        log(Vec3::new(0.17, floor(0.18) + 3.0 * r, 0.0), Vec3::new(0.06, 0.0, 1.0), 1.3, 0.26),
        log(Vec3::new(-0.19, floor(0.19) + 3.0 * r, 0.01), Vec3::new(-0.04, 0.0, 1.0), 4.0, 0.26),
    ];
    for (i, angle) in [0.3f32, 1.55, 2.8, 4.0, 5.2].into_iter().enumerate() {
        let base = Vec3::new(0.3 * angle.cos(), floor(0.3) + 0.02, 0.3 * angle.sin());
        let apex = Vec3::new(0.03 * angle.cos(), 0.36, 0.03 * angle.sin());
        out.push(log((base + apex) * 0.5, apex - base, i as f32 * 1.7, 0.18));
    }
    let seat = Quat::from_rotation_y(0.5);
    out.push(Placed {
        model: Model::BigLog,
        transform: Mat4::from_scale_rotation_translation(Vec3::splat(0.22), seat, Vec3::new(-1.3, 0.072, 0.75)),
        char: 0.0,
        capsule: None,
    });
    out
}

struct Tex3 {
    #[cfg_attr(not(test), allow(dead_code))]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

fn tex3(device: &wgpu::Device, label: &str, size: [u32; 3], format: wgpu::TextureFormat) -> Tex3 {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: size[2] },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    Tex3 { view: texture.create_view(&wgpu::TextureViewDescriptor::default()), texture }
}

struct GpuMesh {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    count: u32,
}

struct DrawCall {
    mesh: usize,
    group: wgpu::BindGroup,
}

/// A named group of passes (named for GPU debuggers).
type Phase<'a> = (&'static str, Vec<SimPass<'a>>);

/// One pass of the simulation: which pipeline, its bindings, and the grid it covers.
struct SimPass<'a> {
    pipeline: &'a wgpu::ComputePipeline,
    group: &'a wgpu::BindGroup,
    size: [u32; 3],
}

fn level_size(level: usize) -> [u32; 3] {
    GRID.map(|n| (n >> level).max(1))
}

struct SimGroups {
    curl: [wgpu::BindGroup; 2],
    forces: [[wgpu::BindGroup; 2]; 2],
    divergence: [wgpu::BindGroup; 2],
    /// Per multigrid level: smoothing by which pressure is newest, the residual handed down,
    /// a cleared start, and the correction handed back up [fine][coarse].
    smooth: Vec<[wgpu::BindGroup; 2]>,
    restrict: Vec<[wgpu::BindGroup; 2]>,
    clear: Vec<wgpu::BindGroup>,
    prolong: Vec<[[wgpu::BindGroup; 2]; 2]>,
    project: [[wgpu::BindGroup; 2]; 2],
    advect_velocity: [wgpu::BindGroup; 2],
    correct_velocity: [wgpu::BindGroup; 2],
    advect_scalars: [[wgpu::BindGroup; 2]; 2],
    correct_scalars: [[wgpu::BindGroup; 2]; 2],
    /// By which coarse scalars are newest.
    detail_blocks: [wgpu::BindGroup; 2],
    /// [velocity][detail scalars]
    detail_advect: [[wgpu::BindGroup; 2]; 2],
    detail_correct: [[wgpu::BindGroup; 2]; 2],
    lights: [wgpu::BindGroup; 2],
    sparks: [wgpu::BindGroup; 2],
    bake_shape: wgpu::BindGroup,
}

struct SimPipelines {
    curl: wgpu::ComputePipeline,
    forces: wgpu::ComputePipeline,
    divergence: wgpu::ComputePipeline,
    jacobi: wgpu::ComputePipeline,
    restrict: wgpu::ComputePipeline,
    clear: wgpu::ComputePipeline,
    prolong: wgpu::ComputePipeline,
    project: wgpu::ComputePipeline,
    advect_velocity: wgpu::ComputePipeline,
    advect_scalars: wgpu::ComputePipeline,
    correct_velocity: wgpu::ComputePipeline,
    correct_scalars: wgpu::ComputePipeline,
    expose: wgpu::ComputePipeline,
    detail_blocks: wgpu::ComputePipeline,
    detail_advect: wgpu::ComputePipeline,
    detail_correct: wgpu::ComputePipeline,
    bake_shape: wgpu::ComputePipeline,
    lights: wgpu::ComputePipeline,
    sparks: wgpu::ComputePipeline,
}

struct Targets {
    size: (u32, u32),
    depth: wgpu::TextureView,
    depth_group: wgpu::BindGroup,
    /// The scene as last drawn (sky, meshes, flames), and its binding for each frame's copy.
    cache: wgpu::TextureView,
    cache_group: wgpu::BindGroup,
    /// Whether `cache` has been drawn since these were made.
    drawn: bool,
}

pub struct Campfire {
    sim_uniform: wgpu::Buffer,
    sim_pipelines: SimPipelines,
    sim_groups: SimGroups,
    /// Ping-pong indices: velocity, scalars, pressure.
    vel_i: usize,
    scal_i: usize,
    pres_i: usize,
    /// The logs' shape is baked on the first step (it needs the uniforms).
    baked: bool,
    /// The simulation has moved on since the scene was last drawn.
    unseen: bool,
    /// This frame draws the scene (decided in `prepare`).
    draw_scene: bool,
    /// The display's frame interval (s), smoothed, and the time since the scene was drawn.
    frame_interval: f32,
    since_drawn: f32,
    /// The light and the meshes never move, so the shadow map is drawn when they arrive.
    shadows_stale: bool,

    view_uniform: wgpu::Buffer,
    /// By which scalar texture is newest.
    view_groups: Vec<wgpu::BindGroup>,
    mesh_layout: wgpu::BindGroupLayout,
    depth_layout: wgpu::BindGroupLayout,
    cache_layout: wgpu::BindGroupLayout,
    sky_pipeline: wgpu::RenderPipeline,
    cache_pipeline: wgpu::RenderPipeline,
    mesh_pipeline: wgpu::RenderPipeline,
    spark_pipeline: wgpu::RenderPipeline,
    volume_pipeline: wgpu::RenderPipeline,
    shadow_pipeline: wgpu::RenderPipeline,
    shadow_faces: Vec<(wgpu::TextureView, wgpu::BindGroup)>,
    /// Baking the shadows: the pipeline, and its bindings for the fine and coarse grids.
    bake_pipeline: wgpu::ComputePipeline,
    bake_groups: Vec<(wgpu::BindGroup, wgpu::BindGroup, [u32; 3])>,
    shade: [Tex3; 2],
    view_layout: wgpu::BindGroupLayout,
    lights: wgpu::Buffer,
    sparks: wgpu::Buffer,
    #[cfg_attr(not(test), allow(dead_code))]
    gas: [Tex3; 2],
    #[cfg_attr(not(test), allow(dead_code))]
    vel: [Tex3; 2],
    /// The flames as drawn, and which of the detail grid's blocks have anything in them.
    detail: [Tex3; 2],
    occupied: wgpu::Buffer,
    lut: Tex3,
    linear: wgpu::Sampler,
    shadow_cmp: wgpu::Sampler,
    shadow_array: wgpu::TextureView,
    textured: wgpu::Sampler,
    sky: wgpu::TextureView,

    meshes: Vec<GpuMesh>,
    draws: Vec<DrawCall>,
    placed: Vec<Placed>,
    loader: Option<assets::Loader>,
    targets: Option<Targets>,
    view: ViewUniform,
    /// GPU timestamps around each stage of `render` (`STAGES`), when profiling.
    timestamps: Option<wgpu::QuerySet>,
}

/// `render`'s stages, as profiled.
#[cfg_attr(not(test), allow(dead_code))]
const STAGES: [&str; 6] = ["lights", "shadow map (once)", "sky + meshes", "flames", "every frame: copy + sparks", "shadow bake (once)"];

fn layout_entry(binding: u32, visibility: wgpu::ShaderStages, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { binding, visibility, ty, count: None }
}

fn texture_binding(dimension: wgpu::TextureViewDimension, filterable: bool) -> wgpu::BindingType {
    wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable }, view_dimension: dimension, multisampled: false }
}

fn storage_texture(format: wgpu::TextureFormat) -> wgpu::BindingType {
    wgpu::BindingType::StorageTexture { access: wgpu::StorageTextureAccess::WriteOnly, format, view_dimension: wgpu::TextureViewDimension::D3 }
}

fn storage_buffer(read_only: bool) -> wgpu::BindingType {
    wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None }
}

fn uniform_binding() -> wgpu::BindingType {
    wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }
}

fn view_entry(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource: wgpu::BindingResource::TextureView(view) }
}

fn sampler_entry(binding: u32, sampler: &wgpu::Sampler) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource: wgpu::BindingResource::Sampler(sampler) }
}

fn mat(m: Mat4) -> [[f32; 4]; 4] {
    m.to_cols_array_2d()
}

/// Upload an RGBA8 image and its mips.
fn texture_2d(device: &wgpu::Device, queue: &wgpu::Queue, label: &str, tex: &assets::Texture, srgb: bool) -> wgpu::TextureView {
    let format = if srgb { wgpu::TextureFormat::Rgba8UnormSrgb } else { wgpu::TextureFormat::Rgba8Unorm };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: tex.size.0, height: tex.size.1, depth_or_array_layers: 1 },
        mip_level_count: tex.levels.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (level, data) in tex.levels.iter().enumerate() {
        let (w, h) = ((tex.size.0 >> level).max(1), (tex.size.1 >> level).max(1));
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: level as u32, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * w), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

fn solid_texture(device: &wgpu::Device, queue: &wgpu::Queue, rgba: [u8; 4], srgb: bool) -> wgpu::TextureView {
    texture_2d(device, queue, "campfire plain", &assets::Texture { size: (1, 1), levels: vec![rgba.to_vec()] }, srgb)
}

/// A ring of ground around the pit, out into the dark; its UVs are world x and z.
fn ground_mesh() -> (Vec<assets::Vertex>, Vec<u32>) {
    let (rings, spokes) = (64u32, 192u32);
    let (inner, outer) = (0.6f32, 40.0f32);
    let mut vertices = Vec::new();
    for i in 0..=rings {
        let r = inner * (outer / inner).powf(i as f32 / rings as f32);
        for j in 0..=spokes {
            let a = j as f32 / spokes as f32 * std::f32::consts::TAU;
            let (x, z) = (r * a.cos(), r * a.sin());
            vertices.push(assets::Vertex { pos: [x, 0.0, z], normal: [0.0, 1.0, 0.0], uv: [x, z] });
        }
    }
    let mut indices = Vec::new();
    let row = spokes + 1;
    for i in 0..rings {
        for j in 0..spokes {
            let (a, b, c, d) = (i * row + j, i * row + j + 1, (i + 1) * row + j, (i + 1) * row + j + 1);
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
    (vertices, indices)
}

impl Campfire {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Campfire {
        let sim_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("campfire sim"),
            source: wgpu::ShaderSource::Wgsl(include_str!("sim.wgsl").into()),
        });
        let scene_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("campfire scene"),
            source: wgpu::ShaderSource::Wgsl(include_str!("scene.wgsl").into()),
        });
        let shadow_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("campfire shadow"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shadow.wgsl").into()),
        });
        use wgpu::ShaderStages as S;
        use wgpu::TextureViewDimension as D;

        // ---- simulation ----
        let c = S::COMPUTE;
        let sim_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("campfire sim"),
            entries: &[
                layout_entry(0, c, uniform_binding()),
                layout_entry(1, c, texture_binding(D::D3, true)),
                layout_entry(2, c, texture_binding(D::D3, true)),
                layout_entry(3, c, texture_binding(D::D3, false)),
                layout_entry(4, c, texture_binding(D::D3, false)),
                layout_entry(5, c, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
                layout_entry(6, c, storage_texture(HDR)),
                layout_entry(7, c, storage_texture(wgpu::TextureFormat::R32Float)),
                layout_entry(8, c, storage_texture(wgpu::TextureFormat::Rg32Float)),
                layout_entry(9, c, storage_buffer(false)),
                layout_entry(10, c, texture_binding(D::D3, true)),
                layout_entry(11, c, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
                layout_entry(12, c, storage_buffer(false)),
                layout_entry(13, c, texture_binding(D::D3, true)),
            ],
        });
        let sim_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[&sim_layout], push_constant_ranges: &[] });
        let compute = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&sim_pl),
                module: &sim_module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let sim_pipelines = SimPipelines {
            curl: compute("curl"),
            forces: compute("forces"),
            divergence: compute("divergence"),
            jacobi: compute("jacobi"),
            restrict: compute("coarsen"),
            clear: compute("clear"),
            prolong: compute("prolong"),
            project: compute("project"),
            advect_velocity: compute("advect_velocity"),
            advect_scalars: compute("advect_scalars"),
            correct_velocity: compute("correct_velocity"),
            correct_scalars: compute("correct_scalars"),
            expose: compute("expose"),
            detail_blocks: compute("detail_blocks"),
            detail_advect: compute("detail_advect"),
            detail_correct: compute("detail_correct"),
            bake_shape: compute("bake_shape"),
            lights: compute("lights"),
            sparks: compute("sparks"),
        };

        let vel = [tex3(device, "fire velocity a", GRID, HDR), tex3(device, "fire velocity b", GRID, HDR)];
        let gas = [tex3(device, "fire scalars a", GRID, HDR), tex3(device, "fire scalars b", GRID, HDR)];
        let tmp = tex3(device, "fire advected", GRID, HDR);
        let curl = tex3(device, "fire curl", GRID, HDR);
        let shape = tex3(device, "fire shape", GRID, HDR);
        let detail = [tex3(device, "fire detail a", DETAIL, HDR), tex3(device, "fire detail b", DETAIL, HDR)];
        let detail_tmp = tex3(device, "fire detail advected", DETAIL, HDR);
        let eddies = tex3(device, "fire eddies", [EDDY_TILE; 3], HDR);
        let potential = tex3(device, "fire eddy potential", [EDDY_TILE; 3], HDR);
        let blocks = DETAIL.map(|n| n / BLOCK);
        let occupied = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fire occupied blocks"),
            size: (blocks[0] * blocks[1] * blocks[2]) as u64 * 4,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        // Pressure and right-hand side per multigrid level; level 0's right-hand side is the
        // divergence (with the logs marked).
        let pressures: Vec<[Tex3; 2]> = (0..LEVELS)
            .map(|l| [0, 1].map(|k| tex3(device, &format!("fire pressure {l}{k}"), level_size(l), wgpu::TextureFormat::R32Float)))
            .collect();
        let rhs: Vec<Tex3> = (0..LEVELS).map(|l| tex3(device, &format!("fire rhs {l}"), level_size(l), wgpu::TextureFormat::Rg32Float)).collect();
        let (pressure, divergence) = (&pressures[0], &rhs[0]);
        // Bindings a pass doesn't use get placeholders: separate ones for reading and for
        // writing, since a texture can't be both in one pass.
        let out4 = tex3(device, "fire spare out", [1, 1, 1], HDR);
        let out1 = tex3(device, "fire spare out r", [1, 1, 1], wgpu::TextureFormat::R32Float);
        let out2 = tex3(device, "fire spare out rg", [1, 1, 1], wgpu::TextureFormat::Rg32Float);
        let dummy4b = tex3(device, "fire spare in", [1, 1, 1], HDR);
        let dummy1 = tex3(device, "fire spare in r", [1, 1, 1], wgpu::TextureFormat::R32Float);
        let dummy2 = tex3(device, "fire spare in rg", [1, 1, 1], wgpu::TextureFormat::Rg32Float);
        let lut = {
            let table: Vec<half::f16> = spectrum::blackbody_table().into_iter().flatten().map(half::f16::from_f32).collect();
            let texture = device.create_texture_with_data(
                queue,
                &wgpu::TextureDescriptor {
                    label: Some("blackbody"),
                    size: wgpu::Extent3d { width: spectrum::LUT_SIZE as u32, height: 1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D3,
                    format: HDR,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                bytemuck::cast_slice(&table),
            );
            Tex3 { view: texture.create_view(&wgpu::TextureViewDescriptor::default()), texture }
        };
        let linear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("campfire linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let wrap = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("campfire eddies"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let sim_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("campfire sim"),
            size: std::mem::size_of::<SimUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // The lights, then the camera's exposure (starting at 1).
        let mut lights_init = vec![0.0f32; (LIGHTS as usize + 1) * 8];
        lights_init[LIGHTS as usize * 8] = 1.0;
        let lights = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("campfire lights"),
            contents: bytemuck::cast_slice(&lights_init),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        });
        let sparks = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("campfire sparks"),
            contents: &vec![0u8; SPARKS as usize * 32],
            usage: wgpu::BufferUsages::STORAGE,
        });
        let spare_buf = device.create_buffer(&wgpu::BufferDescriptor { label: Some("campfire spare"), size: 32, usage: wgpu::BufferUsages::STORAGE, mapped_at_creation: false });

        enum Out<'a> {
            Four(&'a Tex3),
            One(&'a Tex3),
            Two(&'a Tex3),
            None,
        }
        // `fixed`: the baked eddies and shape, or placeholders while they're being baked.
        let group_with = |fixed: (&Tex3, &Tex3), a: &Tex3, b: &Tex3, cc: &Tex3, d: &Tex3, out: Out, buf: &wgpu::Buffer| {
            let (o4, o1, o2) = match out {
                Out::Four(t) => (t, &out1, &out2),
                Out::One(t) => (&out4, t, &out2),
                Out::Two(t) => (&out4, &out1, t),
                Out::None => (&out4, &out1, &out2),
            };
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("campfire sim"),
                layout: &sim_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: sim_uniform.as_entire_binding() },
                    view_entry(1, &a.view),
                    view_entry(2, &b.view),
                    view_entry(3, &cc.view),
                    view_entry(4, &d.view),
                    sampler_entry(5, &linear),
                    view_entry(6, &o4.view),
                    view_entry(7, &o1.view),
                    view_entry(8, &o2.view),
                    wgpu::BindGroupEntry { binding: 9, resource: buf.as_entire_binding() },
                    view_entry(10, &fixed.0.view),
                    sampler_entry(11, &wrap),
                    wgpu::BindGroupEntry { binding: 12, resource: occupied.as_entire_binding() },
                    view_entry(13, &fixed.1.view),
                ],
            })
        };
        let group = |a: &Tex3, b: &Tex3, cc: &Tex3, d: &Tex3, out: Out, buf: &wgpu::Buffer| group_with((&eddies, &shape), a, b, cc, d, out, buf);
        let sb = &spare_buf;
        let sim_groups = SimGroups {
            curl: [0, 1].map(|i| group(&vel[i], &dummy4b, &dummy1, &dummy2, Out::Four(&curl), sb)),
            forces: [0, 1].map(|i| [0, 1].map(|j| group(&vel[i], &curl, &gas[j], &dummy2, Out::Four(&vel[1 - i]), sb))),
            divergence: [0, 1].map(|i| group(&vel[i], &dummy4b, &dummy1, &dummy1, Out::Two(divergence), sb)),
            smooth: (0..LEVELS).map(|l| [0, 1].map(|k| group(&dummy4b, &dummy4b, &pressures[l][k], &rhs[l], Out::One(&pressures[l][1 - k]), sb))).collect(),
            restrict: (0..LEVELS - 1).map(|l| [0, 1].map(|k| group(&dummy4b, &dummy4b, &pressures[l][k], &rhs[l], Out::Two(&rhs[l + 1]), sb))).collect(),
            clear: (0..LEVELS).map(|l| group(&dummy4b, &dummy4b, &dummy1, &dummy2, Out::One(&pressures[l][0]), sb)).collect(),
            prolong: (0..LEVELS - 1)
                .map(|l| [0, 1].map(|k| [0, 1].map(|kc| group(&dummy4b, &dummy4b, &pressures[l][k], &pressures[l + 1][kc], Out::One(&pressures[l][1 - k]), sb))))
                .collect(),
            project: [0, 1].map(|i| [0, 1].map(|k| group(&vel[i], &dummy4b, &pressure[k], divergence, Out::Four(&vel[1 - i]), sb))),
            advect_velocity: [0, 1].map(|i| group(&vel[i], &vel[i], &dummy1, &dummy2, Out::Four(&tmp), sb)),
            correct_velocity: [0, 1].map(|i| group(&vel[i], &tmp, &vel[i], &dummy2, Out::Four(&vel[1 - i]), sb)),
            advect_scalars: [0, 1].map(|i| [0, 1].map(|j| group(&vel[i], &gas[j], &dummy1, &dummy2, Out::Four(&tmp), sb))),
            correct_scalars: [0, 1].map(|i| [0, 1].map(|j| group(&vel[i], &tmp, &gas[j], &dummy2, Out::Four(&gas[1 - j]), sb))),
            detail_blocks: [0, 1].map(|j| group(&gas[j], &dummy4b, &dummy1, &dummy2, Out::None, sb)),
            detail_advect: [0, 1].map(|i| [0, 1].map(|j| group(&vel[i], &detail[j], &dummy1, &dummy2, Out::Four(&detail_tmp), sb))),
            detail_correct: [0, 1].map(|i| [0, 1].map(|j| group(&vel[i], &detail_tmp, &detail[j], &dummy2, Out::Four(&detail[1 - j]), sb))),
            lights: [0, 1].map(|j| group(&gas[j], &lut, &dummy1, &dummy2, Out::None, &lights)),
            sparks: [0, 1].map(|i| group(&vel[i], &dummy4b, &dummy1, &dummy2, Out::None, &sparks)),
            bake_shape: group_with((&eddies, &dummy4b), &dummy4b, &dummy4b, &dummy1, &dummy2, Out::Four(&shape), sb),
        };

        // Bake the eddies' noise once.
        {
            let potential_group = group_with((&dummy4b, &dummy4b), &dummy4b, &dummy4b, &dummy1, &dummy2, Out::Four(&potential), sb);
            let curl_group = group_with((&dummy4b, &dummy4b), &dummy4b, &potential, &dummy1, &dummy2, Out::Four(&eddies), sb);
            let size = [EDDY_TILE; 3];
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("campfire eddies") });
            Self::dispatch(
                &mut encoder,
                "campfire eddies",
                &[
                    SimPass { pipeline: &compute("eddy_potential"), group: &potential_group, size },
                    SimPass { pipeline: &compute("eddy_curl"), group: &curl_group, size },
                ],
            );
            queue.submit([encoder.finish()]);
        }

        // ---- scene ----
        let vf = S::VERTEX_FRAGMENT;
        let view_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("campfire view"),
            entries: &[
                layout_entry(0, vf, uniform_binding()),
                layout_entry(1, vf, storage_buffer(true)),
                layout_entry(2, vf, texture_binding(D::D3, true)),
                layout_entry(3, vf, texture_binding(D::D3, true)),
                layout_entry(4, vf, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
                layout_entry(5, vf, wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Depth, view_dimension: D::D2Array, multisampled: false }),
                layout_entry(6, vf, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison)),
                layout_entry(7, vf, texture_binding(D::D2, true)),
                layout_entry(8, vf, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
                layout_entry(9, vf, storage_buffer(true)),
                layout_entry(10, S::FRAGMENT, storage_buffer(true)),
                layout_entry(11, S::FRAGMENT, texture_binding(D::D3, true)),
                layout_entry(12, S::FRAGMENT, texture_binding(D::D3, true)),
            ],
        });
        let mesh_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("campfire mesh"),
            entries: &[
                layout_entry(0, vf, uniform_binding()),
                layout_entry(1, S::FRAGMENT, texture_binding(D::D2, true)),
                layout_entry(2, S::FRAGMENT, texture_binding(D::D2, true)),
                layout_entry(3, S::FRAGMENT, texture_binding(D::D2, true)),
            ],
        });
        let depth_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("campfire depth"),
            entries: &[layout_entry(0, S::FRAGMENT, wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Depth, view_dimension: D::D2, multisampled: false })],
        });
        let cache_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("campfire cache"),
            entries: &[layout_entry(0, S::FRAGMENT, texture_binding(D::D2, false))],
        });
        let face_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("campfire shadow face"),
            entries: &[layout_entry(0, S::VERTEX, uniform_binding())],
        });
        let pl = |groups: &[&wgpu::BindGroupLayout]| device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: groups, push_constant_ranges: &[] });
        let view_pl = pl(&[&view_layout]);
        let mesh_pl = pl(&[&view_layout, &mesh_layout]);
        let volume_pl = pl(&[&view_layout, &depth_layout]);
        let cache_pl = pl(&[&view_layout, &cache_layout]);
        let shadow_pl = pl(&[&face_layout, &mesh_layout]);

        let mesh_buffers = [wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<assets::Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
        }];
        let depth_state = |write: bool, compare: wgpu::CompareFunction| wgpu::DepthStencilState {
            format: DEPTH,
            depth_write_enabled: write,
            depth_compare: compare,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        };
        let render = |label: &str, layout: &wgpu::PipelineLayout, vs: &str, fs: &str, buffers: &[wgpu::VertexBufferLayout], blend: Option<wgpu::BlendState>, depth: Option<wgpu::DepthStencilState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState { module: &scene_module, entry_point: Some(vs), buffers, compilation_options: Default::default() },
                fragment: Some(wgpu::FragmentState {
                    module: &scene_module,
                    entry_point: Some(fs),
                    targets: &[Some(wgpu::ColorTargetState { format: HDR, blend, write_mask: wgpu::ColorWrites::ALL })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: depth,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        let add = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add };
        let over = wgpu::BlendState {
            color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::SrcAlpha, operation: wgpu::BlendOperation::Add },
            alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::Zero, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add },
        };
        let sky_pipeline = render("campfire sky", &view_pl, "vs_full", "fs_sky", &[], None, Some(depth_state(false, wgpu::CompareFunction::Always)));
        let mesh_pipeline = render("campfire meshes", &mesh_pl, "vs_mesh", "fs_mesh", &mesh_buffers, None, Some(depth_state(true, wgpu::CompareFunction::Less)));
        let spark_pipeline = render("campfire sparks", &view_pl, "vs_spark", "fs_spark", &[], Some(wgpu::BlendState { color: add, alpha: add }), Some(depth_state(false, wgpu::CompareFunction::Less)));
        let volume_pipeline = render("campfire flames", &volume_pl, "vs_full", "fs_volume", &[], Some(over), None);
        let cache_pipeline = render("campfire cached scene", &cache_pl, "vs_full", "fs_cached", &[], None, Some(depth_state(false, wgpu::CompareFunction::Always)));
        let shadow_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("campfire shadow"),
            layout: Some(&shadow_pl),
            vertex: wgpu::VertexState { module: &shadow_module, entry_point: Some("vs_shadow"), buffers: &mesh_buffers, compilation_options: Default::default() },
            fragment: None,
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState { constant: 2, slope_scale: 2.0, clamp: 0.0 },
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let shadow_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("campfire shadow"),
            size: wgpu::Extent3d { width: SHADOW_SIZE, height: SHADOW_SIZE, depth_or_array_layers: 6 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let light = Vec3::from(SHADOW_LIGHT);
        let face_matrices: Vec<Mat4> = [
            (Vec3::X, Vec3::NEG_Y),
            (Vec3::NEG_X, Vec3::NEG_Y),
            (Vec3::Y, Vec3::Z),
            (Vec3::NEG_Y, Vec3::NEG_Z),
            (Vec3::Z, Vec3::NEG_Y),
            (Vec3::NEG_Z, Vec3::NEG_Y),
        ]
        .into_iter()
        .map(|(dir, up)| Mat4::perspective_rh(std::f32::consts::FRAC_PI_2, 1.0, SHADOW_NEAR, SHADOW_FAR) * Mat4::look_at_rh(light, light + dir, up))
        .collect();
        let shadow_faces = face_matrices
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let view = shadow_tex.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(D::D2),
                    base_array_layer: i as u32,
                    array_layer_count: Some(1),
                    ..Default::default()
                });
                let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("campfire face"), contents: bytemuck::bytes_of(&mat(*m)), usage: wgpu::BufferUsages::UNIFORM });
                let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("campfire face"),
                    layout: &face_layout,
                    entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
                });
                (view, group)
            })
            .collect();
        let shadow_array = shadow_tex.create_view(&wgpu::TextureViewDescriptor { dimension: Some(D::D2Array), array_layer_count: Some(6), ..Default::default() });
        let shadow_cmp = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("campfire shadow"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });
        let textured = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("campfire textures"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            anisotropy_clamp: 16,
            ..Default::default()
        });
        let sky = solid_texture(device, queue, [0, 0, 0, 255], false);
        let view_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("campfire view"),
            size: std::mem::size_of::<ViewUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Baking the shadows: the view's shadow map, and per grid its box and output.
        let shade = [SHADE_NEAR, SHADE_FAR].map(|(_, _, size)| tex3(device, "campfire baked shadows", size, wgpu::TextureFormat::Rgba8Unorm));
        let bake_view_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("campfire bake view"),
            entries: &[
                layout_entry(0, c, uniform_binding()),
                layout_entry(5, c, wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Depth, view_dimension: D::D2Array, multisampled: false }),
                layout_entry(6, c, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison)),
            ],
        });
        let bake_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("campfire bake"),
            entries: &[
                layout_entry(0, c, uniform_binding()),
                layout_entry(1, c, wgpu::BindingType::StorageTexture { access: wgpu::StorageTextureAccess::WriteOnly, format: wgpu::TextureFormat::Rgba8Unorm, view_dimension: D::D3 }),
            ],
        });
        let bake_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("campfire bake shadows"),
            layout: Some(&pl(&[&bake_view_layout, &bake_layout])),
            module: &scene_module,
            entry_point: Some("bake_shadows"),
            compilation_options: Default::default(),
            cache: None,
        });
        let bake_view = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("campfire bake view"),
            layout: &bake_view_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: view_uniform.as_entire_binding() }, view_entry(5, &shadow_array), sampler_entry(6, &shadow_cmp)],
        });
        let bake_groups = [SHADE_NEAR, SHADE_FAR]
            .iter()
            .zip(&shade)
            .map(|((origin, voxel, size), out)| {
                let bounds = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("campfire bake box"),
                    contents: bytemuck::cast_slice(&[origin[0], origin[1], origin[2], *voxel]),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
                let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("campfire bake"),
                    layout: &bake_layout,
                    entries: &[wgpu::BindGroupEntry { binding: 0, resource: bounds.as_entire_binding() }, view_entry(1, &out.view)],
                });
                (bake_view.clone(), group, *size)
            })
            .collect();
        let mut view = ViewUniform::zeroed();
        let corner = |(o, voxel, _): ([f32; 3], f32, [u32; 3])| [o[0], o[1], o[2], voxel];
        view.shade_near = corner(SHADE_NEAR);
        view.shade_far = corner(SHADE_FAR);
        for (i, m) in face_matrices.iter().enumerate() {
            view.faces[i] = mat(*m);
        }

        let mut campfire = Campfire {
            sim_uniform,
            sim_pipelines,
            sim_groups,
            vel_i: 0,
            scal_i: 0,
            pres_i: 0,
            baked: false,
            unseen: false,
            draw_scene: true,
            frame_interval: super::STEP,
            since_drawn: 0.0,
            shadows_stale: true,
            view_uniform,
            view_groups: Vec::new(),
            mesh_layout,
            depth_layout,
            cache_layout,
            sky_pipeline,
            cache_pipeline,
            mesh_pipeline,
            spark_pipeline,
            volume_pipeline,
            shadow_pipeline,
            shadow_faces,
            bake_pipeline,
            bake_groups,
            shade,
            view_layout,
            lights,
            sparks,
            gas,
            vel,
            detail,
            occupied,
            lut,
            linear,
            shadow_cmp,
            shadow_array,
            textured,
            sky,
            meshes: Vec::new(),
            draws: Vec::new(),
            placed: layout(),
            loader: Some(assets::Loader::start()),
            targets: None,
            view,
            timestamps: None,
        };
        campfire.view_groups = (0..2).map(|j| campfire.view_group(device, j)).collect();
        let plain = (
            solid_texture(device, queue, [60, 52, 44, 255], true),
            solid_texture(device, queue, [128, 128, 255, 255], false),
            solid_texture(device, queue, [255, 230, 0, 255], false),
        );
        campfire.add_ground(device, &plain.0, &plain.1, &plain.2);
        campfire
    }

    fn view_group(&self, device: &wgpu::Device, gas: usize) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("campfire view"),
            layout: &self.view_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.view_uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: self.lights.as_entire_binding() },
                view_entry(2, &self.detail[gas].view),
                view_entry(3, &self.lut.view),
                sampler_entry(4, &self.linear),
                view_entry(5, &self.shadow_array),
                sampler_entry(6, &self.shadow_cmp),
                view_entry(7, &self.sky),
                sampler_entry(8, &self.textured),
                wgpu::BindGroupEntry { binding: 9, resource: self.sparks.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 10, resource: self.occupied.as_entire_binding() },
                view_entry(11, &self.shade[0].view),
                view_entry(12, &self.shade[1].view),
            ],
        })
    }

    fn draw_group(&self, device: &wgpu::Device, draw: DrawUniform, albedo: &wgpu::TextureView, normal: &wgpu::TextureView, arm: &wgpu::TextureView) -> wgpu::BindGroup {
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("campfire draw"), contents: bytemuck::bytes_of(&draw), usage: wgpu::BufferUsages::UNIFORM });
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("campfire draw"),
            layout: &self.mesh_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }, view_entry(1, albedo), view_entry(2, normal), view_entry(3, arm)],
        })
    }

    fn upload_mesh(&mut self, device: &wgpu::Device, vertices: &[assets::Vertex], indices: &[u32]) -> usize {
        let vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("campfire vertices"), contents: bytemuck::cast_slice(vertices), usage: wgpu::BufferUsages::VERTEX });
        let count = indices.len() as u32;
        let indices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("campfire indices"), contents: bytemuck::cast_slice(indices), usage: wgpu::BufferUsages::INDEX });
        self.meshes.push(GpuMesh { vertices, indices, count });
        self.meshes.len() - 1
    }

    fn add_ground(&mut self, device: &wgpu::Device, albedo: &wgpu::TextureView, normal: &wgpu::TextureView, arm: &wgpu::TextureView) {
        let (v, i) = ground_mesh();
        let mesh = self.upload_mesh(device, &v, &i);
        // The forest floor repeats every 1.6 m.
        let draw = DrawUniform { model: mat(Mat4::IDENTITY), params: [2.0, 1.0 / 1.6, 0.0, 0.0] };
        let group = self.draw_group(device, draw, albedo, normal, arm);
        self.draws.push(DrawCall { mesh, group });
    }

    /// Put the photo-scanned assets in, once they've loaded.
    fn attach(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, assets: assets::Assets) {
        self.meshes.clear();
        self.draws.clear();
        let upload = |m: &assets::Material, name: &str| {
            (
                texture_2d(device, queue, &format!("{name} albedo"), &m.albedo, true),
                texture_2d(device, queue, &format!("{name} normal"), &m.normal, false),
                texture_2d(device, queue, &format!("{name} arm"), &m.arm, false),
            )
        };
        let floor = upload(&assets.forest_floor, "forest floor");
        self.add_ground(device, &floor.0, &floor.1, &floor.2);
        let models = [(Model::Pit, &assets.pit, "pit"), (Model::Log, &assets.log, "log"), (Model::BigLog, &assets.big_log, "big log")];
        for (model, mesh, name) in models {
            let index = self.upload_mesh(device, &mesh.vertices, &mesh.indices);
            let textures = upload(&mesh.material, name);
            let placed: Vec<(Mat4, f32)> = self.placed.iter().filter(|p| p.model == model).map(|p| (p.transform, p.char)).collect();
            for (n, (transform, char)) in placed.into_iter().enumerate() {
                let kind = if model == Model::Pit { 0.0 } else { 1.0 };
                let draw = DrawUniform { model: mat(transform), params: [kind, 1.0, char, n as f32 * 13.7] };
                let group = self.draw_group(device, draw, &textures.0, &textures.1, &textures.2);
                self.draws.push(DrawCall { mesh: index, group });
            }
        }
        let sky = &assets.sky;
        let texture = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("campfire sky"),
                size: wgpu::Extent3d { width: sky.size.0, height: sky.size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: HDR,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            bytemuck::cast_slice(&sky.texels),
        );
        self.sky = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.view_groups = (0..2).map(|j| self.view_group(device, j)).collect();
        self.shadows_stale = true;
    }

    /// Per frame, before rendering: assets, render targets, this frame's settings, and whether
    /// it draws the scene. `steps` is how many simulation steps it runs.
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, params: &Params, size: (u32, u32), steps: u32) {
        if let Some(result) = self.loader.as_ref().and_then(|l| l.poll()) {
            self.loader = None;
            if let Ok(assets) = result {
                self.attach(device, queue, assets);
            }
        }
        if self.targets.as_ref().is_none_or(|t| t.size != size) {
            let depth = device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("campfire depth"),
                    size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: DEPTH,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default());
            let depth_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("campfire depth"),
                layout: &self.depth_layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&depth) }],
            });
            let cache = device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("campfire scene"),
                    size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: HDR,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default());
            let cache_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("campfire cache"),
                layout: &self.cache_layout,
                entries: &[view_entry(0, &cache)],
            });
            self.targets = Some(Targets { size, depth, depth_group, cache, cache_group, drawn: false });
        }

        // The scene only changes when the simulation steps (60 times a second), so it's drawn
        // once per step, not every displayed frame. On fast displays (over 100 Hz) it's drawn
        // on a frame between steps, so a step and a draw never share a frame (at 120 Hz they
        // alternate), unless another step comes first. Slower displays step nearly every frame
        // and draw whenever there's anything new.
        self.frame_interval += (params.frame_dt.clamp(0.001, 0.1) - self.frame_interval) * 0.1;
        let fast = self.frame_interval < 0.6 * super::STEP;
        let drawn = self.targets.as_ref().is_some_and(|t| t.drawn);
        self.draw_scene = !drawn || self.unseen || (!fast && steps > 0);
        self.since_drawn += params.frame_dt;

        let mut logs = [[0.0f32; 4]; 24];
        let mut count = 0;
        for p in self.placed.iter().filter_map(|p| p.capsule).take(12) {
            logs[count * 2] = [p.0.x, p.0.y, p.0.z, p.2];
            logs[count * 2 + 1] = [p.1.x, p.1.y, p.1.z, 0.0];
            count += 1;
        }
        let cell = SIM_SIZE[1] / GRID[1] as f32;
        let glow = |c: [f32; 3]| [c[0] * params.salts * SALT_GLOW, c[1] * params.salts * SALT_GLOW, c[2] * params.salts * SALT_GLOW, 0.0];
        let sim = SimUniform {
            grid: [GRID[0] as f32, GRID[1] as f32, GRID[2] as f32, cell],
            origin: [SIM_ORIGIN[0], SIM_ORIGIN[1], SIM_ORIGIN[2], params.sim_dt],
            forces: [5.0, 1.2, 1.3, 8.0],
            burn: [params.fuel * 1.5, 4.0, 2.1, 0.42],
            flow: [params.clock, params.kick, params.salts, params.salt_drift],
            coals: [0.42, params.coals, 25.0, count as f32],
            sparks: [params.spark_first as f32, params.spark_count as f32, params.frame_dt, params.frame as f32],
            salt_a: glow(params.salt_a),
            salt_b: glow(params.salt_b),
            detail: [DETAIL[0] as f32, DETAIL[1] as f32, DETAIL[2] as f32, SIM_SIZE[1] / DETAIL[1] as f32],
            subgrid: [EDDY_STRENGTH, EDDY_TILES_PER_M, 1.2, 1.5],
            expose: [self.since_drawn, EXPOSURE_REFERENCE, 0.9, 2.5],
            bands: [[params.bands[0], params.bands[1], params.bands[2], params.bands[3]], [params.bands[4], params.bands[5], params.bands[6], params.bands[7]]],
            logs,
        };
        queue.write_buffer(&self.sim_uniform, 0, bytemuck::bytes_of(&sim));

        // The camera sits a few steps back at a seated person's eye height, the fire low in
        // the frame and the night sky and treeline above, drifting slowly.
        let t = params.time;
        let swing = 0.14 * (t * 0.045).sin();
        let eye = Vec3::new(2.4 * swing.sin(), 1.0 + 0.03 * (t * 0.07).sin(), 2.4 * swing.cos());
        let aspect = size.0 as f32 / size.1.max(1) as f32;
        let proj = Mat4::perspective_rh(36f32.to_radians(), aspect, 0.05, 200.0);
        let view_proj = proj * Mat4::look_at_rh(eye, Vec3::new(0.0, 0.5, 0.0), Vec3::Y);
        let v = &mut self.view;
        v.view_proj = mat(view_proj);
        v.inv_view_proj = mat(view_proj.inverse());
        v.eye = [eye.x, eye.y, eye.z, 1.5];
        v.sim_origin = [SIM_ORIGIN[0], SIM_ORIGIN[1], SIM_ORIGIN[2], SIM_SIZE[1] / DETAIL[1] as f32];
        v.sim_size = [SIM_SIZE[0], SIM_SIZE[1], SIM_SIZE[2], LIGHTS as f32];
        // The fire is about 30 cm in radius: that sets how soft its shadows are.
        v.shadow = [SHADOW_LIGHT[0], SHADOW_LIGHT[1], SHADOW_LIGHT[2], 0.3];
        v.sky = [0.35, 0.03, 0.0, t];
        v.flame = [22.0, params.salts * SALT_GLOW, 0.15, params.coals];
        v.salt_a = [params.salt_a[0], params.salt_a[1], params.salt_a[2], 0.0];
        v.salt_b = [params.salt_b[0], params.salt_b[1], params.salt_b[2], 0.0];
        v.screen = [size.0 as f32, size.1 as f32, params.frame as f32, 0.8];
        queue.write_buffer(&self.view_uniform, 0, bytemuck::bytes_of(&self.view));
        if self.draw_scene {
            self.since_drawn = 0.0;
        }
    }

    fn dispatch(encoder: &mut wgpu::CommandEncoder, label: &str, passes: &[SimPass<'_>]) {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some(label), timestamp_writes: None });
        for p in passes {
            pass.set_pipeline(p.pipeline);
            pass.set_bind_group(0, p.group, &[]);
            pass.dispatch_workgroups(p.size[0].div_ceil(8), p.size[1].div_ceil(8), p.size[2].div_ceil(4));
        }
    }

    fn compute_stamps(&self, stage: u32) -> Option<wgpu::ComputePassTimestampWrites<'_>> {
        self.timestamps.as_ref().map(|q| wgpu::ComputePassTimestampWrites { query_set: q, beginning_of_pass_write_index: Some(stage * 2), end_of_pass_write_index: Some(stage * 2 + 1) })
    }

    /// Timestamps at the start of a stage's first pass and the end of its last.
    fn render_stamps(&self, stage: u32, first: bool, last: bool) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
        self.timestamps.as_ref().filter(|_| first || last).map(|q| wgpu::RenderPassTimestampWrites {
            query_set: q,
            beginning_of_pass_write_index: first.then_some(stage * 2),
            end_of_pass_write_index: last.then_some(stage * 2 + 1),
        })
    }

    /// One multigrid V-cycle from `level` down, recording passes. `k` tracks which of each
    /// level's two pressure textures is newest.
    fn v_cycle<'a>(g: &'a SimGroups, p: &'a SimPipelines, level: usize, k: &mut [usize; LEVELS], passes: &mut Vec<SimPass<'a>>) {
        let size = level_size(level);
        let smooth = |k: &mut [usize; LEVELS], passes: &mut Vec<SimPass<'a>>, n: usize| {
            for _ in 0..n {
                passes.push(SimPass { pipeline: &p.jacobi, group: &g.smooth[level][k[level]], size });
                k[level] = 1 - k[level];
            }
        };
        if level + 1 == LEVELS {
            smooth(k, passes, COARSEST_SMOOTH);
            return;
        }
        smooth(k, passes, SMOOTH);
        let coarse = level_size(level + 1);
        passes.push(SimPass { pipeline: &p.restrict, group: &g.restrict[level][k[level]], size: coarse });
        passes.push(SimPass { pipeline: &p.clear, group: &g.clear[level + 1], size: coarse });
        k[level + 1] = 0;
        Self::v_cycle(g, p, level + 1, k, passes);
        passes.push(SimPass { pipeline: &p.prolong, group: &g.prolong[level][k[level]][k[level + 1]], size });
        k[level] = 1 - k[level];
        smooth(k, passes, SMOOTH);
    }

    /// One step's passes in phases (the coarse simulation, then the detail grid), and the
    /// ping-pong indices after it: velocity, scalars, pressure.
    fn step(&self) -> (Vec<Phase<'_>>, (usize, usize, usize)) {
        let (p, g) = (&self.sim_pipelines, &self.sim_groups);
        let (i, j) = (self.vel_i, self.scal_i);
        // Forces, then make the flow incompressible.
        let size = GRID;
        let mut coarse = vec![
            SimPass { pipeline: &p.curl, group: &g.curl[i], size },
            SimPass { pipeline: &p.forces, group: &g.forces[i][j], size },
            SimPass { pipeline: &p.divergence, group: &g.divergence[1 - i], size },
        ];
        let mut k = [0; LEVELS];
        k[0] = self.pres_i;
        for _ in 0..V_CYCLES {
            Self::v_cycle(g, p, 0, &mut k, &mut coarse);
        }
        coarse.push(SimPass { pipeline: &p.project, group: &g.project[1 - i][k[0]], size });
        // Then carry everything along it, burning as it goes.
        coarse.push(SimPass { pipeline: &p.advect_velocity, group: &g.advect_velocity[i], size });
        coarse.push(SimPass { pipeline: &p.correct_velocity, group: &g.correct_velocity[i], size });
        coarse.push(SimPass { pipeline: &p.advect_scalars, group: &g.advect_scalars[1 - i][j], size });
        coarse.push(SimPass { pipeline: &p.correct_scalars, group: &g.correct_scalars[1 - i][j], size });
        // And the flames as drawn, on the finer grid, where there's anything to draw.
        let size = DETAIL;
        let phases = vec![
            ("campfire step", coarse),
            // One workgroup per block (`dispatch` counts in 8×8×4 groups).
            ("campfire detail blocks", vec![SimPass { pipeline: &p.detail_blocks, group: &g.detail_blocks[1 - j], size: [DETAIL[0], DETAIL[1], DETAIL[2] / 2] }]),
            ("campfire detail advect", vec![SimPass { pipeline: &p.detail_advect, group: &g.detail_advect[1 - i][j], size }]),
            ("campfire detail correct", vec![SimPass { pipeline: &p.detail_correct, group: &g.detail_correct[1 - i][j], size }]),
        ];
        (phases, (1 - i, 1 - j, k[0]))
    }

    /// Advance the fire by `steps` fixed steps.
    pub fn simulate(&mut self, encoder: &mut wgpu::CommandEncoder, steps: u32) {
        if steps > 0 && !self.baked {
            let (p, g) = (&self.sim_pipelines, &self.sim_groups);
            Self::dispatch(encoder, "campfire shape", &[SimPass { pipeline: &p.bake_shape, group: &g.bake_shape, size: GRID }]);
            self.baked = true;
        }
        if steps > 0 {
            self.unseen = true;
        }
        for _ in 0..steps {
            let (phases, next) = self.step();
            for (label, passes) in &phases {
                Self::dispatch(encoder, label, passes);
            }
            drop(phases);
            (self.vel_i, self.scal_i, self.pres_i) = next;
        }
    }

    /// Draw into `color` (HDR, the scene's size): the scene if this frame draws it (see
    /// `prepare`), else as last drawn, and the sparks over it.
    pub fn render(&mut self, encoder: &mut wgpu::CommandEncoder, color: &wgpu::TextureView) {
        if self.targets.is_none() {
            return;
        }
        if self.draw_scene {
            self.draw(encoder);
        }
        let Some(targets) = &self.targets else { return };
        let (p, g) = (&self.sim_pipelines, &self.sim_groups);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("campfire sparks"), timestamp_writes: None });
            pass.set_pipeline(&p.sparks);
            pass.set_bind_group(0, &g.sparks[self.vel_i], &[]);
            pass.dispatch_workgroups(SPARKS.div_ceil(256), 1, 1);
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("campfire sparks"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &targets.depth,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                stencil_ops: None,
            }),
            timestamp_writes: self.render_stamps(4, true, true),
            occlusion_query_set: None,
        });
        pass.set_bind_group(0, &self.view_groups[self.scal_i], &[]);
        pass.set_pipeline(&self.cache_pipeline);
        pass.set_bind_group(1, &targets.cache_group, &[]);
        pass.draw(0..3, 0..1);
        pass.set_pipeline(&self.spark_pipeline);
        pass.draw(0..6, 0..SPARKS);
    }

    /// Light the scene and draw it (sky, meshes, flames) into the cache.
    fn draw(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let Some(targets) = &self.targets else { return };
        let (p, g) = (&self.sim_pipelines, &self.sim_groups);
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("campfire lights"), timestamp_writes: self.compute_stamps(0) });
            pass.set_pipeline(&p.lights);
            pass.set_bind_group(0, &g.lights[self.scal_i], &[]);
            pass.dispatch_workgroups(4, 6, 4);
            pass.set_pipeline(&p.expose);
            pass.dispatch_workgroups(1, 1, 1);
        }
        if self.shadows_stale {
            for (n, (face, group)) in self.shadow_faces.iter().enumerate() {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("campfire shadow"),
                    color_attachments: &[],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: face,
                        depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: self.render_stamps(1, n == 0, n + 1 == self.shadow_faces.len()),
                    occlusion_query_set: None,
                });
                pass.set_pipeline(&self.shadow_pipeline);
                pass.set_bind_group(0, group, &[]);
                for draw in &self.draws {
                    let mesh = &self.meshes[draw.mesh];
                    pass.set_bind_group(1, &draw.group, &[]);
                    pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                    pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..mesh.count, 0, 0..1);
                }
            }
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("campfire bake shadows"), timestamp_writes: self.compute_stamps(5) });
            pass.set_pipeline(&self.bake_pipeline);
            for (view, group, size) in &self.bake_groups {
                pass.set_bind_group(0, view, &[]);
                pass.set_bind_group(1, group, &[]);
                pass.dispatch_workgroups(size[0].div_ceil(8), size[1].div_ceil(8), size[2].div_ceil(4));
            }
        }
        let view_group = &self.view_groups[self.scal_i];
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("campfire scene"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &targets.cache,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &targets.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: self.render_stamps(2, true, true),
                occlusion_query_set: None,
            });
            pass.set_bind_group(0, view_group, &[]);
            pass.set_pipeline(&self.sky_pipeline);
            pass.draw(0..3, 0..1);
            pass.set_pipeline(&self.mesh_pipeline);
            for draw in &self.draws {
                let mesh = &self.meshes[draw.mesh];
                pass.set_bind_group(1, &draw.group, &[]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.count, 0, 0..1);
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("campfire flames"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &targets.cache,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: self.render_stamps(3, true, true),
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.volume_pipeline);
            pass.set_bind_group(0, view_group, &[]);
            pass.set_bind_group(1, &targets.depth_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.shadows_stale = false;
        self.unseen = false;
        if let Some(t) = self.targets.as_mut() {
            t.drawn = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read3(device: &wgpu::Device, queue: &wgpu::Queue, tex: &Tex3) -> Vec<[f32; 4]> {
        let [w, h, d] = GRID;
        let row = (w * 8).div_ceil(256) * 256;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (row * h * d) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &tex.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: d },
        );
        queue.submit([encoder.finish()]);
        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
        let data = buffer.slice(..).get_mapped_range();
        let mut out = Vec::with_capacity((w * h * d) as usize);
        for z in 0..d {
            for y in 0..h {
                for x in 0..w {
                    let o = ((z * h + y) * row + x * 8) as usize;
                    let px: [half::f16; 4] = bytemuck::pod_read_unaligned(&data[o..o + 8]);
                    out.push(px.map(|v| v.to_f32()));
                }
            }
        }
        out
    }

    /// Print the fire's profile by height: hottest, sootiest, fastest rising.
    fn profile(device: &wgpu::Device, queue: &wgpu::Queue, fire: &Campfire) {
        let gas = read3(device, queue, &fire.gas[fire.scal_i]);
        let vel = read3(device, queue, &fire.vel[fire.vel_i]);
        let bad = |v: &[[f32; 4]]| v.iter().filter(|p| p.iter().any(|x| !x.is_finite())).count();
        eprintln!("non-finite cells: gas {}, velocity {}", bad(&gas), bad(&vel));
        let [w, h, d] = GRID;
        let cell = SIM_SIZE[1] / h as f32;
        for y in (0..h).step_by(12) {
            let (mut t, mut soot, mut fuel, mut up, mut side) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for z in 0..d {
                for x in 0..w {
                    let i = ((z * h + y) * w + x) as usize;
                    t = t.max(gas[i][0]);
                    fuel = fuel.max(gas[i][1]);
                    soot = soot.max(gas[i][2]);
                    up = up.max(vel[i][1]);
                    side = side.max(vel[i][0].abs().max(vel[i][2].abs()));
                }
            }
            eprintln!("y {:5.2} m: T max {t:4.2} kK  fuel {fuel:5.2}  soot {soot:5.2}  up {up:5.2} m/s  side {side:5.2}", SIM_ORIGIN[1] + (y as f32 + 0.5) * cell);
        }
    }

    /// One line on how the fire stands: how high the flames reach and how the air moves.
    fn series(device: &wgpu::Device, queue: &wgpu::Queue, fire: &Campfire, t: f32, image: &wgpu::Texture) {
        let gas = read3(device, queue, &fire.gas[fire.scal_i]);
        let vel = read3(device, queue, &fire.vel[fire.vel_i]);
        let [w, h, d] = GRID;
        let cell = SIM_SIZE[1] / h as f32;
        let at = |y: u32| SIM_ORIGIN[1] + (y as f32 + 0.5) * cell;
        let mut top = 0.0f32;
        let mut line = String::new();
        let (mut soot_sum, mut warm, mut hot_cells) = (0.0f64, 0.0f64, 0u32);
        for y in 0..h {
            let (mut tmax, mut up_sum, mut up_n, mut down) = (0.0f32, 0.0f32, 0u32, 0.0f32);
            for z in 0..d {
                for x in 0..w {
                    let i = ((z * h + y) * w + x) as usize;
                    let g = gas[i];
                    tmax = tmax.max(g[0]);
                    soot_sum += g[2] as f64;
                    warm += (g[0] - 0.29) as f64;
                    if g[0] > 1.2 {
                        hot_cells += 1;
                    }
                    let (px, pz) = (x as f32 - w as f32 / 2.0, z as f32 - d as f32 / 2.0);
                    if px * px + pz * pz < 40.0 * 40.0 {
                        up_sum += vel[i][1];
                        up_n += 1;
                    }
                    down = down.min(vel[i][1]);
                }
            }
            if tmax > 1.2 {
                top = at(y);
            }
            if y % 32 == 16 {
                line += &format!(" {:.2}:{:+.2}/{:+.1}", at(y), up_sum / up_n.max(1) as f32, down);
            }
        }
        let light = {
            let readback = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 32, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            encoder.copy_buffer_to_buffer(&fire.lights, LIGHTS as u64 * 32, &readback, 0, 32);
            queue.submit([encoder.finish()]);
            readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
            let v: Vec<f32> = bytemuck::cast_slice(&readback.slice(..).get_mapped_range()).to_vec();
            (v[1], v[0])
        };
        let (w, h) = (image.width(), image.height());
        let row = (w * 8).div_ceil(256) * 256;
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (row * h) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: image, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
        let data = readback.slice(..).get_mapped_range();
        let mut sum = 0.0f64;
        for y in 0..h {
            for x in 0..w {
                let o = (y * row + x * 8) as usize;
                let px: [half::f16; 4] = bytemuck::pod_read_unaligned(&data[o..o + 8]);
                sum += (0.2126 * px[0].to_f32() + 0.7152 * px[1].to_f32() + 0.0722 * px[2].to_f32()) as f64;
            }
        }
        let shown = sum / (w * h) as f64;
        eprintln!("t {t:5.1}s shown {shown:6.4} (unexposed {:6.4}) light {:5.2} exposure {:4.2} flame top {top:4.2} m  hot {hot_cells:7}  soot {soot_sum:8.0}  warm {warm:9.0} | mean up/min up by height:{line}", shown / light.1 as f64, light.0, light.1);
    }

    /// Resolve `count` timestamps from `set` (in ms, relative to the first).
    fn stamps(device: &wgpu::Device, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, set: &wgpu::QuerySet, count: u32) -> wgpu::Buffer {
        let resolved = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: count as u64 * 8, usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: count as u64 * 8, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        encoder.resolve_query_set(set, 0..count, &resolved, 0);
        encoder.copy_buffer_to_buffer(&resolved, 0, &readback, 0, count as u64 * 8);
        let _ = queue;
        readback
    }

    fn read_stamps(device: &wgpu::Device, queue: &wgpu::Queue, readback: &wgpu::Buffer) -> Vec<f64> {
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
        let period = queue.get_timestamp_period() as f64 / 1e6;
        let ticks: Vec<u64> = bytemuck::cast_slice(&readback.slice(..).get_mapped_range()).to_vec();
        ticks.iter().map(|t| t.saturating_sub(ticks[0]) as f64 * period).collect()
    }

    /// How long each stage of drawing and each pass of a step take on the GPU (medians), and
    /// how much of the detail grid runs.
    fn timing(device: &wgpu::Device, queue: &wgpu::Queue, fire: &mut Campfire, view: &wgpu::TextureView) {
        let wait = || device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
        wait();
        let median = |mut v: Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let count = STAGES.len() as u32 * 2;
        fire.timestamps = Some(device.create_query_set(&wgpu::QuerySetDescriptor { label: None, ty: wgpu::QueryType::Timestamp, count }));
        let mut stages = vec![Vec::new(); STAGES.len()];
        for _ in 0..20 {
            // A full draw, shadow map and all, to see every stage's cost.
            fire.draw_scene = true;
            fire.shadows_stale = true;
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            fire.render(&mut encoder, view);
            let readback = stamps(device, queue, &mut encoder, fire.timestamps.as_ref().unwrap(), count);
            queue.submit([encoder.finish()]);
            let t = read_stamps(device, queue, &readback);
            for (n, stage) in stages.iter_mut().enumerate() {
                stage.push(t[n * 2 + 1] - t[n * 2]);
            }
        }
        fire.timestamps = None;
        let mut drawing = 0.0;
        for (n, (name, times)) in STAGES.iter().zip(stages).enumerate() {
            let ms = median(times);
            if n == 0 || n == 2 || n == 3 {
                drawing += ms;
            }
            eprintln!("draw: {name:28} {ms:6.2} ms");
        }
        eprintln!("draw: {:28} {drawing:6.2} ms", "scene draw (once per step)");
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 32, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_buffer_to_buffer(&fire.lights, LIGHTS as u64 * 32, &readback, 0, 32);
        queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        wait();
        let exposure: Vec<f32> = bytemuck::cast_slice(&readback.slice(..).get_mapped_range()).to_vec();
        eprintln!("fire's total light {:.3}, exposure {:.3}", exposure[1], exposure[0]);

        let p = &fire.sim_pipelines;
        let names = [
            (&p.curl, "curl"),
            (&p.forces, "forces"),
            (&p.divergence, "divergence"),
            (&p.jacobi, "jacobi"),
            (&p.restrict, "restrict"),
            (&p.clear, "clear"),
            (&p.prolong, "prolong"),
            (&p.project, "project"),
            (&p.advect_velocity, "advect velocity"),
            (&p.correct_velocity, "correct velocity"),
            (&p.advect_scalars, "advect scalars"),
            (&p.correct_scalars, "correct scalars"),
            (&p.detail_blocks, "detail blocks"),
            (&p.detail_advect, "detail advect"),
            (&p.detail_correct, "detail correct"),
        ]
        .map(|(pipeline, name)| (pipeline as *const wgpu::ComputePipeline, name));
        // Each pass repeated, so its own cost shows rather than the gaps between passes.
        let repeat = 8;
        let mut totals: Vec<(&str, Vec<f64>)> = Vec::new();
        for _ in 0..10 {
            let (phases, next) = fire.step();
            let passes: Vec<&SimPass> = phases.iter().flat_map(|(_, passes)| passes).collect();
            let count = passes.len() as u32 * 2;
            let set = device.create_query_set(&wgpu::QuerySetDescriptor { label: None, ty: wgpu::QueryType::Timestamp, count });
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            for (n, pass) in passes.iter().enumerate() {
                let mut cp = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: None,
                    timestamp_writes: Some(wgpu::ComputePassTimestampWrites { query_set: &set, beginning_of_pass_write_index: Some(n as u32 * 2), end_of_pass_write_index: Some(n as u32 * 2 + 1) }),
                });
                cp.set_pipeline(pass.pipeline);
                cp.set_bind_group(0, pass.group, &[]);
                for _ in 0..repeat {
                    cp.dispatch_workgroups(pass.size[0].div_ceil(8), pass.size[1].div_ceil(8), pass.size[2].div_ceil(4));
                }
            }
            let readback = stamps(device, queue, &mut encoder, &set, count);
            queue.submit([encoder.finish()]);
            let t = read_stamps(device, queue, &readback);
            let mut step: Vec<(&str, f64)> = Vec::new();
            for (n, pass) in passes.iter().enumerate() {
                let name = names.iter().find(|(ptr, _)| std::ptr::eq(*ptr, pass.pipeline)).map_or("?", |(_, n)| n);
                let ms = (t[n * 2 + 1] - t[n * 2]) / repeat as f64;
                match step.iter_mut().find(|(n, _)| *n == name) {
                    Some(s) => s.1 += ms,
                    None => step.push((name, ms)),
                }
            }
            for (name, ms) in step {
                match totals.iter_mut().find(|(n, _)| *n == name) {
                    Some(t) => t.1.push(ms),
                    None => totals.push((name, vec![ms])),
                }
            }
            drop(passes);
            drop(phases);
            (fire.vel_i, fire.scal_i, fire.pres_i) = next;
        }
        let mut sum = 0.0;
        for (label, times) in totals {
            let ms = median(times);
            sum += ms;
            eprintln!("step: {label:22} {ms:6.2} ms");
        }
        eprintln!("step: {:22} {sum:6.2} ms", "total");
        let size = fire.occupied.size();
        let buffer = device.create_buffer(&wgpu::BufferDescriptor { label: None, size, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_buffer_to_buffer(&fire.occupied, 0, &buffer, 0, size);
        queue.submit([encoder.finish()]);
        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        wait();
        let flags: Vec<u32> = bytemuck::cast_slice(&buffer.slice(..).get_mapped_range()).to_vec();
        let runs = flags.iter().filter(|f| *f & 2 != 0).count();
        let shows = flags.iter().filter(|f| *f & 1 != 0).count();
        eprintln!("detail blocks: {runs} of {} run ({:.0}%), {shows} show ({:.0}%)", flags.len(), 100.0 * runs as f64 / flags.len() as f64, 100.0 * shows as f64 / flags.len() as f64);
    }

    /// Render the campfire headless to `$CAMPFIRE_PNG` (needs a GPU and the assets):
    /// `CAMPFIRE_PNG=/tmp/fire.png cargo test --profile fast -p vibrance -- --ignored snapshot`.
    /// `CAMPFIRE_SECONDS` sets how long the fire burns first, `CAMPFIRE_SIZE` the image ("WxH"),
    /// `CAMPFIRE_RATE` how fast it flickers (as the music version does), `CAMPFIRE_FUEL` how
    /// hard it burns, and `CAMPFIRE_KICKS` pumps it with kicks twice a second. `CAMPFIRE_SERIES`
    /// prints how bright it shows, how high the flames reach and how the air moves every half
    /// second,
    /// `CAMPFIRE_PROFILE` the fire by height at the end, and `CAMPFIRE_TIMING` each pass's GPU
    /// time and the draw time.
    #[test]
    #[ignore]
    fn snapshot() {
        let path = std::env::var("CAMPFIRE_PNG").unwrap_or_else(|_| "/tmp/campfire.png".into());
        let seconds: f32 = std::env::var("CAMPFIRE_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(8.0);
        let size = std::env::var("CAMPFIRE_SIZE")
            .ok()
            .and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
            .unwrap_or((1920u32, 1080u32));
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..Default::default() });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })).expect("adapter");
        // The same limits iced asks for.
        let limits = wgpu::Limits { max_bind_groups: 2, max_non_sampler_bindings: 2048, ..wgpu::Limits::default() };
        let features = if std::env::var_os("CAMPFIRE_TIMING").is_some() { wgpu::Features::TIMESTAMP_QUERY } else { wgpu::Features::empty() };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_limits: limits, required_features: features, ..Default::default() })).expect("device");
        let mut fire = Campfire::new(&device, &queue);
        while fire.loader.is_some() {
            std::thread::sleep(std::time::Duration::from_millis(50));
            fire.prepare(&device, &queue, &Params::default(), size, 0);
        }
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: HDR,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        // `CAMPFIRE_RATE` runs the flicker faster, as the music version does for fast songs.
        let rate: f32 = std::env::var("CAMPFIRE_RATE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
        let fuel: f32 = std::env::var("CAMPFIRE_FUEL").ok().and_then(|s| s.parse().ok()).unwrap_or(4.0);
        // `CAMPFIRE_KICKS`: pumping music, a kick twice a second, as the music version takes it.
        let kicks = std::env::var_os("CAMPFIRE_KICKS").is_some();
        let frames = (seconds * 60.0) as u32;
        let mut clock = 0.0;
        let mut cursor = 0;
        let started = std::time::Instant::now();
        for frame in 0..frames {
            let t = frame as f32 / 60.0;
            let count = if frame % 20 == 0 { 2 } else { 0 };
            let kick = if kicks { (-(t % 0.5) / 0.12).exp() } else { 0.0 };
            let (rate, fuel) = if kicks { (1.0 + kick, 4.2 + 2.5 * kick) } else { (rate, fuel) };
            clock += rate / 60.0;
            let params = Params {
                clock,
                sim_dt: rate / 60.0,
                frame_dt: 1.0 / 60.0,
                time: t,
                fuel,
                kick,
                bands: [0.5; 8],
                coals: 0.8,
                spark_first: cursor,
                spark_count: count,
                frame,
                ..Params::default()
            };
            cursor = (cursor + count) % SPARKS;
            fire.prepare(&device, &queue, &params, size, 1);
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            fire.simulate(&mut encoder, 1);
            if frame + 1 == frames {
                fire.render(&mut encoder, &view);
            }
            queue.submit([encoder.finish()]);
            if std::env::var_os("CAMPFIRE_SERIES").is_some() && frame % 30 == 29 {
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                fire.draw_scene = true;
                fire.render(&mut encoder, &view);
                queue.submit([encoder.finish()]);
                series(&device, &queue, &fire, t, &target);
            }
        }
        if std::env::var_os("CAMPFIRE_PROFILE").is_some() {
            profile(&device, &queue, &fire);
        }
        if std::env::var_os("CAMPFIRE_TIMING").is_some() {
            timing(&device, &queue, &mut fire, &view);
        }
        let row = (size.0 * 8).div_ceil(256) * 256;
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (row * size.1) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None } },
            wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
        eprintln!("{frames} frames in {:?}", started.elapsed());
        let data = readback.slice(..).get_mapped_range();
        // A filmic curve like the visualizer's, then sRGB.
        let tone = |x: f32| {
            let x = x.max(0.0);
            let k = 0.7;
            let x = if x > k { k + (1.0 - k) * (1.0 - (-(x - k) / (1.0 - k)).exp()) } else { x };
            let x = (x * x / (x + 0.015)).powf(0.8);
            (x.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8
        };
        let mut img = image::RgbImage::new(size.0, size.1);
        for y in 0..size.1 {
            for x in 0..size.0 {
                let o = (y * row + x * 8) as usize;
                let px: [half::f16; 4] = bytemuck::pod_read_unaligned(&data[o..o + 8]);
                img.put_pixel(x, y, image::Rgb([tone(px[0].to_f32()), tone(px[1].to_f32()), tone(px[2].to_f32())]));
            }
        }
        img.save(&path).expect("save");
    }

    /// Every shader parses and validates (wgpu would otherwise only say so at run time, fatally).
    #[test]
    fn shaders_validate() {
        let shaders = [
            ("sim.wgsl", include_str!("sim.wgsl")),
            ("scene.wgsl", include_str!("scene.wgsl")),
            ("shadow.wgsl", include_str!("shadow.wgsl")),
            ("viz.wgsl", include_str!("../viz.wgsl")),
        ];
        for (name, source) in shaders {
            let module = naga::front::wgsl::parse_str(source).unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(source)));
            naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::default())
                .validate(&module)
                .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(source)));
        }
    }
}
