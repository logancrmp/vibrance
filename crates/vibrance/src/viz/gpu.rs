//! wgpu side of the visualizer: ping-pong feedback textures stepped at a fixed rate, the
//! campfire's gas simulation, a 3D view of them into a scene texture, bloom, the
//! photosensitivity limiter, and the final pass to the screen.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use iced::widget::shader::{self, Viewport};
use iced::{Rectangle, mouse};

use super::campfire::Campfire;
use super::{Frame, Uniforms, Vertex};
use crate::app::Message;

/// Larger areas render at this many pixels and are upscaled.
const MAX_PIXELS: f32 = 3840.0 * 2160.0;
/// Mip levels per chain: the feedback's for the 3D view's level of detail, the scene's for
/// bloom and the limiter. Matches `MAX_LOD` in the shader.
const MIPS: u32 = 6;
const HDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The shader widget's program: hands the latest engine frame to the GPU.
pub struct Program {
    pub frame: Arc<Frame>,
}

impl shader::Program<Message> for Program {
    type State = ();
    type Primitive = Primitive;

    fn draw(&self, _state: &(), _cursor: mouse::Cursor, _bounds: Rectangle) -> Primitive {
        Primitive { frame: self.frame.clone() }
    }
}

#[derive(Debug)]
pub struct Primitive {
    frame: Arc<Frame>,
}

/// A texture with its mip levels, each renderable and sampleable on its own.
struct Chain {
    levels: Vec<wgpu::TextureView>,
    /// Samples only that level (for building the chain).
    level_groups: Vec<wgpu::BindGroup>,
    /// Samples every level.
    group: wgpu::BindGroup,
    full: wgpu::TextureView,
}

struct Targets {
    size: (u32, u32),
    feedback: [Chain; 2],
    scene: Chain,
    /// The campfire's scene, drawn at this size.
    fire: wgpu::TextureView,
    /// Scene pass, by which feedback texture is newest.
    scenes: [wgpu::BindGroup; 2],
    /// Final pass, by which limiter texture is newest.
    finals: [wgpu::BindGroup; 2],
}

pub struct Pipeline {
    warp: wgpu::RenderPipeline,
    shape: wgpu::RenderPipeline,
    down: wgpu::RenderPipeline,
    up: wgpu::RenderPipeline,
    scene: wgpu::RenderPipeline,
    measure: wgpu::RenderPipeline,
    limit: wgpu::RenderPipeline,
    present: wgpu::RenderPipeline,
    main_layout: wgpu::BindGroupLayout,
    scene_layout: wgpu::BindGroupLayout,
    final_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    fire_sampler: wgpu::Sampler,
    /// The campfire scene, made the first time it's shown and let go (about 4.5 GB of video
    /// memory) once it hasn't been for a while.
    campfire: Mutex<Option<Campfire>>,
    fire_seen: Option<std::time::Instant>,
    /// The newest frame shows the campfire.
    fire_on: AtomicBool,
    uniforms: wgpu::Buffer,
    vertices: wgpu::Buffer,
    vertex_capacity: usize,
    vertex_count: u32,
    measure_view: wgpu::TextureView,
    limited_views: [wgpu::TextureView; 2],
    /// [previous limited]
    limit_groups: [wgpu::BindGroup; 2],
    targets: Option<Targets>,
    srgb: bool,
    last_frame: u64,
    latest: AtomicUsize,
    latest_limited: AtomicUsize,
    /// Fixed-rate warp steps still to run for the newest engine frame.
    steps: AtomicU32,
    /// The newest frame still needs its scene, bloom, and limiter update.
    pending: AtomicBool,
    /// The feedback textures are new and their mips not built yet.
    fresh: AtomicBool,
    area: Rectangle,
}

fn entry(binding: u32, visibility: wgpu::ShaderStages, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { binding, visibility, ty, count: None }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        wgpu::ShaderStages::FRAGMENT,
        wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
    )
}

fn texture(device: &wgpu::Device, label: &str, size: (u32, u32), mips: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: mips,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: HDR,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

impl shader::Pipeline for Pipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Pipeline {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vibrance viz"),
            source: wgpu::ShaderSource::Wgsl(include_str!("viz.wgsl").into()),
        });
        let uniform_entry = entry(
            0,
            wgpu::ShaderStages::VERTEX_FRAGMENT,
            wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
        );
        let sampler_entry = entry(2, wgpu::ShaderStages::FRAGMENT, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering));
        let main_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viz main"),
            entries: &[uniform_entry, texture_entry(1), sampler_entry],
        });
        let limit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viz limit"),
            entries: &[uniform_entry, texture_entry(3), texture_entry(4)],
        });
        let final_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viz final"),
            entries: &[uniform_entry, texture_entry(1), sampler_entry, texture_entry(5), texture_entry(6)],
        });
        let fire_sampler_entry = entry(11, wgpu::ShaderStages::FRAGMENT, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering));
        let scene_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viz scene"),
            entries: &[uniform_entry, texture_entry(1), sampler_entry, texture_entry(8), fire_sampler_entry],
        });
        let layout = |l: &wgpu::BindGroupLayout| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[l], push_constant_ranges: &[] })
        };
        let (main_pl, limit_pl, final_pl) = (layout(&main_layout), layout(&limit_layout), layout(&final_layout));
        let scene_pl = layout(&scene_layout);

        let additive = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::SrcAlpha, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add };
        let sum = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Add };
        let full = |label: &str, pl: &wgpu::PipelineLayout, entry_point: &str, format: wgpu::TextureFormat, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(pl),
                vertex: wgpu::VertexState { module: &module, entry_point: Some("vs_full"), buffers: &[], compilation_options: Default::default() },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry_point),
                    targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        let warp = full("viz warp", &main_pl, "fs_warp", HDR, None);
        let down = full("viz downsample", &main_pl, "fs_down", HDR, None);
        let up = full("viz upsample", &main_pl, "fs_up", HDR, Some(wgpu::BlendState { color: sum, alpha: sum }));
        let scene = full("viz scene", &scene_pl, "fs_scene", HDR, None);
        let measure = full("viz measure", &main_pl, "fs_measure", HDR, None);
        let limit = full("viz limit", &limit_pl, "fs_limit", HDR, None);
        let present = full("viz final", &final_pl, "fs_final", format, None);

        let shape = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("viz shapes"),
            layout: Some(&main_pl),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_shape"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4, 2 => Float32x3],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_shape"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: HDR,
                    blend: Some(wgpu::BlendState { color: additive, alpha: additive }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // Mirror-repeat lets "wrap" presets fold the picture back in at the edges, and joins
        // the tunnel's texture seamlessly around its circumference.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("viz sampler"),
            address_mode_u: wgpu::AddressMode::MirrorRepeat,
            address_mode_v: wgpu::AddressMode::MirrorRepeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let fire_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("viz fire sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viz uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let vertex_capacity = 4096;
        let vertices = vertex_buffer(device, vertex_capacity);

        let small = |label: &str| texture(device, label, (3, 3), 1).create_view(&wgpu::TextureViewDescriptor::default());
        let measure_view = small("viz measured");
        let limited_views = [small("viz limited a"), small("viz limited b")];
        let limit_group = |prev: usize| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("viz limit"),
                layout: &limit_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&measure_view) },
                    wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&limited_views[prev]) },
                ],
            })
        };
        let limit_groups = [limit_group(0), limit_group(1)];

        Pipeline {
            warp,
            shape,
            down,
            up,
            scene,
            measure,
            limit,
            present,
            main_layout,
            scene_layout,
            final_layout,
            sampler,
            fire_sampler,
            campfire: Mutex::new(None),
            fire_seen: None,
            fire_on: AtomicBool::new(false),
            uniforms,
            vertices,
            vertex_capacity,
            vertex_count: 0,
            measure_view,
            limited_views,
            limit_groups,
            targets: None,
            srgb: format.is_srgb(),
            last_frame: u64::MAX,
            latest: AtomicUsize::new(0),
            latest_limited: AtomicUsize::new(0),
            steps: AtomicU32::new(0),
            pending: AtomicBool::new(false),
            fresh: AtomicBool::new(false),
            area: Rectangle::default(),
        }
    }
}

fn vertex_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viz vertices"),
        size: (capacity * std::mem::size_of::<Vertex>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn pass<'a>(encoder: &'a mut wgpu::CommandEncoder, label: &str, view: &'a wgpu::TextureView, load: wgpu::LoadOp<wgpu::Color>) -> wgpu::RenderPass<'a> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    })
}

fn draw(encoder: &mut wgpu::CommandEncoder, label: &str, view: &wgpu::TextureView, load: wgpu::LoadOp<wgpu::Color>, pipeline: &wgpu::RenderPipeline, group: &wgpu::BindGroup) {
    let mut p = pass(encoder, label, view, load);
    p.set_pipeline(pipeline);
    p.set_bind_group(0, group, &[]);
    p.draw(0..3, 0..1);
}

impl Pipeline {
    fn group(&self, device: &wgpu::Device, view: &wgpu::TextureView) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viz main"),
            layout: &self.main_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.uniforms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        })
    }

    fn chain(&self, device: &wgpu::Device, label: &str, size: (u32, u32)) -> Chain {
        let mips = MIPS.min(32 - size.0.max(size.1).leading_zeros());
        let tex = texture(device, label, size, mips);
        let levels: Vec<_> = (0..mips)
            .map(|level| tex.create_view(&wgpu::TextureViewDescriptor { base_mip_level: level, mip_level_count: Some(1), ..Default::default() }))
            .collect();
        let full = tex.create_view(&wgpu::TextureViewDescriptor::default());
        Chain { level_groups: levels.iter().map(|v| self.group(device, v)).collect(), group: self.group(device, &full), levels, full }
    }

    fn ensure_targets(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, size: (u32, u32)) {
        if self.targets.as_ref().is_some_and(|t| t.size == size) {
            return;
        }
        let feedback = [self.chain(device, "viz feedback a", size), self.chain(device, "viz feedback b", size)];
        let scene = self.chain(device, "viz scene", size);
        let fire = texture(device, "viz campfire", size, 1).create_view(&wgpu::TextureViewDescriptor::default());
        let scenes = [0, 1].map(|i| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("viz scene"),
                layout: &self.scene_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.uniforms.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&feedback[i].full) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::TextureView(&fire) },
                    wgpu::BindGroupEntry { binding: 11, resource: wgpu::BindingResource::Sampler(&self.fire_sampler) },
                ],
            })
        });
        let finals = [0, 1].map(|k| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("viz final"),
                layout: &self.final_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.uniforms.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&scene.full) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&self.measure_view) },
                    wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&self.limited_views[k]) },
                ],
            })
        });
        // New textures start black; the limiter restarts from what it next measures.
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("viz clear") });
        let all = feedback.iter().chain([&scene]).flat_map(|c| c.levels.iter());
        for view in all.chain(&self.limited_views).chain([&self.measure_view, &fire]) {
            drop(pass(&mut encoder, "viz clear", view, wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)));
        }
        queue.submit([encoder.finish()]);
        self.targets = Some(Targets { size, feedback, scene, fire, scenes, finals });
        self.latest.store(0, Ordering::Relaxed);
        self.fresh.store(true, Ordering::Relaxed);
        self.pending.store(true, Ordering::Relaxed);
    }

    /// Each level from the one above it.
    fn downsample(&self, encoder: &mut wgpu::CommandEncoder, chain: &Chain) {
        for level in 1..chain.levels.len() {
            draw(encoder, "viz downsample", &chain.levels[level], wgpu::LoadOp::Clear(wgpu::Color::BLACK), &self.down, &chain.level_groups[level - 1]);
        }
    }

    /// Bloom: blur each level up into the next larger one, so level 1 sums every scale.
    fn upsample(&self, encoder: &mut wgpu::CommandEncoder, chain: &Chain) {
        for level in (1..chain.levels.len().saturating_sub(1)).rev() {
            draw(encoder, "viz bloom", &chain.levels[level], wgpu::LoadOp::Load, &self.up, &chain.level_groups[level + 1]);
        }
    }
}

impl shader::Primitive for Primitive {
    type Pipeline = Pipeline;

    fn prepare(&self, pipeline: &mut Pipeline, device: &wgpu::Device, queue: &wgpu::Queue, bounds: &Rectangle, viewport: &Viewport) {
        // Whole device pixels, so a full-size scene maps one texel to one pixel.
        let scale = viewport.scale_factor();
        let (x0, y0) = ((bounds.x * scale).round(), (bounds.y * scale).round());
        let (x1, y1) = (((bounds.x + bounds.width) * scale).round(), ((bounds.y + bounds.height) * scale).round());
        let area = Rectangle { x: x0, y: y0, width: (x1 - x0).max(1.0), height: (y1 - y0).max(1.0) };
        pipeline.area = area;
        let shrink = (MAX_PIXELS / (area.width * area.height)).sqrt().min(1.0);
        let size = (((area.width * shrink).round() as u32).max(1), ((area.height * shrink).round() as u32).max(1));
        pipeline.ensure_targets(device, queue, size);

        if self.frame.id == pipeline.last_frame || self.frame.id == 0 {
            return;
        }
        pipeline.last_frame = self.frame.id;
        let mut uniforms = self.frame.uniforms;
        uniforms.frame[0] = size.0 as f32;
        uniforms.frame[1] = size.1 as f32;
        uniforms.frame[3] = size.0 as f32 / size.1 as f32;
        uniforms.safety[1] = if pipeline.srgb { 1.0 } else { 0.0 };
        // Sharpen what was upscaled, and a touch at full size.
        uniforms.safety[2] = if shrink < 1.0 { 0.35 } else { 0.12 };
        queue.write_buffer(&pipeline.uniforms, 0, bytemuck::bytes_of(&uniforms));

        let verts = &self.frame.vertices;
        if verts.len() > pipeline.vertex_capacity {
            pipeline.vertex_capacity = verts.len().next_power_of_two();
            pipeline.vertices = vertex_buffer(device, pipeline.vertex_capacity);
        }
        if !verts.is_empty() {
            queue.write_buffer(&pipeline.vertices, 0, bytemuck::cast_slice(verts));
        }
        pipeline.vertex_count = verts.len() as u32;
        pipeline.steps.store(self.frame.steps, Ordering::Relaxed);
        let fire_on = uniforms.fire[0] > 0.001;
        pipeline.fire_on.store(fire_on, Ordering::Relaxed);
        let campfire = pipeline.campfire.get_mut().unwrap_or_else(|e| e.into_inner());
        if fire_on {
            pipeline.fire_seen = Some(std::time::Instant::now());
            campfire.get_or_insert_with(|| Campfire::new(device, queue)).prepare(device, queue, &self.frame.campfire, size, self.frame.steps);
        } else if pipeline.fire_seen.is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(30)) {
            *campfire = None;
            pipeline.fire_seen = None;
        }
        pipeline.pending.store(true, Ordering::Relaxed);
    }

    fn render(&self, pipeline: &Pipeline, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, clip_bounds: &Rectangle<u32>) {
        // Nothing to show until the engine's first frame (an empty one has no settings).
        let Some(t) = pipeline.targets.as_ref().filter(|_| pipeline.last_frame != u64::MAX) else { return };
        let mut latest = pipeline.latest.load(Ordering::Relaxed);

        // The feedback loop advances in fixed 60 Hz steps, however often iced redraws.
        let steps = pipeline.steps.swap(0, Ordering::Relaxed);
        for _ in 0..steps {
            let dst = 1 - latest;
            let mut p = pass(encoder, "viz warp + shapes", &t.feedback[dst].levels[0], wgpu::LoadOp::Clear(wgpu::Color::BLACK));
            p.set_pipeline(&pipeline.warp);
            p.set_bind_group(0, &t.feedback[latest].group, &[]);
            p.draw(0..3, 0..1);
            if pipeline.vertex_count > 0 {
                // Shapes reuse the source group for the uniforms; it never aliases `dst`.
                p.set_pipeline(&pipeline.shape);
                p.set_vertex_buffer(0, pipeline.vertices.slice(..));
                p.draw(0..pipeline.vertex_count, 0..1);
            }
            drop(p);
            latest = dst;
        }
        pipeline.latest.store(latest, Ordering::Relaxed);
        if steps > 0 || pipeline.fresh.swap(false, Ordering::Relaxed) {
            pipeline.downsample(encoder, &t.feedback[latest]);
        }

        // The 3D view moves every displayed frame, even between feedback steps.
        let mut limited = pipeline.latest_limited.load(Ordering::Relaxed);
        if pipeline.pending.swap(false, Ordering::Relaxed) {
            if pipeline.fire_on.load(Ordering::Relaxed) {
                let mut campfire = pipeline.campfire.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(campfire) = campfire.as_mut() {
                    campfire.simulate(encoder, steps);
                    campfire.render(encoder, &t.fire);
                }
            }
            draw(encoder, "viz scene", &t.scene.levels[0], wgpu::LoadOp::Clear(wgpu::Color::BLACK), &pipeline.scene, &t.scenes[latest]);
            pipeline.downsample(encoder, &t.scene);
            // The limiter reads the smallest level, which bloom leaves as it is.
            draw(encoder, "viz measure", &pipeline.measure_view, wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), &pipeline.measure, &t.scene.group);
            let next = 1 - limited;
            draw(encoder, "viz limit", &pipeline.limited_views[next], wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), &pipeline.limit, &pipeline.limit_groups[limited]);
            limited = next;
            pipeline.latest_limited.store(limited, Ordering::Relaxed);
            pipeline.upsample(encoder, &t.scene);
        }

        let mut p = pass(encoder, "viz final", target, wgpu::LoadOp::Load);
        let a = pipeline.area;
        p.set_viewport(a.x, a.y, a.width, a.height, 0.0, 1.0);
        p.set_scissor_rect(clip_bounds.x, clip_bounds.y, clip_bounds.width.max(1), clip_bounds.height.max(1));
        p.set_pipeline(&pipeline.present);
        p.set_bind_group(0, &t.finals[limited], &[]);
        p.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use iced::widget::shader::{Pipeline as _, Primitive as _};
    use iced::{Point, Size};
    use vibrance_audio::Analysis;

    use super::*;
    use crate::viz::{Engine, PRESETS};

    /// The whole visualizer headless: every preset renders without a GPU validation error,
    /// which wgpu treats as fatal. Needs a GPU: `cargo test -p vibrance -- --ignored presets_render`.
    #[test]
    #[ignore]
    fn presets_render() {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends: wgpu::Backends::VULKAN, ..Default::default() });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() })).expect("adapter");
        let limits = wgpu::Limits { max_bind_groups: 2, max_non_sampler_bindings: 2048, ..wgpu::Limits::default() };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { required_limits: limits, ..Default::default() })).expect("device");
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let mut pipeline = Pipeline::new(&device, &queue, format);
        // `VIZ_PNG`, `VIZ_PRESET`, `VIZ_SECONDS` and `VIZ_SIZE` ("WxH") save one preset's
        // frame through the whole pipeline instead; `VIZ_HZ` sets the display rate, and
        // `VIZ_TIMING` prints how long the last two seconds' frames took (submitted one by one).
        let only = std::env::var("VIZ_PRESET").ok();
        let hz: f32 = std::env::var("VIZ_HZ").ok().and_then(|s| s.parse().ok()).unwrap_or(60.0);
        let timing = std::env::var_os("VIZ_TIMING").is_some();
        let mut times = Vec::new();
        let seconds: f32 = std::env::var("VIZ_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(8.0 / 60.0);
        let (w, h) = std::env::var("VIZ_SIZE")
            .ok()
            .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
            .unwrap_or((800u32, 450u32));
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let viewport = Viewport::with_physical_size(Size::new(w, h), 1.0);
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(w as f32, h as f32));
        let clip = Rectangle { x: 0, y: 0, width: w, height: h };
        // Music at 120 BPM: a beat every half second.
        let music = |frame: usize| Analysis { bass: 1.0, mid: 1.0, treble: 0.9, bars: vec![0.45; 32], bpm: Some(120.0), beat: frame.is_multiple_of(30), beat_strength: 0.7, ..Analysis::default() };
        let mut engine = Engine::new(true);
        for preset in PRESETS.iter().filter(|p| only.as_deref().is_none_or(|o| o == p.name)) {
            engine.select(preset.name);
            let frames = (seconds * hz) as usize;
            for frame in 0..frames {
                let started = std::time::Instant::now();
                let primitive = Primitive { frame: engine.update(1.0 / hz, Some(&music(frame * 60 / hz as usize)), w as f32 / h as f32) };
                primitive.prepare(&mut pipeline, &device, &queue, &bounds, &viewport);
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                primitive.render(&pipeline, &mut encoder, &view, &clip);
                queue.submit([encoder.finish()]);
                // Let the campfire's assets arrive the way they would while it burns.
                device.poll(wgpu::PollType::Poll).ok();
                if timing && frame + 2 * hz as usize >= frames {
                    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
                    // The first only drains what was queued before.
                    if frame + 2 * hz as usize > frames {
                        times.push(started.elapsed().as_secs_f64() * 1000.0);
                    }
                }
            }
        }
        device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
        if timing && !times.is_empty() {
            let mean = times.iter().sum::<f64>() / times.len() as f64;
            let mut sorted = times.clone();
            sorted.sort_by(f64::total_cmp);
            eprintln!(
                "frames: mean {mean:.2} ms, median {:.2}, 90% {:.2}, worst {:.2}; first ten {:?}",
                sorted[sorted.len() / 2],
                sorted[sorted.len() * 9 / 10],
                sorted[sorted.len() - 1],
                times.iter().take(10).map(|t| (t * 10.0).round() / 10.0).collect::<Vec<_>>()
            );
        }
        if let Ok(path) = std::env::var("VIZ_PNG") {
            let row = (w * 4).div_ceil(256) * 256;
            let buffer = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (row * h) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None } },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            queue.submit([encoder.finish()]);
            buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
            let data = buffer.slice(..).get_mapped_range();
            let mut img = image::RgbaImage::new(w, h);
            for y in 0..h {
                let start = (y * row) as usize;
                for x in 0..w {
                    let o = start + x as usize * 4;
                    img.put_pixel(x, y, image::Rgba([data[o], data[o + 1], data[o + 2], 255]));
                }
            }
            img.save(path).expect("save");
        }
    }
}
