//! Image-space export steps on display-linear RGB: resizing and output sharpening.

use image::{imageops::FilterType, ImageBuffer, Rgb};
use pe_pipeline::RgbImage;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// How to size the exported image. All modes keep the aspect ratio.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum Resize {
    #[default]
    Original,
    LongEdge {
        pixels: u32,
    },
    ShortEdge {
        pixels: u32,
    },
    Width {
        pixels: u32,
    },
    Height {
        pixels: u32,
    },
    /// Fit inside width x height.
    Fit {
        width: u32,
        height: u32,
    },
    Megapixels {
        megapixels: f32,
    },
    Percent {
        percent: f32,
    },
}

/// Target size for an image of `w` x `h`. With `dont_enlarge`, never larger than the source.
pub fn target_size(w: u32, h: u32, r: Resize, dont_enlarge: bool) -> (u32, u32) {
    let (wf, hf) = (w as f64, h as f64);
    let scale = match r {
        Resize::Original => 1.0,
        Resize::LongEdge { pixels } => pixels as f64 / wf.max(hf),
        Resize::ShortEdge { pixels } => pixels as f64 / wf.min(hf),
        Resize::Width { pixels } => pixels as f64 / wf,
        Resize::Height { pixels } => pixels as f64 / hf,
        Resize::Fit { width, height } => (width as f64 / wf).min(height as f64 / hf),
        Resize::Megapixels { megapixels } => (megapixels as f64 * 1e6 / (wf * hf)).sqrt(),
        Resize::Percent { percent } => percent as f64 / 100.0,
    };
    let scale = if dont_enlarge { scale.min(1.0) } else { scale };
    (
        ((wf * scale).round() as u32).max(1),
        ((hf * scale).round() as u32).max(1),
    )
}

/// Lanczos3 resize in linear light.
pub fn resize(img: &RgbImage, w: u32, h: u32) -> RgbImage {
    if (img.width as u32, img.height as u32) == (w, h) {
        return img.clone();
    }
    let buf: ImageBuffer<Rgb<f32>, Vec<f32>> =
        ImageBuffer::from_raw(img.width as u32, img.height as u32, img.data.clone())
            .expect("buffer size");
    let out = image::imageops::resize(&buf, w, h, FilterType::Lanczos3);
    RgbImage {
        width: w as usize,
        height: h as usize,
        data: out.into_raw(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SharpenAmount {
    Low,
    #[default]
    Standard,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Medium {
    #[default]
    Screen,
    Matte,
    Glossy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sharpen {
    pub amount: SharpenAmount,
    pub medium: Medium,
}

/// Unsharp-mask parameters (this project's own tuning): Gaussian sigma in output pixels, and strength.
/// Print sharpening scales its radius with output resolution so it acts at a constant size on paper;
/// matte paper diffuses ink more than glossy, so it gets more.
pub fn sharpen_params(s: Sharpen, ppi: u32) -> (f32, f32) {
    let strength = match s.amount {
        SharpenAmount::Low => 0.35,
        SharpenAmount::Standard => 0.6,
        SharpenAmount::High => 0.9,
    };
    match s.medium {
        Medium::Screen => (0.6, strength),
        Medium::Glossy => ((ppi as f32 / 300.0 * 0.8).max(0.5), strength * 1.1),
        Medium::Matte => ((ppi as f32 / 300.0 * 1.0).max(0.6), strength * 1.4),
    }
}

fn gaussian_kernel(sigma: f32) -> Vec<f32> {
    let r = (sigma * 3.0).ceil().max(1.0) as i32;
    let k: Vec<f32> = (-r..=r)
        .map(|x| (-(x * x) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let s: f32 = k.iter().sum();
    k.into_iter().map(|v| v / s).collect()
}

fn blur(src: &[f32], w: usize, h: usize, k: &[f32]) -> Vec<f32> {
    let r = (k.len() / 2) as isize;
    let mut tmp = vec![0.0; src.len()];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, v) in row.iter_mut().enumerate() {
            *v = k
                .iter()
                .enumerate()
                .map(|(i, kv)| {
                    kv * src
                        [y * w + (x as isize + i as isize - r).clamp(0, w as isize - 1) as usize]
                })
                .sum();
        }
    });
    let mut out = vec![0.0; src.len()];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, v) in row.iter_mut().enumerate() {
            *v = k
                .iter()
                .enumerate()
                .map(|(i, kv)| {
                    kv * tmp
                        [(y as isize + i as isize - r).clamp(0, h as isize - 1) as usize * w + x]
                })
                .sum();
        }
    });
    out
}

/// Output sharpening: unsharp mask on perceptual luminance (Y^(1/2.2)), applied to RGB as a gain so
/// colours are not shifted. Gains are limited to avoid halos turning into hard clipping.
pub fn sharpen(img: &mut RgbImage, s: Sharpen, ppi: u32, luma: [f32; 3]) {
    let (sigma, strength) = sharpen_params(s, ppi);
    let (w, h) = (img.width, img.height);
    let y: Vec<f32> = img
        .data
        .par_chunks(3)
        .map(|p| {
            (p[0] * luma[0] + p[1] * luma[1] + p[2] * luma[2])
                .max(0.0)
                .powf(1.0 / 2.2)
        })
        .collect();
    let b = blur(&y, w, h, &gaussian_kernel(sigma));
    img.data.par_chunks_mut(3).enumerate().for_each(|(i, p)| {
        if y[i] <= 1e-4 {
            return;
        }
        let sharpened = (y[i] + strength * (y[i] - b[i])).max(0.0);
        let gain = (sharpened / y[i]).powf(2.2).clamp(0.5, 2.0);
        for v in p.iter_mut() {
            *v *= gain;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(
            target_size(6000, 4000, Resize::LongEdge { pixels: 2048 }, true),
            (2048, 1365)
        );
        assert_eq!(
            target_size(4000, 6000, Resize::LongEdge { pixels: 2048 }, true),
            (1365, 2048)
        );
        assert_eq!(
            target_size(6000, 4000, Resize::ShortEdge { pixels: 1000 }, true),
            (1500, 1000)
        );
        assert_eq!(
            target_size(
                6000,
                4000,
                Resize::Fit {
                    width: 1000,
                    height: 1000
                },
                true
            ),
            (1000, 667)
        );
        assert_eq!(
            target_size(6000, 4000, Resize::Megapixels { megapixels: 6.0 }, true),
            (3000, 2000)
        );
        assert_eq!(
            target_size(600, 400, Resize::LongEdge { pixels: 2048 }, true),
            (600, 400)
        );
        assert_eq!(
            target_size(600, 400, Resize::LongEdge { pixels: 1200 }, false),
            (1200, 800)
        );
        assert_eq!(
            target_size(600, 400, Resize::Percent { percent: 50.0 }, true),
            (300, 200)
        );
    }

    #[test]
    fn resize_keeps_flat_colour() {
        let img = RgbImage {
            width: 40,
            height: 30,
            data: [0.2, 0.4, 0.6].repeat(1200),
        };
        let r = resize(&img, 13, 10);
        assert_eq!((r.width, r.height), (13, 10));
        for p in r.data.chunks(3) {
            assert!((p[0] - 0.2).abs() < 1e-4 && (p[2] - 0.6).abs() < 1e-4);
        }
    }

    #[test]
    fn sharpen_increases_edge_contrast_and_leaves_flat_areas() {
        let (w, h) = (32, 8);
        let data: Vec<f32> = (0..w * h)
            .flat_map(|i| if i % w < 16 { [0.1; 3] } else { [0.5; 3] })
            .collect();
        let mut img = RgbImage {
            width: w,
            height: h,
            data: data.clone(),
        };
        sharpen(
            &mut img,
            Sharpen {
                amount: SharpenAmount::High,
                medium: Medium::Screen,
            },
            240,
            [0.2, 0.7, 0.1],
        );
        let at = |d: &[f32], x: usize| d[(4 * w + x) * 3];
        assert!(
            at(&img.data, 15) < at(&data, 15),
            "dark side of edge darkened"
        );
        assert!(
            at(&img.data, 16) > at(&data, 16),
            "bright side of edge brightened"
        );
        assert!(
            (at(&img.data, 3) - 0.1).abs() < 1e-5 && (at(&img.data, 28) - 0.5).abs() < 1e-5,
            "flat areas unchanged"
        );
    }
}
