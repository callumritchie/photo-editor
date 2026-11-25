//! Single-channel image filters: box blur, resampling, and the guided filter.
//!
//! Guided filter: He, Sun & Tang, "Guided Image Filtering" (ECCV 2010 / TPAMI 2013).
//! Fast variant (coefficients computed at reduced resolution, then upsampled): He & Sun,
//! "Fast Guided Filter" (arXiv:1505.00996, 2015).

use rayon::prelude::*;

/// A single-channel f32 image.
#[derive(Debug, Clone)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

impl Plane {
    pub fn new(width: usize, height: usize, data: Vec<f32>) -> Self {
        assert_eq!(data.len(), width * height);
        Self {
            width,
            height,
            data,
        }
    }

    fn transpose(&self) -> Plane {
        let (w, h) = (self.width, self.height);
        let mut out = vec![0.0; w * h];
        out.par_chunks_mut(h).enumerate().for_each(|(x, col)| {
            for (y, v) in col.iter_mut().enumerate() {
                *v = self.data[y * w + x];
            }
        });
        Plane::new(h, w, out)
    }

    fn blur_rows(&self, r: usize) -> Plane {
        let w = self.width;
        let mut out = vec![0.0; self.data.len()];
        out.par_chunks_mut(w)
            .zip(self.data.par_chunks(w))
            .for_each(|(dst, src)| {
                // Running sum with clamped window (normalised by the actual count).
                let mut prefix = vec![0.0f64; w + 1];
                for i in 0..w {
                    prefix[i + 1] = prefix[i] + src[i] as f64;
                }
                for (x, d) in dst.iter_mut().enumerate() {
                    let a = x.saturating_sub(r);
                    let b = (x + r + 1).min(w);
                    *d = ((prefix[b] - prefix[a]) / (b - a) as f64) as f32;
                }
            });
        Plane::new(w, self.height, out)
    }

    /// Mean over a (2r+1) x (2r+1) window, clamped at the borders.
    pub fn box_blur(&self, r: usize) -> Plane {
        self.blur_rows(r).transpose().blur_rows(r).transpose()
    }

    /// Area-average downscale by an integer factor (edge blocks may be partial).
    pub fn downscale(&self, factor: usize) -> Plane {
        let (w2, h2) = (self.width.div_ceil(factor), self.height.div_ceil(factor));
        let mut out = vec![0.0; w2 * h2];
        out.par_chunks_mut(w2).enumerate().for_each(|(y2, row)| {
            for (x2, v) in row.iter_mut().enumerate() {
                let (mut s, mut n) = (0.0f32, 0u32);
                for y in y2 * factor..((y2 + 1) * factor).min(self.height) {
                    for x in x2 * factor..((x2 + 1) * factor).min(self.width) {
                        s += self.data[y * self.width + x];
                        n += 1;
                    }
                }
                *v = s / n.max(1) as f32;
            }
        });
        Plane::new(w2, h2, out)
    }

    /// Bilinear sample at continuous coordinates (pixel centres at integer + 0.5).
    #[inline]
    pub fn sample(&self, fx: f32, fy: f32) -> f32 {
        let x = (fx - 0.5).clamp(0.0, (self.width - 1) as f32);
        let y = (fy - 0.5).clamp(0.0, (self.height - 1) as f32);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (tx, ty) = (x - x0 as f32, y - y0 as f32);
        let at = |xx: usize, yy: usize| self.data[yy * self.width + xx];
        let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
        let bot = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
        top * (1.0 - ty) + bot * ty
    }
}

/// Self-guided edge-preserving smoothing of `input`, computed at 1/`factor` resolution.
/// `radius` is in full-resolution pixels; `eps` is the regularisation (in input units squared):
/// structures with variance well above `eps` are preserved, finer variation is smoothed.
pub fn fast_guided_filter(input: &Plane, radius: usize, eps: f32, factor: usize) -> Plane {
    let factor = factor.max(1);
    let small = input.downscale(factor);
    let r = (radius / factor).max(1);
    let mean_i = small.box_blur(r);
    let sq = Plane::new(
        small.width,
        small.height,
        small.data.iter().map(|v| v * v).collect(),
    );
    let mean_ii = sq.box_blur(r);
    let (mut a, mut b) = (vec![0.0; small.data.len()], vec![0.0; small.data.len()]);
    for i in 0..small.data.len() {
        let var = (mean_ii.data[i] - mean_i.data[i] * mean_i.data[i]).max(0.0);
        a[i] = var / (var + eps);
        b[i] = mean_i.data[i] - a[i] * mean_i.data[i];
    }
    let mean_a = Plane::new(small.width, small.height, a).box_blur(r);
    let mean_b = Plane::new(small.width, small.height, b).box_blur(r);
    let (w, h) = (input.width, input.height);
    let inv = 1.0 / factor as f32;
    let mut out = vec![0.0; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let fy = (y as f32 + 0.5) * inv;
        for (x, v) in row.iter_mut().enumerate() {
            let fx = (x as f32 + 0.5) * inv;
            *v = mean_a.sample(fx, fy) * input.data[y * w + x] + mean_b.sample(fx, fy);
        }
    });
    Plane::new(w, h, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_blur_of_constant_is_constant() {
        let p = Plane::new(7, 5, vec![2.5; 35]);
        assert!(p.box_blur(2).data.iter().all(|v| (v - 2.5).abs() < 1e-6));
    }

    #[test]
    fn box_blur_averages_window() {
        let p = Plane::new(5, 1, vec![0.0, 0.0, 3.0, 0.0, 0.0]);
        let b = p.box_blur(1);
        assert!(
            (b.data[1] - 1.0).abs() < 1e-6 && (b.data[2] - 1.0).abs() < 1e-6 && b.data[0] == 0.0
        );
    }

    #[test]
    fn guided_filter_preserves_strong_edge_and_flattens_noise() {
        // Left half 0, right half 4 (a strong edge), with small alternating noise.
        let (w, h) = (64, 16);
        let data = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let base = if x < w / 2 { 0.0 } else { 4.0 };
                base + if (x + y) % 2 == 0 { 0.05 } else { -0.05 }
            })
            .collect();
        let out = fast_guided_filter(&Plane::new(w, h, data), 6, 0.1, 2);
        let at = |x: usize, y: usize| out.data[y * w + x];
        assert!(at(5, 8).abs() < 0.05, "left side stays ~0: {}", at(5, 8));
        assert!(
            (at(58, 8) - 4.0).abs() < 0.05,
            "right side stays ~4: {}",
            at(58, 8)
        );
        assert!((at(10, 8) - at(11, 8)).abs() < 0.04, "noise smoothed");
    }
}
