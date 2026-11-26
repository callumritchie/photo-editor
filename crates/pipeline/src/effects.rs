//! Effects on the final (display-linear, oriented) image: vignette and grain.
//!
//! Design (this project's own). Both work in output coordinates normalised to the image, so a reduced
//! preview and a full-size export look alike.
//! - **Vignette**: distance from the centre in coordinates where the frame spans -1..1 on each axis.
//!   Roundness 0 gives an ellipse fitted to the frame; + moves towards a circle (the long axis scaled
//!   by the aspect ratio); - towards a rounded rectangle (a p-norm with p up to 8). The mask rises from
//!   0 to 1 between an inner radius (Midpoint) and inner + width (Feather). Negative amounts darken
//!   (optionally sparing highlights), positive amounts lighten towards white.
//! - **Grain**: value noise on an integer lattice (hashed, so the CPU and GPU agree exactly on the
//!   lattice values), smoothly interpolated, two octaves mixed by Roughness. Cell size is a fraction of
//!   the short side (Size). Applied as a multiplicative change weighted to midtones.

use crate::Settings;

/// Grain octave frequency ratio and strength at amount 100.
pub const GRAIN_OCTAVE: f32 = 2.3;
pub const GRAIN_STRENGTH: f32 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectsParams {
    pub vignette: bool,
    pub amount: f32,
    pub inner: f32,
    pub width: f32,
    pub highlights: f32,
    /// Axis scales and norm exponent for the vignette distance.
    pub sx: f32,
    pub sy: f32,
    pub p: f32,
    pub grain: bool,
    pub grain_amount: f32,
    /// Grain cell size in output pixels.
    pub cell: f32,
    pub roughness: f32,
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Lattice value in [-1, 1] (integer hash; identical on the GPU).
#[inline]
pub fn lattice(ix: i32, iy: i32, octave: u32) -> f32 {
    let mut h = (ix as u32).wrapping_mul(0x8DA6_B343)
        ^ (iy as u32).wrapping_mul(0xD816_3841)
        ^ octave.wrapping_mul(0xCB1A_B31F);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    (h >> 8) as f32 / 8_388_607.5 - 1.0
}

/// Smoothly interpolated value noise at continuous lattice coordinates.
#[inline]
pub fn value_noise(x: f32, y: f32, octave: u32) -> f32 {
    let (fx, fy) = (x.floor(), y.floor());
    let (ix, iy) = (fx as i32, fy as i32);
    let (tx, ty) = (x - fx, y - fy);
    let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
    let a = lattice(ix, iy, octave);
    let b = lattice(ix + 1, iy, octave);
    let c = lattice(ix, iy + 1, octave);
    let d = lattice(ix + 1, iy + 1, octave);
    let top = a + (b - a) * sx;
    let bot = c + (d - c) * sx;
    top + (bot - top) * sy
}

impl EffectsParams {
    /// For an output image of `w` x `h` pixels.
    pub fn new(s: &Settings, w: usize, h: usize) -> Self {
        let v = &s.vignette;
        let r = v.roundness / 100.0;
        let aspect = w as f32 / h.max(1) as f32;
        // Towards a circle: stretch the long axis so equal distances are equal pixel distances.
        let (sx, sy) = if r > 0.0 {
            if aspect >= 1.0 {
                (1.0 + (aspect - 1.0) * r, 1.0)
            } else {
                (1.0, 1.0 + (1.0 / aspect - 1.0) * r)
            }
        } else {
            (1.0, 1.0)
        };
        let g = &s.grain;
        Self {
            vignette: v.amount != 0.0,
            amount: v.amount / 100.0,
            inner: 0.25 + 1.0 * v.midpoint / 100.0,
            width: 0.05 + 1.2 * v.feather / 100.0,
            highlights: v.highlights / 100.0,
            sx,
            sy,
            p: 2.0 + 6.0 * (-r).max(0.0),
            grain: g.amount > 0.0,
            grain_amount: g.amount / 100.0 * GRAIN_STRENGTH,
            cell: ((0.0004 + 0.0016 * g.size / 100.0) * w.min(h) as f32).max(0.5),
            roughness: g.roughness / 100.0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.vignette || self.grain
    }

    /// Apply to a display-linear pixel at output position (x, y) of a `w` x `h` image.
    #[inline]
    pub fn apply(
        &self,
        c: [f32; 3],
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        luma: [f32; 3],
    ) -> [f32; 3] {
        let mut c = c;
        if self.vignette {
            let u = ((x as f32 + 0.5) / w as f32 * 2.0 - 1.0) * self.sx;
            let v = ((y as f32 + 0.5) / h as f32 * 2.0 - 1.0) * self.sy;
            let d = (u.abs().powf(self.p) + v.abs().powf(self.p)).powf(1.0 / self.p);
            let t = smoothstep(self.inner, self.inner + self.width, d);
            if self.amount < 0.0 {
                let l = c[0] * luma[0] + c[1] * luma[1] + c[2] * luma[2];
                let keep = self.highlights * smoothstep(0.5, 1.0, l);
                let k = 1.0 + self.amount * t * (1.0 - keep);
                c = c.map(|v| v * k);
            } else {
                c = c.map(|v| v + (1.0 - v).max(0.0) * self.amount * t);
            }
        }
        if self.grain {
            let (gx, gy) = (x as f32 / self.cell, y as f32 / self.cell);
            let n1 = value_noise(gx, gy, 0);
            let n2 = value_noise(gx * GRAIN_OCTAVE, gy * GRAIN_OCTAVE, 1);
            let n = n1 + (n2 - n1) * self.roughness;
            let l = (c[0] * luma[0] + c[1] * luma[1] + c[2] * luma[2])
                .clamp(0.0, 1.0)
                .sqrt();
            let k = 1.0 + self.grain_amount * n * 4.0 * l * (1.0 - l);
            c = c.map(|v| v * k);
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Grain, Vignette};

    const LUMA: [f32; 3] = pe_colour::REC2020_LUMA;

    #[test]
    fn vignette_darkens_corners_not_centre() {
        let s = Settings {
            vignette: Vignette {
                amount: -80.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let (w, h) = (300, 200);
        let p = EffectsParams::new(&s, w, h);
        let grey = [0.3; 3];
        assert_eq!(p.apply(grey, 150, 100, w, h, LUMA), grey);
        assert!(p.apply(grey, 0, 0, w, h, LUMA)[0] < 0.15);
        let light = EffectsParams::new(
            &Settings {
                vignette: Vignette {
                    amount: 80.0,
                    ..Default::default()
                },
                ..Default::default()
            },
            w,
            h,
        );
        assert!(light.apply(grey, 0, 0, w, h, LUMA)[0] > 0.6);
    }

    #[test]
    fn grain_is_zero_mean_and_scale_free() {
        let s = Settings {
            grain: Grain {
                amount: 60.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mean_dev = |w: usize, h: usize| {
            let p = EffectsParams::new(&s, w, h);
            let mut sum = 0.0;
            let mut abs = 0.0;
            for y in 0..h {
                for x in 0..w {
                    let d = p.apply([0.2; 3], x, y, w, h, LUMA)[0] - 0.2;
                    sum += d;
                    abs += d.abs();
                }
            }
            (sum / (w * h) as f32, abs / (w * h) as f32)
        };
        let (m1, a1) = mean_dev(800, 600);
        let (_, a2) = mean_dev(1600, 1200);
        assert!(m1.abs() < a1 * 0.1, "roughly zero mean: {m1} vs {a1}");
        assert!(
            a1 > 0.002 && (a1 - a2).abs() < a1 * 0.2,
            "same strength at any size: {a1} {a2}"
        );
        for (ix, iy) in [(0, 0), (-5, 7), (1000, -3)] {
            let v = lattice(ix, iy, 0);
            assert!((-1.0..=1.0).contains(&v));
        }
    }
}
