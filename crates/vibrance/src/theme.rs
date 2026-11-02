//! Material 3 color roles, type scale, and shapes.

use iced::font::{Family, Weight};
use iced::{Color, Font};
use material_colors::color::Rgb;
use material_colors::dynamic_color::color_spec::SpecVersion;
use material_colors::dynamic_color::variant::Variant;
use material_colors::scheme::Scheme;
use material_colors::theme::ThemeBuilder;
use serde::{Deserialize, Serialize};

pub const FONT: Font = Font::with_name("Roboto Flex");
pub const FONT_MEDIUM: Font = Font { weight: Weight::Medium, ..FONT };
pub const FONT_SEMIBOLD: Font = Font { weight: Weight::Semibold, ..FONT };
pub const ICONS: Font = Font {
    family: Family::Name("Material Symbols Rounded Filled"),
    ..Font::DEFAULT
};

pub const FONT_BYTES: [&[u8]; 4] = [
    include_bytes!("../../../assets/fonts/RobotoFlex-400.ttf"),
    include_bytes!("../../../assets/fonts/RobotoFlex-500.ttf"),
    include_bytes!("../../../assets/fonts/RobotoFlex-600.ttf"),
    include_bytes!("../../../assets/fonts/MaterialSymbolsRounded-Filled.ttf"),
];

/// Sizes from the M3 type scale: (font size, line height).
pub mod scale {
    pub const TITLE_LARGE: (f32, f32) = (22.0, 28.0);
    pub const TITLE_MEDIUM: (f32, f32) = (16.0, 24.0);
    pub const TITLE_SMALL: (f32, f32) = (14.0, 20.0);
    pub const BODY_LARGE: (f32, f32) = (16.0, 24.0);
    pub const BODY_MEDIUM: (f32, f32) = (14.0, 20.0);
    pub const LABEL_MEDIUM: (f32, f32) = (12.0, 16.0);
}

/// M3 shape scale, in logical pixels.
pub mod shape {
    pub const SMALL: f32 = 8.0;
    pub const MEDIUM: f32 = 12.0;
    pub const LARGE: f32 = 16.0;
}

/// State-layer opacities.
pub mod state {
    pub const HOVER: f32 = 0.08;
    pub const PRESSED: f32 = 0.10;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PaletteChoice {
    #[default]
    Vibrance,
    Tide,
    Ember,
    Moss,
    Graphite,
    AlbumArt,
}

impl PaletteChoice {
    pub const ALL: [PaletteChoice; 6] = [
        PaletteChoice::Vibrance,
        PaletteChoice::Tide,
        PaletteChoice::Ember,
        PaletteChoice::Moss,
        PaletteChoice::Graphite,
        PaletteChoice::AlbumArt,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PaletteChoice::Vibrance => "Vibrance",
            PaletteChoice::Tide => "Tide",
            PaletteChoice::Ember => "Ember",
            PaletteChoice::Moss => "Moss",
            PaletteChoice::Graphite => "Graphite",
            PaletteChoice::AlbumArt => "Album Art",
        }
    }

    /// Seed and variant. The album-art seed comes from the cover at runtime.
    pub fn seed(self, art_seed: Option<Rgb>) -> (Rgb, Variant) {
        match self {
            PaletteChoice::Vibrance => (Rgb::from_u32(0xA23CF0), Variant::Vibrant),
            PaletteChoice::Tide => (Rgb::from_u32(0x0091C2), Variant::TonalSpot),
            PaletteChoice::Ember => (Rgb::from_u32(0xE8590C), Variant::TonalSpot),
            PaletteChoice::Moss => (Rgb::from_u32(0x5A8F29), Variant::TonalSpot),
            PaletteChoice::Graphite => (Rgb::from_u32(0x808080), Variant::Monochrome),
            PaletteChoice::AlbumArt => match art_seed {
                Some(seed) => (seed, Variant::Content),
                None => PaletteChoice::Vibrance.seed(None),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ColorMode {
    #[default]
    Dark,
    Light,
    System,
}

/// The M3 color roles Vibrance uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    pub primary: Color,
    pub on_primary: Color,
    pub primary_container: Color,
    pub on_primary_container: Color,
    pub secondary_container: Color,
    pub on_secondary_container: Color,
    pub tertiary: Color,
    pub surface: Color,
    pub surface_container_lowest: Color,
    pub surface_container_low: Color,
    pub surface_container: Color,
    pub surface_container_high: Color,
    pub surface_container_highest: Color,
    pub on_surface: Color,
    pub on_surface_variant: Color,
    pub outline: Color,
    pub outline_variant: Color,
    pub inverse_surface: Color,
    pub inverse_on_surface: Color,
    pub error: Color,
    pub scrim: Color,
}

fn c(rgb: Rgb) -> Color {
    Color::from_rgb8(rgb.red, rgb.green, rgb.blue)
}

impl Colors {
    pub fn generate(seed: Rgb, variant: Variant, dark: bool) -> Colors {
        let theme = ThemeBuilder::with_source(seed)
            .variant(variant)
            .spec_version(SpecVersion::Spec2025)
            .contrast_level(0.0)
            .build();
        let s: &Scheme = if dark { &theme.schemes.dark } else { &theme.schemes.light };
        Colors {
            primary: c(s.primary),
            on_primary: c(s.on_primary),
            primary_container: c(s.primary_container),
            on_primary_container: c(s.on_primary_container),
            secondary_container: c(s.secondary_container),
            on_secondary_container: c(s.on_secondary_container),
            tertiary: c(s.tertiary),
            surface: c(s.surface),
            surface_container_lowest: c(s.surface_container_lowest),
            surface_container_low: c(s.surface_container_low),
            surface_container: c(s.surface_container),
            surface_container_high: c(s.surface_container_high),
            surface_container_highest: c(s.surface_container_highest),
            on_surface: c(s.on_surface),
            on_surface_variant: c(s.on_surface_variant),
            outline: c(s.outline),
            outline_variant: c(s.outline_variant),
            inverse_surface: c(s.inverse_surface),
            inverse_on_surface: c(s.inverse_on_surface),
            error: c(s.error),
            scrim: c(s.scrim),
        }
    }

    /// Linear blend toward `other`, used for the 300 ms palette crossfade.
    pub fn mix(&self, other: &Colors, t: f32) -> Colors {
        let m = |a: Color, b: Color| {
            Color::from_rgba(
                a.r + (b.r - a.r) * t,
                a.g + (b.g - a.g) * t,
                a.b + (b.b - a.b) * t,
                a.a + (b.a - a.a) * t,
            )
        };
        Colors {
            primary: m(self.primary, other.primary),
            on_primary: m(self.on_primary, other.on_primary),
            primary_container: m(self.primary_container, other.primary_container),
            on_primary_container: m(self.on_primary_container, other.on_primary_container),
            secondary_container: m(self.secondary_container, other.secondary_container),
            on_secondary_container: m(self.on_secondary_container, other.on_secondary_container),
            tertiary: m(self.tertiary, other.tertiary),
            surface: m(self.surface, other.surface),
            surface_container_lowest: m(self.surface_container_lowest, other.surface_container_lowest),
            surface_container_low: m(self.surface_container_low, other.surface_container_low),
            surface_container: m(self.surface_container, other.surface_container),
            surface_container_high: m(self.surface_container_high, other.surface_container_high),
            surface_container_highest: m(self.surface_container_highest, other.surface_container_highest),
            on_surface: m(self.on_surface, other.on_surface),
            on_surface_variant: m(self.on_surface_variant, other.on_surface_variant),
            outline: m(self.outline, other.outline),
            outline_variant: m(self.outline_variant, other.outline_variant),
            inverse_surface: m(self.inverse_surface, other.inverse_surface),
            inverse_on_surface: m(self.inverse_on_surface, other.inverse_on_surface),
            error: m(self.error, other.error),
            scrim: m(self.scrim, other.scrim),
        }
    }
}

/// `color` at `alpha` opacity, for state layers.
pub fn alpha(color: Color, alpha: f32) -> Color {
    Color { a: color.a * alpha, ..color }
}

/// A state layer of `content` over `base`, flattened to one opaque color.
pub fn layer(base: Color, content: Color, opacity: f32) -> Color {
    Color::from_rgba(
        base.r + (content.r - base.r) * opacity,
        base.g + (content.g - base.g) * opacity,
        base.b + (content.b - base.b) * opacity,
        base.a,
    )
}

/// Seed color for the Album Art palette: Celebi quantizer + score.
pub fn art_seed(image: &image::RgbaImage) -> Rgb {
    let small = image::imageops::thumbnail(image, 112, 112);
    let pixels: Vec<Rgb> = small
        .pixels()
        .filter(|p| p.0[3] > 0)
        .map(|p| Rgb::new(p.0[0], p.0[1], p.0[2]))
        .collect();
    material_colors::image::extract_color(&pixels)
}
