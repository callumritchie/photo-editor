//! Detail: noise reduction and capture sharpening, on scene-linear Rec.2020 right after white balance
//! and exposure.
//!
//! Design (this project's own, from the open literature):
//! - Work on variance-stabilised values (square root of each channel, which roughly evens out photon
//!   noise across brightness) in an opponent space: O1 = mean (luminance), O2 = (R - B) / 2,
//!   O3 = (2G - R - B) / 4 (colour).
//! - **Noise reduction**: undecimated ("à trous") wavelet transform with the B3-spline kernel
//!   [1 4 6 4 1] / 16 and five levels (Starck & Murtagh, "Astronomical Image and Data Analysis"),
//!   with soft-thresholding of each detail level. Thresholds are multiples of the noise level measured
//!   in this very frame (median absolute deviation of the finest level, on a 1-in-16 sample grid,
//!   measured before exposure and scaled by sqrt(exposure gain), which is exact in this domain),
//!   scaled by the known attenuation of white noise per level. So the sliders mean the same on a
//!   reduced preview (where averaging has already removed much noise) and a full-size export.
//!   Luminance keeps part of the removed detail per the Detail slider; colour does not.
//! - **Sharpening**: unsharp mask on O1 with a Gaussian of `radius` sensor pixels (divided by the
//!   frame's scale), the detail soft-limited to curb halos (Detail slider), and an optional edge mask
//!   from the blurred gradient (Masking slider). Only luminance changes.

use rayon::prelude::*;

use crate::Settings;

/// A plane of four channels (the GPU's layout), row-major.
pub type Plane4 = Vec<[f32; 4]>;

/// B3-spline taps.
pub const B3: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
/// Wavelet levels.
pub const LEVELS: usize = 5;
/// Standard deviation of each detail level for unit white noise (B3 à trous).
pub const NOISE_ATTENUATION: [f32; LEVELS] = [0.890, 0.201, 0.086, 0.041, 0.020];
/// Threshold (in noise standard deviations) at luminance / colour 100.
pub const LUMA_K_MAX: f32 = 3.0;
pub const COLOR_K_MAX: f32 = 4.0;
/// Noise sampling stride.
pub const NOISE_STRIDE: usize = 16;

#[inline]
pub fn to_opponent(c: [f32; 3]) -> [f32; 3] {
    let v = c.map(|x| x.max(0.0).sqrt());
    [
        (v[0] + v[1] + v[2]) / 3.0,
        (v[0] - v[2]) / 2.0,
        (2.0 * v[1] - v[0] - v[2]) / 4.0,
    ]
}

#[inline]
pub fn from_opponent(o: [f32; 3]) -> [f32; 3] {
    let v1 = o[0] + 4.0 / 3.0 * o[2];
    let v0 = o[0] - 2.0 / 3.0 * o[2] + o[1];
    let v2 = o[0] - 2.0 / 3.0 * o[2] - o[1];
    [v0, v1, v2].map(|v| {
        let v = v.max(0.0);
        v * v
    })
}

/// Noise standard deviation of each opponent channel, measured on the finest wavelet level at points
/// of a 1-in-[`NOISE_STRIDE`] grid. `px(x, y)` returns the scene-linear pixel.
pub fn noise_sigma(w: usize, h: usize, px: impl Fn(usize, usize) -> [f32; 3] + Sync) -> [f32; 3] {
    let clampi = |v: isize, n: usize| v.clamp(0, n as isize - 1) as usize;
    let points: Vec<(usize, usize)> = (0..h)
        .step_by(NOISE_STRIDE)
        .flat_map(|y| (0..w).step_by(NOISE_STRIDE).map(move |x| (x, y)))
        .collect();
    let details: Vec<[f32; 3]> = points
        .par_iter()
        .map(|&(x, y)| {
            let mut smooth = [0.0f32; 3];
            for (j, wy) in B3.iter().enumerate() {
                let yy = clampi(y as isize + j as isize - 2, h);
                let mut row = [0.0f32; 3];
                for (i, wx) in B3.iter().enumerate() {
                    let xx = clampi(x as isize + i as isize - 2, w);
                    let o = to_opponent(px(xx, yy));
                    for c in 0..3 {
                        row[c] += wx * o[c];
                    }
                }
                for c in 0..3 {
                    smooth[c] += wy * row[c];
                }
            }
            let o = to_opponent(px(x, y));
            [0, 1, 2].map(|c| (o[c] - smooth[c]).abs())
        })
        .collect();
    if details.is_empty() {
        return [0.0; 3];
    }
    [0, 1, 2].map(|c| {
        let mut v: Vec<f32> = details.iter().map(|d| d[c]).collect();
        let mid = v.len() / 2;
        let (_, m, _) = v.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        *m / 0.6745 / NOISE_ATTENUATION[0]
    })
}

/// One separable pass of the à trous smoothing (hole = 2^level). `vertical` picks the axis.
pub fn atrous_pass(src: &[[f32; 4]], w: usize, h: usize, hole: usize, vertical: bool) -> Plane4 {
    let mut out = vec![[0.0; 4]; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = [0.0f32; 4];
            for (i, k) in B3.iter().enumerate() {
                let d = (i as isize - 2) * hole as isize;
                let j = if vertical {
                    (y as isize + d).clamp(0, h as isize - 1) as usize * w + x
                } else {
                    y * w + (x as isize + d).clamp(0, w as isize - 1) as usize
                };
                for c in 0..4 {
                    acc[c] += k * src[j][c];
                }
            }
            *o = acc;
        }
    });
    out
}

/// Soft threshold keeping a fraction `keep` of what is removed.
#[inline]
pub fn shrink(d: f32, t: f32, keep: f32) -> f32 {
    d - (1.0 - keep) * d.clamp(-t, t)
}

/// Gaussian blur pass (sigma in frame pixels, radius ceil(3 sigma), at least 1), all four channels.
pub fn gauss_pass(src: &[[f32; 4]], w: usize, h: usize, sigma: f32, vertical: bool) -> Plane4 {
    let r = (3.0 * sigma).ceil().max(1.0) as isize;
    let mut out = vec![[0.0; 4]; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let (mut acc, mut sum) = ([0.0f32; 4], 0.0f32);
            for k in -r..=r {
                let wgt = (-((k * k) as f32) / (2.0 * sigma * sigma)).exp();
                let j = if vertical {
                    (y as isize + k).clamp(0, h as isize - 1) as usize * w + x
                } else {
                    y * w + (x as isize + k).clamp(0, w as isize - 1) as usize
                };
                for c in 0..4 {
                    acc[c] += wgt * src[j][c];
                }
                sum += wgt;
            }
            *o = acc.map(|v| v / sum);
        }
    });
    out
}

/// Derived constants for the detail stage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetailParams {
    pub nr: bool,
    /// Per level: thresholds for O1, O2, O3 and the luminance keep fraction.
    pub thresholds: [[f32; 4]; LEVELS],
    pub sharpen: bool,
    pub amount: f32,
    pub sigma: f32,
    pub knee: f32,
    /// Edge-mask threshold (0 = no mask).
    pub mask: f32,
}

impl DetailParams {
    /// `noise` from [`noise_sigma`] (only needed when noise reduction is on); `scale` is the frame's.
    pub fn new(s: &Settings, noise: [f32; 3], scale: f32) -> Self {
        let n = &s.noise_reduction;
        let (kl, kc) = (
            n.luminance / 100.0 * LUMA_K_MAX,
            n.color / 100.0 * COLOR_K_MAX,
        );
        let keep = n.detail / 100.0 * 0.5;
        let thresholds = NOISE_ATTENUATION.map(|a| {
            [
                kl * noise[0] * a,
                kc * noise[1] * a,
                kc * noise[2] * a,
                keep,
            ]
        });
        let sh = &s.sharpening;
        Self {
            nr: n.luminance > 0.0 || n.color > 0.0,
            thresholds,
            sharpen: sh.amount > 0.0,
            amount: sh.amount / 100.0,
            sigma: (sh.radius / scale.max(1e-3)).max(0.2),
            knee: 0.005 + 0.1 * (sh.detail / 100.0).powi(2),
            mask: (sh.masking / 100.0).powi(2) * 0.05,
        }
    }

    pub fn enabled(&self) -> bool {
        self.nr || self.sharpen
    }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Apply noise reduction and sharpening to interleaved scene RGB in place.
pub fn apply(rgb: &mut [f32], w: usize, h: usize, p: &DetailParams) {
    if !p.enabled() {
        return;
    }
    let mut o: Plane4 = rgb
        .par_chunks(3)
        .map(|c| {
            let v = to_opponent([c[0], c[1], c[2]]);
            [v[0], v[1], v[2], 0.0]
        })
        .collect();
    if p.nr {
        let mut acc = vec![[0.0f32; 4]; w * h];
        for (l, t) in p.thresholds.iter().enumerate() {
            let hole = 1 << l;
            let next = atrous_pass(&atrous_pass(&o, w, h, hole, false), w, h, hole, true);
            acc.par_iter_mut()
                .zip(o.par_iter().zip(&next))
                .for_each(|(a, (cur, nx))| {
                    for c in 0..3 {
                        let keep = if c == 0 { t[3] } else { 0.0 };
                        a[c] += shrink(cur[c] - nx[c], t[c], keep);
                    }
                });
            o = next;
        }
        o.par_iter_mut().zip(&acc).for_each(|(v, a)| {
            for c in 0..3 {
                v[c] += a[c];
            }
        });
    }
    if p.sharpen {
        let b = gauss_pass(&gauss_pass(&o, w, h, p.sigma, false), w, h, p.sigma, true);
        let at = |x: usize, y: usize| b[y.min(h - 1) * w + x.min(w - 1)][0];
        o.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
            for (x, v) in row.iter_mut().enumerate() {
                let bl = b[y * w + x][0];
                let d = v[0] - bl;
                let d = d / (1.0 + d.abs() / p.knee);
                let m = if p.mask > 0.0 {
                    let gx = at(x + 1, y) - at(x.saturating_sub(1), y);
                    let gy = at(x, y + 1) - at(x, y.saturating_sub(1));
                    smoothstep(p.mask, 2.0 * p.mask, gx.abs() + gy.abs())
                } else {
                    1.0
                };
                v[0] += p.amount * d * m;
            }
        });
    }
    rgb.par_chunks_mut(3).zip(&o).for_each(|(c, v)| {
        c.copy_from_slice(&from_opponent([v[0], v[1], v[2]]));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{NoiseReduction, Sharpening};

    /// Deterministic pseudo-random noise in [-1, 1].
    fn noise(i: usize) -> f32 {
        let mut x = (i as u32).wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
        x ^= x >> 13;
        x = x.wrapping_mul(0xC2B2_AE35);
        x ^= x >> 16;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    fn noisy_image(w: usize, h: usize, amp: f32) -> Vec<f32> {
        (0..w * h)
            .flat_map(|i| {
                let base = if (i % w) < w / 2 { 0.05 } else { 0.4 };
                [0, 1, 2].map(|c| (base * (1.0 + amp * noise(i * 3 + c))).max(0.0))
            })
            .collect()
    }

    fn mad_y(rgb: &[f32], w: usize) -> f32 {
        // Mean absolute horizontal difference of luminance within the flat right half.
        let y = |i: usize| rgb[i * 3] + rgb[i * 3 + 1] + rgb[i * 3 + 2];
        let mut s = 0.0;
        let mut n = 0;
        for i in 0..rgb.len() / 3 - 1 {
            if i % w > w / 2 + 2 && i % w < w - 2 {
                s += (y(i + 1) - y(i)).abs();
                n += 1;
            }
        }
        s / n as f32
    }

    #[test]
    fn opponent_round_trip() {
        for c in [[0.2, 0.4, 0.6], [1.5, 0.0, 0.3], [0.0, 0.0, 0.0]] {
            let r = from_opponent(to_opponent(c));
            assert!((0..3).all(|k| (r[k] - c[k]).abs() < 1e-5), "{c:?} -> {r:?}");
        }
    }

    #[test]
    fn noise_estimate_tracks_noise_level() {
        let (w, h) = (256, 128);
        let a = noisy_image(w, h, 0.05);
        let b = noisy_image(w, h, 0.2);
        let sigma = |img: &Vec<f32>| {
            noise_sigma(w, h, |x, y| {
                let i = (y * w + x) * 3;
                [img[i], img[i + 1], img[i + 2]]
            })[0]
        };
        let (sa, sb) = (sigma(&a), sigma(&b));
        assert!(sb > sa * 2.5 && sb < sa * 5.5, "{sa} {sb}");
    }

    #[test]
    fn noise_reduction_smooths_flat_areas_and_keeps_the_edge() {
        let (w, h) = (128, 64);
        let img = noisy_image(w, h, 0.15);
        let sig = noise_sigma(w, h, |x, y| {
            let i = (y * w + x) * 3;
            [img[i], img[i + 1], img[i + 2]]
        });
        let s = Settings {
            noise_reduction: NoiseReduction {
                luminance: 80.0,
                color: 80.0,
                detail: 0.0,
            },
            ..Default::default()
        };
        let mut out = img.clone();
        apply(&mut out, w, h, &DetailParams::new(&s, sig, 1.0));
        assert!(
            mad_y(&out, w) < mad_y(&img, w) * 0.5,
            "{} vs {}",
            mad_y(&out, w),
            mad_y(&img, w)
        );
        // The step between the halves survives.
        let row = h / 2 * w;
        let y = |v: &[f32], x: usize| v[(row + x) * 3 + 1];
        assert!(y(&out, w / 2 + 3) > 3.0 * y(&out, w / 2 - 4));
    }

    #[test]
    fn sharpening_raises_edge_contrast_and_scales_with_frame() {
        let (w, h) = (64, 16);
        let img: Vec<f32> = (0..w * h)
            .flat_map(|i| if i % w < w / 2 { [0.05; 3] } else { [0.4; 3] })
            .collect();
        let s = Settings {
            sharpening: Sharpening {
                amount: 100.0,
                radius: 1.0,
                detail: 50.0,
                masking: 0.0,
            },
            ..Default::default()
        };
        let mut out = img.clone();
        apply(&mut out, w, h, &DetailParams::new(&s, [0.0; 3], 1.0));
        let at = |v: &[f32], x: usize| v[(h / 2 * w + x) * 3];
        assert!(at(&out, w / 2 - 1) < at(&img, w / 2 - 1) && at(&out, w / 2) > at(&img, w / 2));
        assert!((at(&out, 5) - 0.05).abs() < 1e-4, "flat area untouched");
        assert_eq!(
            DetailParams::new(&s, [0.0; 3], 4.0).sigma,
            0.25,
            "radius in sensor pixels"
        );
    }
}
