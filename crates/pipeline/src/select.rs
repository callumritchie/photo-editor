//! Automatic selections that produce mask rasters.
//!
//! **Sky** (this project's own heuristic; no trained model): on a reduced copy of the rendered
//! picture, score each pixel for "sky-likeness" (bright, blue or neutral rather than warm, low local
//! texture), grow a region from the top edge through sky-like pixels whose colour changes smoothly,
//! then refine the edge with a guided filter whose guide is lightness, so it follows branches and
//! rooflines. It works for ordinary daylight skies; night skies, sunsets with saturated colour and
//! skies not touching the top edge are not found.

use std::collections::VecDeque;

use crate::filter::Plane;
use crate::local::guided_coeffs;
use crate::masks::Raster;

/// Long edge of selection rasters, in pixels.
pub const SELECT_LONG_EDGE: usize = 512;

/// OKLab of an sRGB-encoded colour (0..1).
fn oklab_srgb(c: [f32; 3]) -> [f32; 3] {
    let l = c.map(pe_colour::srgb_decode);
    let lms = pe_colour::apply(&crate::look::srgb_to_lms(), l).map(f32::cbrt);
    pe_colour::apply(&crate::look::LMS_TO_LAB, lms)
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Sky mask for an sRGB-encoded image (interleaved, 0..1) of `w` x `h`, as a raster of at most
/// [`SELECT_LONG_EDGE`] on the long edge.
pub fn select_sky(rgb: &[f32], w: usize, h: usize) -> Raster {
    let scale = (SELECT_LONG_EDGE as f32 / w.max(h) as f32).min(1.0);
    let (sw, sh) = (
        ((w as f32 * scale).round() as usize).max(1),
        ((h as f32 * scale).round() as usize).max(1),
    );
    let small = crate::resample_area(rgb, w, h, sw, sh);
    let lab: Vec<[f32; 3]> = small
        .chunks(3)
        .map(|p| oklab_srgb([p[0], p[1], p[2]]))
        .collect();
    let n = sw * sh;
    // Local texture: mean absolute lightness difference to the 4 neighbours.
    let at = |x: usize, y: usize| lab[y * sw + x][0];
    let texture: Vec<f32> = (0..n)
        .map(|i| {
            let (x, y) = (i % sw, i / sw);
            let l = at(x, y);
            let nb = [
                at(x.saturating_sub(1), y),
                at((x + 1).min(sw - 1), y),
                at(x, y.saturating_sub(1)),
                at(x, (y + 1).min(sh - 1)),
            ];
            nb.iter().map(|v| (v - l).abs()).sum::<f32>() / 4.0
        })
        .collect();
    let score: Vec<f32> = (0..n)
        .map(|i| {
            let [l, a, b] = lab[i];
            let bright = smoothstep(0.45, 0.7, l);
            // Blue (b < 0) or near-neutral; penalise warm and green.
            let hue_ok = 1.0 - smoothstep(0.01, 0.06, b) * 1.0 - smoothstep(0.03, 0.08, -a) * 0.5;
            let smooth = 1.0 - smoothstep(0.01, 0.05, texture[i]);
            (bright * hue_ok.max(0.0) * smooth).clamp(0.0, 1.0)
        })
        .collect();
    // Region growing from sky-like pixels on the top edge.
    let mut region = vec![0.0f32; n];
    let mut queue = VecDeque::new();
    for x in 0..sw {
        if score[x] > 0.5 {
            region[x] = 1.0;
            queue.push_back(x);
        }
    }
    while let Some(i) = queue.pop_front() {
        let (x, y) = (i % sw, i / sw);
        let mut visit = |j: usize| {
            if region[j] == 0.0 && score[j] > 0.35 {
                let d = ((lab[i][0] - lab[j][0]).powi(2)
                    + (lab[i][1] - lab[j][1]).powi(2)
                    + (lab[i][2] - lab[j][2]).powi(2))
                .sqrt();
                if d < 0.04 {
                    region[j] = 1.0;
                    queue.push_back(j);
                }
            }
        };
        if x > 0 {
            visit(i - 1);
        }
        if x + 1 < sw {
            visit(i + 1);
        }
        if y > 0 {
            visit(i - sw);
        }
        if y + 1 < sh {
            visit(i + sw);
        }
    }
    // Edge refinement guided by lightness.
    let guide = Plane::new(sw, sh, lab.iter().map(|p| p[0]).collect());
    let input = Plane::new(sw, sh, region);
    let r = (sw.min(sh) / 40).max(2);
    let (ma, mb) = guided_coeffs(&guide, &input, r, 1e-3);
    let data = (0..n)
        .map(|i| (ma.data[i] * guide.data[i] + mb.data[i]).clamp(0.0, 1.0))
        .collect();
    Raster {
        width: sw,
        height: sh,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_blue_sky_above_a_textured_landscape() {
        let (w, h) = (300, 200);
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                if y < 90 {
                    // Sky: smooth blue gradient.
                    let t = y as f32 / 90.0;
                    rgb.extend([0.45 + 0.2 * t, 0.62 + 0.15 * t, 0.9]);
                } else {
                    // Ground: textured green/brown.
                    let n = ((x * 7 + y * 13) % 11) as f32 / 11.0;
                    rgb.extend([0.25 + 0.2 * n, 0.35 + 0.15 * n, 0.15]);
                }
            }
        }
        let m = select_sky(&rgb, w, h);
        assert!(
            m.sample(0.5, 0.1) > 0.9,
            "sky selected: {}",
            m.sample(0.5, 0.1)
        );
        assert!(m.sample(0.5, 0.3) > 0.9);
        assert!(
            m.sample(0.5, 0.8) < 0.1,
            "ground not: {}",
            m.sample(0.5, 0.8)
        );
        assert!(m.sample(0.2, 0.95) < 0.1);
    }

    #[test]
    fn bright_wall_not_touching_the_top_is_not_sky() {
        let (w, h) = (120, 80);
        let mut rgb = vec![0.2f32; w * h * 3];
        for y in 40..70 {
            for x in 20..100 {
                let i = (y * w + x) * 3;
                rgb[i..i + 3].copy_from_slice(&[0.8, 0.82, 0.85]);
            }
        }
        let m = select_sky(&rgb, w, h);
        assert!(m.sample(0.5, 0.7) < 0.1);
    }
}
