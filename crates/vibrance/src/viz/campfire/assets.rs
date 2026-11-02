//! The campfire's photo-scanned assets (CC0, from Poly Haven; `scripts/fetch-campfire-assets.py`
//! downloads them), decoded with full mip chains on background threads.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use bytemuck::{Pod, Zeroable};
use image::imageops::FilterType;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

/// An RGBA8 image and its mip chain, largest first.
pub struct Texture {
    pub size: (u32, u32),
    pub levels: Vec<Vec<u8>>,
}

/// Albedo, normal (OpenGL convention) and ambient-occlusion/roughness/metal maps.
pub struct Material {
    pub albedo: Texture,
    pub normal: Texture,
    pub arm: Texture,
}

pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub material: Material,
}

/// An equirectangular HDR sky as RGBA half floats.
pub struct Sky {
    pub size: (u32, u32),
    pub texels: Vec<half::f16>,
}

pub struct Assets {
    pub pit: Mesh,
    pub log: Mesh,
    pub big_log: Mesh,
    pub forest_floor: Material,
    pub sky: Sky,
}

/// Where the assets live: `$VIBRANCE_SCENES`, or the user's data directory (shared by every
/// profile, so `VIBRANCE_HOME` test runs find them too).
pub fn dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VIBRANCE_SCENES") {
        return Some(PathBuf::from(dir).join("campfire"));
    }
    directories::ProjectDirs::from("", "", "vibrance").map(|d| d.data_dir().join("scenes/campfire"))
}

/// Loads in the background; poll until it's done.
pub struct Loader {
    rx: mpsc::Receiver<Result<Assets, String>>,
}

impl Loader {
    pub fn start() -> Loader {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("campfire assets".into())
            .spawn(move || {
                let started = std::time::Instant::now();
                let result = dir().ok_or_else(|| "no data directory".to_owned()).and_then(|d| load(&d));
                match &result {
                    Ok(_) => log::info!("campfire assets loaded in {:?}", started.elapsed()),
                    Err(err) => log::warn!("campfire assets unavailable ({err}); run scripts/fetch-campfire-assets.py"),
                }
                let _ = tx.send(result);
            })
            .expect("spawn asset loader");
        Loader { rx }
    }

    /// `Some` once loading has finished, successfully or not.
    pub fn poll(&self) -> Option<Result<Assets, String>> {
        self.rx.try_recv().ok()
    }
}

fn load(dir: &Path) -> Result<Assets, String> {
    // Decoding and mipping a dozen 4K maps is the slow part; do them side by side.
    std::thread::scope(|scope| {
        let pit = scope.spawn(|| model(&dir.join("stone_fire_pit/stone_fire_pit.gltf")));
        let log = scope.spawn(|| model(&dir.join("dead_tree_trunk/dead_tree_trunk.gltf")));
        let big_log = scope.spawn(|| model(&dir.join("dead_tree_trunk_02/dead_tree_trunk_02.gltf")));
        let forest_floor = scope.spawn(|| material(&dir.join("forrest_ground_03")));
        let sky = scope.spawn(|| sky(&dir.join("satara_night_no_lamps/sky.hdr")));
        Ok(Assets {
            pit: join(pit)?,
            log: join(log)?,
            big_log: join(big_log)?,
            forest_floor: join(forest_floor)?,
            sky: join(sky)?,
        })
    })
}

fn join<T>(handle: std::thread::ScopedJoinHandle<'_, Result<T, String>>) -> Result<T, String> {
    handle.join().map_err(|_| "loader panicked".to_owned())?
}

fn model(path: &Path) -> Result<Mesh, String> {
    let (doc, buffers, images) = gltf::import(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut textures = None;
    for mesh in doc.meshes() {
        for prim in mesh.primitives() {
            let reader = prim.reader(|b| Some(&buffers[b.index()]));
            let base = vertices.len() as u32;
            let positions = reader.read_positions().ok_or("mesh without positions")?;
            let normals: Vec<[f32; 3]> = reader.read_normals().ok_or("mesh without normals")?.collect();
            let uvs: Vec<[f32; 2]> = reader.read_tex_coords(0).ok_or("mesh without UVs")?.into_f32().collect();
            for (i, pos) in positions.enumerate() {
                vertices.push(Vertex { pos, normal: normals[i], uv: uvs[i] });
            }
            match reader.read_indices() {
                Some(ix) => indices.extend(ix.into_u32().map(|i| i + base)),
                None => indices.extend(base..vertices.len() as u32),
            }
            if textures.is_none() {
                let m = prim.material();
                let pbr = m.pbr_metallic_roughness();
                let image = |t: Option<gltf::texture::Texture<'_>>| t.map(|t| t.source().index());
                textures = Some((
                    image(pbr.base_color_texture().map(|i| i.texture())),
                    image(m.normal_texture().map(|i| i.texture())),
                    image(pbr.metallic_roughness_texture().map(|i| i.texture())),
                ));
            }
        }
    }
    let (albedo, normal, arm) = textures.ok_or("model without a material")?;
    let tex = |i: Option<usize>| -> Result<Texture, String> {
        let data = &images[i.ok_or("missing texture")?];
        Ok(mipmapped(rgba(data)?))
    };
    Ok(Mesh { vertices, indices, material: Material { albedo: tex(albedo)?, normal: tex(normal)?, arm: tex(arm)? } })
}

fn rgba(data: &gltf::image::Data) -> Result<image::RgbaImage, String> {
    let px = match data.format {
        gltf::image::Format::R8G8B8A8 => data.pixels.clone(),
        gltf::image::Format::R8G8B8 => data.pixels.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        other => return Err(format!("unsupported texture format {other:?}")),
    };
    image::RgbaImage::from_raw(data.width, data.height, px).ok_or_else(|| "bad texture".to_owned())
}

fn material(dir: &Path) -> Result<Material, String> {
    let open = |name: &str| -> Result<Texture, String> {
        let path = dir.join(name);
        let img = image::open(&path).map_err(|e| format!("{}: {e}", path.display()))?.to_rgba8();
        Ok(mipmapped(img))
    };
    Ok(Material { albedo: open("diff.jpg")?, normal: open("nor_gl.jpg")?, arm: open("arm.jpg")? })
}

fn mipmapped(img: image::RgbaImage) -> Texture {
    let size = img.dimensions();
    let mut levels = vec![img];
    while let Some(last) = levels.last() {
        let (w, h) = last.dimensions();
        if w == 1 && h == 1 {
            break;
        }
        let next = image::imageops::resize(last, (w / 2).max(1), (h / 2).max(1), FilterType::Triangle);
        levels.push(next);
    }
    Texture { size, levels: levels.into_iter().map(|l| l.into_raw()).collect() }
}

fn sky(path: &Path) -> Result<Sky, String> {
    let img = image::open(path).map_err(|e| format!("{}: {e}", path.display()))?.to_rgb32f();
    let size = img.dimensions();
    let texels = img.pixels().flat_map(|p| [p[0], p[1], p[2], 1.0]).map(half::f16::from_f32).collect();
    Ok(Sky { size, texels })
}
