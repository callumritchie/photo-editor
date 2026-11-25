//! White balance: multipliers in camera space for each mode, and as-shot temperature/tint.

use crate::settings::WhiteBalance;
use pe_colour::{apply, cct, invert, Mat3};
use pe_raw::RawFrame;
use rayon::prelude::*;

/// Camera-space multipliers (green = 1) for the chosen mode. `cam_rgb` is the demosaiced,
/// pre-white-balance camera image (used by Auto).
pub fn multipliers(frame: &RawFrame, mode: &WhiteBalance, cam_rgb: &[f32]) -> [f32; 3] {
    match mode {
        WhiteBalance::AsShot => frame.wb,
        WhiteBalance::Auto => grey_world(cam_rgb).unwrap_or(frame.wb),
        WhiteBalance::Custom { temperature, tint } => {
            for_temperature(&frame.xyz_to_cam, *temperature, *tint).unwrap_or(frame.wb)
        }
    }
}

/// Multipliers that render a light source of the given temperature/tint as neutral.
pub fn for_temperature(xyz_to_cam: &Mat3, kelvin: f32, tint: f32) -> Option<[f32; 3]> {
    let [x, y] = cct::temp_tint_to_xy(kelvin, tint);
    let xyz = [(x / y) as f32, 1.0, ((1.0 - x - y) / y) as f32];
    let cam = apply(xyz_to_cam, xyz);
    if cam.iter().any(|v| *v <= 0.0) {
        return None;
    }
    Some([cam[1] / cam[0], 1.0, cam[1] / cam[2]])
}

/// Temperature/tint of the light the given multipliers neutralise (e.g. the camera's as-shot WB).
pub fn temperature_of(xyz_to_cam: &Mat3, wb: [f32; 3]) -> Option<(f32, f32)> {
    let neutral_cam = [1.0 / wb[0], 1.0 / wb[1], 1.0 / wb[2]];
    let xyz = apply(&invert(xyz_to_cam)?, neutral_cam);
    let s = xyz[0] + xyz[1] + xyz[2];
    if s <= 0.0 || xyz[1] <= 0.0 {
        return None;
    }
    Some(cct::xy_to_temp_tint([
        (xyz[0] / s) as f64,
        (xyz[1] / s) as f64,
    ]))
}

/// Grey-world estimate over well-exposed pixels (all channels between 2% and 90% of saturation).
fn grey_world(cam_rgb: &[f32]) -> Option<[f32; 3]> {
    let (sum, n) = cam_rgb
        .par_chunks(3)
        .filter(|p| p.iter().all(|v| (0.02..0.9).contains(v)))
        .map(|p| ([p[0] as f64, p[1] as f64, p[2] as f64], 1u64))
        .reduce(
            || ([0.0; 3], 0),
            |a, b| {
                (
                    [a.0[0] + b.0[0], a.0[1] + b.0[1], a.0[2] + b.0[2]],
                    a.1 + b.1,
                )
            },
        );
    if n < 100 || sum.iter().any(|v| *v <= 0.0) {
        return None;
    }
    Some([(sum[1] / sum[0]) as f32, 1.0, (sum[1] / sum[2]) as f32])
}

#[cfg(test)]
mod tests {
    use super::*;

    // A synthetic camera whose native space equals XYZ: neutral for D65 needs XYZ_D65 = (0.95, 1, 1.09).
    const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

    #[test]
    fn temperature_roundtrip_through_multipliers() {
        for (t, tint) in [(3200.0, 0.0), (5500.0, 10.0), (7500.0, -5.0)] {
            let wb = for_temperature(&IDENTITY, t, tint).unwrap();
            let (t2, tint2) = temperature_of(&IDENTITY, wb).unwrap();
            assert!((t2 - t).abs() / t < 0.01, "{t} -> {t2}");
            assert!((tint2 - tint).abs() < 1.0, "{tint} -> {tint2}");
        }
    }

    #[test]
    fn grey_world_neutralises_a_tinted_image() {
        let img: Vec<f32> = (0..1000)
            .flat_map(|i| {
                let v = 0.1 + (i % 10) as f32 * 0.05;
                [v * 0.5, v, v * 0.8]
            })
            .collect();
        let m = grey_world(&img).unwrap();
        assert!(
            (m[0] - 2.0).abs() < 1e-3 && (m[2] - 1.25).abs() < 1e-3,
            "{m:?}"
        );
    }
}
