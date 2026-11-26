//! Local (spatial) adjustments on scene-linear Rec.2020: dehaze, then one luminance gain combining
//! shadows/highlights, clarity and texture.
//!
//! Design (this project's own). All radii are fractions of the image's short side, so a reduced preview
//! and a full-size export look alike. Edge-aware smoothing uses the fast guided filter computed on a
//! reduced grid (factor = short side / 512) and upsampled bilinearly.
//! - **Dehaze**: dark channel prior (He, Sun & Tang, "Single Image Haze Removal Using Dark Channel
//!   Prior", CVPR 2009). Airlight A: mean colour of the 0.1% of pixels (on a 1-in-4 grid) with the
//!   highest dark channel. Transmission t = 1 - w * (patch minimum of min_c I_c / A_c), refined with a
//!   guided filter whose guide is luminance relative to A; then J = (I - A) / max(t, 0.1) + A.
//!   Negative amounts mix towards A instead (adding haze).
//! - **Shadows / highlights**: as in `tone.rs` (guided-filter base of log2 luminance shifted by region).
//! - **Clarity**: detail = log2 Y minus a guided-filter base at ~0.8% radius, scaled by the amount and
//!   weighted to midtones (Gaussian in stops around mid-grey).
//! - **Texture**: detail = log2 Y minus a box blur at ~0.2% radius, soft-limited (d / (1 + |d|/0.35))
//!   so strong edges do not halo.

use rayon::prelude::*;

use crate::filter::Plane;
use crate::tone::{self, MID_GREY};
use crate::Settings;

/// Clarity's guided-filter regularisation (stops squared).
pub const CLARITY_EPS: f32 = 0.05;
/// Detail gain at clarity / texture +/-100.
pub const CLARITY_MAX: f32 = 0.6;
pub const TEXTURE_MAX: f32 = 0.8;
/// Texture detail soft limit (stops).
pub const TEXTURE_KNEE: f32 = 0.35;
/// Dehaze: transmission floor, guided-filter regularisation, haze mix at -100.
pub const DEHAZE_T_MIN: f32 = 0.1;
pub const DEHAZE_EPS: f32 = 1e-3;
pub const HAZE_MAX: f32 = 0.5;
/// Airlight sampling stride and fraction of samples used.
pub const AIRLIGHT_STRIDE: usize = 4;

/// Radii (in reduced-grid pixels unless noted) for an image of `w` x `h`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub factor: usize,
    pub small_w: usize,
    pub small_h: usize,
    pub shadows_r: usize,
    pub clarity_r: usize,
    /// Texture blur radius in full-resolution pixels.
    pub texture_r: usize,
    pub patch_r: usize,
    pub guide_r: usize,
}

pub fn geometry(w: usize, h: usize) -> Geometry {
    let short = w.min(h);
    let (radius, factor) = tone::local_tone_geometry(w, h);
    let frac = |f: f32, min: f32| (short as f32 * f).round().max(min) as usize;
    Geometry {
        factor,
        small_w: w.div_ceil(factor),
        small_h: h.div_ceil(factor),
        shadows_r: (radius / factor).max(1),
        clarity_r: (frac(0.008, 3.0) / factor).max(1),
        texture_r: frac(0.002, 1.0),
        patch_r: (frac(0.01, 3.0) / factor).max(1),
        guide_r: (frac(0.03, 6.0) / factor).max(2),
    }
}

#[inline]
fn luma(p: &[f32]) -> f32 {
    p[0] * pe_colour::REC2020_LUMA[0]
        + p[1] * pe_colour::REC2020_LUMA[1]
        + p[2] * pe_colour::REC2020_LUMA[2]
}

/// Airlight from scene pixels sampled on the 1-in-[`AIRLIGHT_STRIDE`] grid (given as RGB triples).
pub fn airlight(samples: &[[f32; 3]]) -> [f32; 3] {
    if samples.is_empty() {
        return [1.0; 3];
    }
    let mut idx: Vec<usize> = (0..samples.len()).collect();
    let dark = |i: usize| samples[i][0].min(samples[i][1]).min(samples[i][2]);
    let n = (samples.len() / 1000).max(16).min(samples.len());
    idx.select_nth_unstable_by(n - 1, |&a, &b| dark(b).total_cmp(&dark(a)).then(a.cmp(&b)));
    let mut a = [0.0f64; 3];
    for &i in &idx[..n] {
        for c in 0..3 {
            a[c] += samples[i][c] as f64;
        }
    }
    a.map(|v| ((v / n as f64) as f32).max(1e-3))
}

/// The grid samples of an interleaved RGB image, for [`airlight`].
pub fn airlight_samples(rgb: &[f32], w: usize, h: usize) -> Vec<[f32; 3]> {
    let mut out = Vec::with_capacity((w / AIRLIGHT_STRIDE + 1) * (h / AIRLIGHT_STRIDE + 1));
    for y in (0..h).step_by(AIRLIGHT_STRIDE) {
        for x in (0..w).step_by(AIRLIGHT_STRIDE) {
            let i = (y * w + x) * 3;
            out.push([rgb[i], rgb[i + 1], rgb[i + 2]]);
        }
    }
    out
}

/// Clamped-window minimum, separable.
pub fn min_filter(p: &Plane, r: usize) -> Plane {
    let (w, h) = (p.width, p.height);
    let mut tmp = vec![0.0; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, v) in row.iter_mut().enumerate() {
            let (a, b) = (x.saturating_sub(r), (x + r + 1).min(w));
            *v = p.data[y * w + a..y * w + b]
                .iter()
                .copied()
                .fold(f32::INFINITY, f32::min);
        }
    });
    let mut out = vec![0.0; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let (a, b) = (y.saturating_sub(r), (y + r + 1).min(h));
        for (x, v) in row.iter_mut().enumerate() {
            *v = (a..b)
                .map(|yy| tmp[yy * w + x])
                .fold(f32::INFINITY, f32::min);
        }
    });
    Plane::new(w, h, out)
}

/// Guided-filter coefficients (mean a, mean b) of `input` guided by `guide`, on the same grid.
pub fn guided_coeffs(guide: &Plane, input: &Plane, r: usize, eps: f32) -> (Plane, Plane) {
    let (w, h) = (guide.width, guide.height);
    let prod = |a: &Plane, b: &Plane| {
        Plane::new(
            w,
            h,
            a.data.iter().zip(&b.data).map(|(x, y)| x * y).collect(),
        )
    };
    let mean_i = guide.box_blur(r);
    let mean_p = input.box_blur(r);
    let mean_ip = prod(guide, input).box_blur(r);
    let mean_ii = prod(guide, guide).box_blur(r);
    let n = w * h;
    let (mut a, mut b) = (vec![0.0; n], vec![0.0; n]);
    for i in 0..n {
        let var = (mean_ii.data[i] - mean_i.data[i] * mean_i.data[i]).max(0.0);
        let cov = mean_ip.data[i] - mean_i.data[i] * mean_p.data[i];
        a[i] = cov / (var + eps);
        b[i] = mean_p.data[i] - a[i] * mean_i.data[i];
    }
    (
        Plane::new(w, h, a).box_blur(r),
        Plane::new(w, h, b).box_blur(r),
    )
}

/// Remove (amount > 0) or add (amount < 0) haze, in place. `a` is the airlight.
pub fn dehaze(rgb: &mut [f32], w: usize, h: usize, amount: f32, a: [f32; 3]) {
    if amount == 0.0 {
        return;
    }
    if amount < 0.0 {
        let beta = -amount / 100.0 * HAZE_MAX;
        rgb.par_chunks_mut(3).for_each(|p| {
            for c in 0..3 {
                p[c] = p[c] * (1.0 - beta) + a[c] * beta;
            }
        });
        return;
    }
    let g = geometry(w, h);
    let omega = amount / 100.0 * 0.95;
    let ya = luma(&a).max(1e-6);
    // Reduced-grid colour (block means), then dark channel and guide.
    let f = g.factor;
    let mut dark = vec![0.0; g.small_w * g.small_h];
    let mut guide = vec![0.0; g.small_w * g.small_h];
    dark.par_iter_mut()
        .zip(guide.par_iter_mut())
        .enumerate()
        .for_each(|(i, (d, gi))| {
            let (x2, y2) = (i % g.small_w, i / g.small_w);
            let mut s = [0.0f32; 3];
            let mut n = 0.0f32;
            for y in y2 * f..((y2 + 1) * f).min(h) {
                for x in x2 * f..((x2 + 1) * f).min(w) {
                    let j = (y * w + x) * 3;
                    s[0] += rgb[j];
                    s[1] += rgb[j + 1];
                    s[2] += rgb[j + 2];
                    n += 1.0;
                }
            }
            let m = [s[0] / n, s[1] / n, s[2] / n];
            *d = (m[0] / a[0]).min(m[1] / a[1]).min(m[2] / a[2]);
            *gi = (luma(&m) / ya).clamp(0.0, 2.0);
        });
    let dark = min_filter(&Plane::new(g.small_w, g.small_h, dark), g.patch_r);
    let t = Plane::new(
        g.small_w,
        g.small_h,
        dark.data.iter().map(|d| 1.0 - omega * d).collect(),
    );
    let guide = Plane::new(g.small_w, g.small_h, guide);
    let (ma, mb) = guided_coeffs(&guide, &t, g.guide_r, DEHAZE_EPS);
    let inv = 1.0 / f as f32;
    rgb.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        let fy = (y as f32 + 0.5) * inv;
        for (x, p) in row.chunks_mut(3).enumerate() {
            let fx = (x as f32 + 0.5) * inv;
            let gi = (luma(p) / ya).clamp(0.0, 2.0);
            let t = (ma.sample(fx, fy) * gi + mb.sample(fx, fy)).clamp(DEHAZE_T_MIN, 1.0);
            for c in 0..3 {
                p[c] = ((p[c] - a[c]) / t + a[c]).max(0.0);
            }
        }
    });
}

/// Self-guided coefficients on the reduced grid (as in the fast guided filter).
fn self_guided(small: &Plane, r: usize, eps: f32) -> (Plane, Plane) {
    guided_coeffs(small, small, r, eps)
}

/// Per-pixel luminance gain for shadows/highlights, clarity and texture. `None` when all are zero.
pub fn local_gains(rgb: &[f32], w: usize, h: usize, s: &Settings) -> Option<Vec<f32>> {
    let sh = s.shadows != 0.0 || s.highlights != 0.0;
    let (cl, tx) = (s.clarity != 0.0, s.texture != 0.0);
    if !(sh || cl || tx) {
        return None;
    }
    let g = geometry(w, h);
    let logy: Vec<f32> = rgb
        .par_chunks(3)
        .map(|p| luma(p).max(1e-6).log2())
        .collect();
    let full = Plane::new(w, h, logy);
    let small = full.downscale(g.factor);
    let sh_c = sh.then(|| self_guided(&small, g.shadows_r, tone::LOCAL_EPS));
    let cl_c = cl.then(|| self_guided(&small, g.clarity_r, CLARITY_EPS));
    let blurred = tx.then(|| full.box_blur(g.texture_r));
    let (sev, hev) = tone::local_tone_amounts(s.shadows, s.highlights);
    let (kc, kt) = (
        s.clarity / 100.0 * CLARITY_MAX,
        s.texture / 100.0 * TEXTURE_MAX,
    );
    let grey = MID_GREY.log2();
    let inv = 1.0 / g.factor as f32;
    let mut gains = vec![1.0; w * h];
    gains.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let fy = (y as f32 + 0.5) * inv;
        for (x, gain) in row.iter_mut().enumerate() {
            let fx = (x as f32 + 0.5) * inv;
            let i = y * w + x;
            let l = full.data[i];
            let mut d = 0.0;
            if let Some((ma, mb)) = &sh_c {
                let base = ma.sample(fx, fy) * l + mb.sample(fx, fy);
                d += tone::local_tone_shift(base - grey, sev, hev);
            }
            if let Some((ma, mb)) = &cl_c {
                let base = ma.sample(fx, fy) * l + mb.sample(fx, fy);
                let m = base - grey;
                d += kc * (l - base) * (-(m * m) / 8.0).exp();
            }
            if let Some(b) = &blurred {
                let dt = l - b.data[i];
                d += kt * dt / (1.0 + dt.abs() / TEXTURE_KNEE);
            }
            *gain = d.exp2();
        }
    });
    Some(gains)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn std_dev(v: &[f32]) -> f32 {
        let m = v.iter().sum::<f32>() / v.len() as f32;
        (v.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / v.len() as f32).sqrt()
    }

    /// Mid-grey image with fine checkerboard detail and a large-scale gradient.
    fn test_image(w: usize, h: usize) -> Vec<f32> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let v =
                    0.18 * (1.0 + 0.3 * ((x / 2 + y / 2) % 2) as f32) * (1.0 + x as f32 / w as f32);
                [v, v, v]
            })
            .collect()
    }

    #[test]
    fn texture_and_clarity_change_detail_by_sign() {
        let (w, h) = (256, 128);
        let img = test_image(w, h);
        let detail = |s: &Settings| {
            let g = local_gains(&img, w, h, s).unwrap();
            let out: Vec<f32> = img
                .chunks(3)
                .zip(&g)
                .map(|(p, g)| (p[0] * g).log2())
                .collect();
            std_dev(&out)
        };
        let base = std_dev(&img.chunks(3).map(|p| p[0].log2()).collect::<Vec<_>>());
        for (more, less) in [
            (
                Settings {
                    texture: 100.0,
                    ..Default::default()
                },
                Settings {
                    texture: -100.0,
                    ..Default::default()
                },
            ),
            (
                Settings {
                    clarity: 100.0,
                    ..Default::default()
                },
                Settings {
                    clarity: -100.0,
                    ..Default::default()
                },
            ),
        ] {
            assert!(
                detail(&more) > base * 1.05 && detail(&less) < base * 0.95,
                "{more:?}"
            );
        }
        assert!(local_gains(&img, w, h, &Settings::default()).is_none());
    }

    #[test]
    fn dehaze_restores_contrast_and_negative_adds_haze() {
        let (w, h) = (200, 120);
        // A contrasty scene seen through 50% haze of airlight 0.8.
        let clear: Vec<f32> = (0..w * h)
            .flat_map(|i| {
                if (i % w) / 20 % 2 == 0 {
                    [0.05, 0.1, 0.2]
                } else {
                    [0.4, 0.3, 0.1]
                }
            })
            .collect();
        let hazy: Vec<f32> = clear.iter().map(|v| v * 0.5 + 0.8 * 0.5).collect();
        let a = airlight(&airlight_samples(&hazy, w, h));
        let spread = |v: &[f32]| std_dev(&v.chunks(3).map(|p| p[1]).collect::<Vec<_>>());
        let mut d = hazy.clone();
        dehaze(&mut d, w, h, 100.0, a);
        assert!(
            spread(&d) > spread(&hazy) * 1.4,
            "{} vs {}",
            spread(&d),
            spread(&hazy)
        );
        let mut more = hazy.clone();
        dehaze(&mut more, w, h, -100.0, a);
        assert!(spread(&more) < spread(&hazy) * 0.7);
    }

    #[test]
    fn shadows_lift_only_where_dark() {
        let (w, h) = (64, 32);
        let rgb: Vec<f32> = (0..w * h)
            .flat_map(|i| {
                let v = if i % w < w / 2 { 0.01 } else { 0.5 };
                [v, v, v]
            })
            .collect();
        let s = Settings {
            shadows: 100.0,
            ..Default::default()
        };
        let g = local_gains(&rgb, w, h, &s).unwrap();
        assert!(
            g[h / 2 * w + 5] > 2.0,
            "dark side lifted: {}",
            g[h / 2 * w + 5]
        );
        assert!(
            g[h / 2 * w + 60] < 1.1,
            "bright side ~unchanged: {}",
            g[h / 2 * w + 60]
        );
    }

    #[test]
    fn min_filter_and_airlight() {
        let p = Plane::new(5, 1, vec![3.0, 1.0, 4.0, 1.0, 5.0]);
        assert_eq!(min_filter(&p, 1).data, vec![1.0, 1.0, 1.0, 1.0, 1.0]);
        let mut s = vec![[0.1f32, 0.1, 0.1]; 2000];
        for v in s.iter_mut().take(16) {
            *v = [0.9, 0.8, 0.7];
        }
        assert_eq!(airlight(&s), [0.9, 0.8, 0.7]);
    }
}
