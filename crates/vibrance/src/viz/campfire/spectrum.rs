//! Colours of light from physics: the blackbody glow of hot soot by temperature (Planck's
//! law seen through the CIE 1931 observer), and the emission lines of salts in a flame.

/// Range of the blackbody table, in kelvin.
pub const LUT_MIN: f32 = 500.0;
pub const LUT_MAX: f32 = 2600.0;
pub const LUT_SIZE: usize = 256;

/// CIE 1931 colour matching functions, from the multi-lobe Gaussian fit in Wyman, Sloan &
/// Shirley, "Simple Analytic Approximations to the CIE XYZ Color Matching Functions" (JCGT 2013).
fn cie(nm: f64) -> [f64; 3] {
    let g = |mu: f64, below: f64, above: f64| {
        let t = (nm - mu) / if nm < mu { below } else { above };
        (-0.5 * t * t).exp()
    };
    [
        1.056 * g(599.8, 37.9, 31.0) + 0.362 * g(442.0, 16.0, 26.7) - 0.065 * g(501.1, 20.4, 26.2),
        0.821 * g(568.8, 46.9, 40.5) + 0.286 * g(530.9, 16.3, 31.1),
        1.217 * g(437.0, 11.8, 36.0) + 0.681 * g(459.0, 26.0, 13.8),
    ]
}

fn xyz_to_linear_srgb([x, y, z]: [f64; 3]) -> [f64; 3] {
    [3.2406 * x - 1.5372 * y - 0.4986 * z, -0.9689 * x + 1.8758 * y + 0.0415 * z, 0.0557 * x - 0.2040 * y + 1.0570 * z]
}

/// Planck's spectral radiance (W·sr⁻¹·m⁻³) at a wavelength in nm and a temperature in K.
fn planck(nm: f64, kelvin: f64) -> f64 {
    const C1: f64 = 1.191_042_972e-16; // 2hc²
    const C2: f64 = 1.438_776_877e-2; // hc/k
    let l = nm * 1e-9;
    C1 / (l.powi(5) * ((C2 / (l * kelvin)).exp() - 1.0))
}

fn blackbody_xyz(kelvin: f64) -> [f64; 3] {
    let mut xyz = [0.0; 3];
    let mut nm = 380.0;
    while nm <= 780.0 {
        let b = planck(nm, kelvin);
        let c = cie(nm);
        for i in 0..3 {
            xyz[i] += b * c[i];
        }
        nm += 2.0;
    }
    xyz
}

/// Blackbody radiance in linear sRGB from `LUT_MIN` to `LUT_MAX` kelvin, scaled so 1500 K has
/// a luminance of 1. The range is physical: 1000 K is about a thousandth of that, 2000 K
/// about ten times, which is what gives flames their crisp edges and glowing cores.
pub fn blackbody_table() -> Vec<[f32; 4]> {
    let reference = blackbody_xyz(1500.0)[1];
    (0..LUT_SIZE)
        .map(|i| {
            let kelvin = LUT_MIN as f64 + (LUT_MAX - LUT_MIN) as f64 * i as f64 / (LUT_SIZE - 1) as f64;
            let xyz = blackbody_xyz(kelvin).map(|v| v / reference);
            let rgb = xyz_to_linear_srgb(xyz).map(|v| v.max(0.0) as f32);
            [rgb[0], rgb[1], rgb[2], xyz[1] as f32]
        })
        .collect()
}

/// The colour of a set of emission lines (nm), brought into the sRGB gamut by mixing toward
/// grey of the same luminance, and scaled to a luminance of 1.
pub fn lines(nm: &[f64]) -> [f32; 3] {
    let mut xyz = [0.0; 3];
    for &l in nm {
        let c = cie(l);
        for i in 0..3 {
            xyz[i] += c[i];
        }
    }
    let y = xyz[1].max(1e-9);
    let rgb = xyz_to_linear_srgb(xyz.map(|v| v / y));
    let low = rgb.iter().cloned().fold(f64::INFINITY, f64::min);
    // Most spectral colours lie outside sRGB; desaturate just enough to fit.
    let k = if low < 0.0 { 1.0 / (1.0 - low) } else { 1.0 };
    let rgb = rgb.map(|v| 1.0 + (v - 1.0) * k);
    [rgb[0] as f32, rgb[1] as f32, rgb[2] as f32]
}

/// Salts and the lines they emit in a flame (nm).
pub const COPPER_CHLORIDE: &[f64] = &[428.9, 435.8, 443.9, 452.0];
pub const COPPER: &[f64] = &[510.5, 515.3, 521.8];
pub const BORON: &[f64] = &[518.0, 548.0];
pub const BARIUM: &[f64] = &[513.9, 524.2];
pub const CESIUM: &[f64] = &[455.5, 459.3];
pub const POTASSIUM: &[f64] = &[404.4, 766.5, 769.9];
pub const LITHIUM: &[f64] = &[670.8];
pub const STRONTIUM: &[f64] = &[606.0, 646.0, 659.0, 681.0];
pub const INDIUM: &[f64] = &[410.2, 451.1];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotter_is_brighter_and_whiter() {
        let lut = blackbody_table();
        let at = |k: f32| lut[((k - LUT_MIN) / (LUT_MAX - LUT_MIN) * (LUT_SIZE - 1) as f32).round() as usize];
        let (warm, hot) = (at(1000.0), at(2000.0));
        assert!(hot[3] > warm[3] * 100.0, "brightness rises steeply with temperature");
        assert!(hot[1] / hot[0] > warm[1] / warm[0], "and the glow turns from red toward yellow");
        assert!((at(1500.0)[3] - 1.0).abs() < 0.05);
    }

    #[test]
    fn salts_have_their_colours() {
        let [r, g, b] = lines(COPPER);
        assert!(g > r && g > b, "copper burns green");
        let [r, g, b] = lines(LITHIUM);
        assert!(r > g && r > b, "lithium burns red");
        let [r, _, b] = lines(CESIUM);
        assert!(b > r, "cesium burns blue");
    }
}
