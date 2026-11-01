//! Album art lookup: embedded pictures first, then cover images beside the file.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::imageops::FilterType;
use image::{DynamicImage, ImageReader, RgbaImage};
use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::probe::Probe;

/// Stems of folder images, in order of preference (compared case-insensitively).
const FOLDER_NAMES: &[&str] = &["cover", "folder", "front"];
const FOLDER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

pub(crate) fn load(track: &Path, max_px: u32) -> Option<RgbaImage> {
    embedded(track, max_px).or_else(|| {
        folder_images(track)
            .into_iter()
            .find_map(|path| decode_fit(&std::fs::read(path).ok()?, max_px))
    })
}

/// The front cover if there is one, otherwise the first embedded picture that decodes.
fn embedded(track: &Path, max_px: u32) -> Option<RgbaImage> {
    let options = ParseOptions::new().read_properties(false);
    let file = Probe::open(track)
        .ok()?
        .options(options)
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
    let pictures = file.tags().iter().flat_map(|t| t.pictures());
    let is_front = |p: &&lofty::picture::Picture| p.pic_type() == PictureType::CoverFront;
    pictures
        .clone()
        .filter(is_front)
        .chain(pictures.filter(|p| !is_front(p)))
        .find_map(|p| decode_fit(p.data(), max_px))
}

/// cover/folder/front images in the track's directory, best match first.
fn folder_images(track: &Path) -> Vec<PathBuf> {
    let Some(entries) = track.parent().and_then(|dir| std::fs::read_dir(dir).ok()) else {
        return Vec::new();
    };
    let mut found: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let (stem, ext) = name.to_str()?.rsplit_once('.')?;
            let rank = |list: &[&str], s: &str| list.iter().position(|n| n.eq_ignore_ascii_case(s));
            Some((
                rank(FOLDER_NAMES, stem)?,
                rank(FOLDER_EXTENSIONS, ext)?,
                entry.path(),
            ))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, _, path)| path).collect()
}

fn decode_fit(data: &[u8], max_px: u32) -> Option<RgbaImage> {
    let image = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    Some(fit(image, max_px).into_rgba8())
}

/// Downscales to fit within `max_px` × `max_px`, keeping the aspect ratio. Never upscales.
/// `max_px == 0` keeps the original size.
fn fit(image: DynamicImage, max_px: u32) -> DynamicImage {
    if max_px == 0 || (image.width() <= max_px && image.height() <= max_px) {
        return image;
    }
    // A box-filter pre-shrink makes big reductions cheap; Catmull-Rom does the final, sharp pass.
    let image = if image.width().max(image.height()) >= max_px.saturating_mul(4) {
        let pre = max_px.saturating_mul(2);
        image.thumbnail(pre, pre)
    } else {
        image
    };
    image.resize(max_px, max_px, FilterType::CatmullRom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_never_upscales() {
        let wide = DynamicImage::new_rgb8(1000, 500);
        let out = fit(wide, 100);
        assert_eq!((out.width(), out.height()), (100, 50));

        let big = DynamicImage::new_rgb8(3000, 3000);
        let out = fit(big, 300);
        assert_eq!((out.width(), out.height()), (300, 300));

        let small = DynamicImage::new_rgb8(64, 32);
        let out = fit(small, 512);
        assert_eq!((out.width(), out.height()), (64, 32));

        let out = fit(DynamicImage::new_rgb8(64, 32), 0);
        assert_eq!((out.width(), out.height()), (64, 32));
    }
}
