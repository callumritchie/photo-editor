//! Rendering part of a picture (for 1:1 viewing) so that it matches the same part of a full render.
//!
//! Everything that depends on the whole picture is taken from the whole picture: filter radii and
//! the reduced grid of the guided filters (from the full size; region origins are aligned to the grid
//! and to the colour filter array), mask and lens positions (sensor coordinates plus the region's
//! origin), geometry, vignette and grain (full output coordinates), and the measurements in
//! [`Globals`] (auto white balance, dehaze airlight, noise level) when supplied. Close to the region's
//! edges, filters see less context than in a full render, so callers render a margin around what they
//! show ([`margin`]).

use pe_raw::{Layout, RawFrame};

use crate::Settings;

/// Where a sensor-space region sits in the whole frame, and which output pixels to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// Whole frame size (sensor orientation).
    pub full_w: usize,
    pub full_h: usize,
    /// Origin of the region's pixels in the whole frame.
    pub x0: usize,
    pub y0: usize,
    /// Output rectangle (x, y, width, height) in whole-picture output coordinates.
    pub out: [usize; 4],
}

/// Measurements of the whole picture. `None` means "measure on what is being rendered".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Globals {
    pub auto_wb: Option<[f32; 3]>,
    pub airlight: Option<[f32; 3]>,
    pub noise: Option<[f32; 3]>,
}

/// Sensor-space context of a render: the whole frame size and this frame's origin within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ctx {
    pub full_w: usize,
    pub full_h: usize,
    pub x0: usize,
    pub y0: usize,
}

impl Ctx {
    pub fn whole(w: usize, h: usize) -> Self {
        Self {
            full_w: w,
            full_h: h,
            x0: 0,
            y0: 0,
        }
    }
}

/// Context pixels needed around a region so its interior matches a full render, for these settings
/// on a frame of `full_w` x `full_h`.
pub fn margin(s: &Settings, full_w: usize, full_h: usize) -> usize {
    let g = crate::local::geometry(full_w, full_h);
    let f = g.factor;
    let mut m = 8; // demosaic (reflection, colour differences, median)
    let n = &s.noise_reduction;
    if n.luminance > 0.0 || n.color > 0.0 {
        m = m.max(2 * (1 + 2 + 4 + 8 + 16) + 4);
    }
    if s.sharpening.amount > 0.0 {
        m = m.max((3.0 * s.sharpening.radius).ceil() as usize * 2 + 4);
    }
    let local = s.shadows != 0.0 || s.highlights != 0.0 || s.clarity != 0.0 || !s.masks.is_empty();
    if local {
        // Two box blurs on the reduced grid, plus bilinear upsampling.
        m = m.max((2 * g.shadows_r.max(g.clarity_r) + 2) * f);
    }
    if s.texture != 0.0 || !s.masks.is_empty() {
        m = m.max(2 * g.texture_r + 2);
    }
    if s.dehaze != 0.0 {
        m = m.max((g.patch_r + 2 * g.guide_r + 2) * f);
    }
    m
}

/// Alignment for region origins: the reduced grid's factor and the colour filter array's period.
pub fn alignment(full_w: usize, full_h: usize, cfa_period: usize) -> usize {
    let f = crate::local::geometry(full_w, full_h).factor;
    let gcd = |mut a: usize, mut b: usize| {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    f / gcd(f, cfa_period.max(1)) * cfa_period.max(1)
}

/// The pixels of `frame` in the rectangle (x0, y0, w, h), as a frame of their own.
pub fn crop_frame(frame: &RawFrame, x0: usize, y0: usize, w: usize, h: usize) -> RawFrame {
    let ch = match frame.layout {
        Layout::Mosaic(_) => 1,
        Layout::LinearRgb => 3,
    };
    let mut data = Vec::with_capacity(w * h * ch);
    for y in y0..y0 + h {
        let s = (y * frame.width + x0) * ch;
        data.extend_from_slice(&frame.data[s..s + w * ch]);
    }
    RawFrame {
        width: w,
        height: h,
        data,
        ..frame.clone_header()
    }
}

/// Plan a region render of the output rectangle `out` (whole-picture output coordinates): the
/// aligned sensor rectangle to crop (x0, y0, w, h, including the margin) and the [`Region`].
pub fn plan_region(frame: &RawFrame, settings: &Settings, out: [usize; 4]) -> ([usize; 4], Region) {
    let (fw, fh) = (frame.width, frame.height);
    let s = settings.clone().clamped();
    let geo = crate::geometry::GeometryParams::for_frame(&s, frame, fw, fh);
    let mut q = geo;
    if !q.active {
        q.out_w = q.frame_w;
        q.out_h = q.frame_h;
        q.crop = [0.0, 0.0, 1.0, 1.0];
    }
    // Bounding box of the sensor positions the output rectangle samples (edges every 16 px).
    let [ox, oy, ow, oh] = out;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    let mut visit = |x: usize, y: usize| {
        for c in 0..3 {
            let (sx, sy) = q.source(x, y, c);
            x0 = x0.min(sx);
            y0 = y0.min(sy);
            x1 = x1.max(sx);
            y1 = y1.max(sy);
        }
    };
    let xs: Vec<usize> = (ox..ox + ow).step_by(16).chain([ox + ow - 1]).collect();
    let ys: Vec<usize> = (oy..oy + oh).step_by(16).chain([oy + oh - 1]).collect();
    for &x in &xs {
        visit(x, oy);
        visit(x, oy + oh - 1);
    }
    for &y in &ys {
        visit(ox, y);
        visit(ox + ow - 1, y);
    }
    let m = margin(&s, fw, fh) as f32 + 2.0;
    let period = match &frame.layout {
        Layout::Mosaic(cfa) => cfa.width.max(cfa.height),
        Layout::LinearRgb => 1,
    };
    let a = alignment(fw, fh, period);
    let sx0 = (((x0 - m).floor().max(0.0) as usize) / a) * a;
    let sy0 = (((y0 - m).floor().max(0.0) as usize) / a) * a;
    let sx1 = ((x1 + m).ceil().max(0.0) as usize).min(fw);
    let sy1 = ((y1 + m).ceil().max(0.0) as usize).min(fh);
    let (w, h) = (
        sx1.saturating_sub(sx0).max(1),
        sy1.saturating_sub(sy0).max(1),
    );
    (
        [sx0, sy0, w, h],
        Region {
            full_w: fw,
            full_h: fh,
            x0: sx0,
            y0: sy0,
            out,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_raw::Cfa;

    /// A synthetic Bayer mosaic with structure at several scales.
    fn mosaic(w: usize, h: usize, orientation: u8) -> RawFrame {
        let cfa = Cfa::bayer("RGGB").unwrap();
        let data = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                let c = cfa.color_at(i / w, i % w) as f32;
                let v = 0.25
                    + 0.15 * (x / 37.0).sin() * (y / 23.0).cos()
                    + 0.1 * ((x * 0.7 + y * 1.3).sin() > 0.6) as u8 as f32
                    + 0.05 * c
                    + 0.3 * (x > w as f32 * 0.6 && y < h as f32 * 0.3) as u8 as f32;
                v.min(1.0)
            })
            .collect();
        RawFrame {
            make: "Test".into(),
            model: "Synthetic".into(),
            width: w,
            height: h,
            layout: Layout::Mosaic(cfa),
            data,
            wb: [1.8, 1.0, 1.5],
            xyz_to_cam: [[0.9, -0.2, -0.05], [-0.3, 1.2, 0.1], [0.0, 0.1, 0.8]],
            orientation,
            scale: 1.0,
            lens: None,
            rendered: false,
            exposure_bias: 0.0,
            dng_warp: None,
            own_lens_corrections: false,
        }
    }

    fn check(settings: &Settings, orientation: u8, out_frac: [f32; 4]) {
        let (w, h) = (600, 420);
        let frame = mosaic(w, h, orientation);
        let full = crate::render_display(&frame, settings).unwrap();
        let globals = crate::measure_globals(&frame, settings).unwrap();
        let out = [
            (out_frac[0] * full.width as f32) as usize,
            (out_frac[1] * full.height as f32) as usize,
            (out_frac[2] * full.width as f32) as usize,
            (out_frac[3] * full.height as f32) as usize,
        ];
        let (rect, region) = plan_region(&frame, settings, out);
        let crop = crop_frame(&frame, rect[0], rect[1], rect[2], rect[3]);
        assert!(
            rect[2] < w || rect[3] < h,
            "the region is smaller than the frame: {rect:?}"
        );
        let part = crate::render_region_display(&crop, settings, &region, &globals).unwrap();
        assert_eq!((part.width, part.height), (out[2], out[3]));
        let mut worst = 0.0f32;
        for y in 0..out[3] {
            for x in 0..out[2] {
                for c in 0..3 {
                    let a = part.data[(y * out[2] + x) * 3 + c];
                    let b = full.data[((y + out[1]) * full.width + x + out[0]) * 3 + c];
                    worst = worst.max((a - b).abs());
                }
            }
        }
        assert!(
            worst < 2e-3,
            "{settings:?} orientation {orientation}: worst difference {worst}"
        );
    }

    #[test]
    fn region_matches_the_same_part_of_a_full_render() {
        let centre = [0.3, 0.3, 0.35, 0.4];
        check(&Settings::default(), 1, centre);
        check(&Settings::default(), 6, [0.05, 0.5, 0.4, 0.3]);
        let busy: Settings = serde_json::from_value(serde_json::json!({
            "exposure": 0.4, "shadows": 50, "highlights": -40, "clarity": 40, "texture": 30,
            "dehaze": 25, "whiteBalance": {"mode": "auto"},
            "noiseReduction": {"luminance": 40, "color": 50},
            "sharpening": {"amount": 60, "radius": 1.2},
            "vignette": {"amount": -40}, "grain": {"amount": 30},
            "lens": {"vignetting": 30},
            "masks": [{"id": "m", "components": [{"shape": {"type": "radial", "cx": 0.5, "cy": 0.4, "rx": 0.3, "ry": 0.3}}],
                       "adjust": {"exposure": 0.5, "clarity": 30, "noise": 60, "dehaze": 40, "sharpness": 50}}]
        }))
        .unwrap();
        check(&busy, 1, centre);
        check(&busy, 3, [0.5, 0.1, 0.3, 0.3]);
        let geometric: Settings = serde_json::from_value(serde_json::json!({
            "crop": {"left": 0.1, "top": 0.05, "right": 0.9, "bottom": 0.95, "angle": 4},
            "transform": {"vertical": 20},
            "lens": {"distortion": 30, "caRedCyan": 40},
            "shadows": 30
        }))
        .unwrap();
        check(&geometric, 1, [0.2, 0.25, 0.4, 0.35]);
    }
}
