//! Render pipeline (CPU reference implementation).
//!
//! Stage order:
//! 1. demosaic (camera-native, pre white balance)
//! 2. white balance + highlight reconstruction (camera space)
//! 3. camera -> linear Rec.2020 working space; exposure
//! 4. noise reduction, sharpening; dehaze; local tone (shadows / highlights, clarity, texture)
//! 5. contrast, filmic display transform (whites), black level; colour (mixer, vibrance/saturation,
//!    black and white, grading)
//! 6. Rec.2020 -> linear sRGB, gamut mapping, sRGB encoding
//! 7. point curve (encoded values); geometry (crop, straighten, perspective, lens; orientation);
//!    effects (vignette, grain)
//!
//! `pe-gpu` implements the same stages for linear-RGB frames and is tested against this path.

pub mod demosaic;
pub mod detail;
pub mod effects;
pub mod filter;
pub mod geometry;
pub mod local;
pub mod look;
pub mod orient;
pub mod settings;
pub mod tone;
pub mod wb;

pub use settings::{
    Crop, Demosaic, GradeWheel, Grading, Grain, Hsl, LensCorrection, NoiseReduction, Settings,
    Sharpening, Transform, Treatment, Vignette, WhiteBalance, HUE_BANDS, PROCESS_VERSION,
};

pub use pe_colour::ColorSpace;
use pe_colour::{apply, Mat3, REC2020_LUMA, REC2020_TO_XYZ_D65};
use pe_raw::{Layout, RawFrame};
use rayon::prelude::*;

/// Camera values at or above this fraction of saturation are treated as clipped.
pub const CLIP_THRESHOLD: f32 = 0.985;

/// Interleaved RGB image, f32 per channel.
#[derive(Debug, Clone)]
pub struct RgbImage {
    pub width: usize,
    pub height: usize,
    /// `width * height * 3` values.
    pub data: Vec<f32>,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("camera colour matrix is singular")]
    SingularMatrix,
}

/// Fast reduced-resolution render for previews and thumbnails.
///
/// Mosaic sensors are first reduced by averaging each CFA tile (2x2 for Bayer, 3x3 blocks otherwise)
/// into one RGB pixel ("superpixel"), which needs no demosaic and quarters (or ninths) the work. The
/// result then goes through the normal pipeline, so it matches a full render apart from resolution.
pub fn render_draft(frame: &RawFrame, settings: &Settings) -> Result<RgbImage, RenderError> {
    match &frame.layout {
        Layout::Mosaic(cfa) => render(&superpixel(frame, cfa), settings),
        Layout::LinearRgb => render(frame, settings),
    }
}

/// Reduce a frame once for interactive editing: superpixel (no demosaic), then an area-average
/// resample so the long edge is `long_edge` (never enlarged). The result is a small linear-RGB frame
/// that [`render`] processes quickly on every slider change.
pub fn prepare_preview(frame: &RawFrame, long_edge: usize) -> RawFrame {
    let base = match &frame.layout {
        Layout::Mosaic(cfa) => superpixel(frame, cfa),
        Layout::LinearRgb => frame.clone(),
    };
    let long = base.width.max(base.height);
    if long <= long_edge.max(1) {
        return base;
    }
    let scale = long_edge.max(1) as f64 / long as f64;
    let w = ((base.width as f64 * scale).round() as usize).max(1);
    let h = ((base.height as f64 * scale).round() as usize).max(1);
    let data = resample_area(&base.data, base.width, base.height, w, h);
    RawFrame {
        width: w,
        height: h,
        data,
        scale: base.scale * base.width as f32 / w as f32,
        ..base.clone_header()
    }
}

/// For each output index, the source indices and weights of an area-average (box) reduction from
/// `n_in` to `n_out` samples.
fn area_weights(n_in: usize, n_out: usize) -> Vec<Vec<(usize, f32)>> {
    let s = n_in as f64 / n_out as f64;
    (0..n_out)
        .map(|o| {
            let (a, b) = (o as f64 * s, ((o + 1) as f64 * s).min(n_in as f64));
            let mut taps = Vec::new();
            let mut i = a.floor() as usize;
            while (i as f64) < b && i < n_in {
                let cover = (b.min(i as f64 + 1.0) - a.max(i as f64)).max(0.0);
                if cover > 0.0 {
                    taps.push((i, (cover / (b - a)) as f32));
                }
                i += 1;
            }
            taps
        })
        .collect()
}

/// Area-average resample of interleaved RGB (for reductions; each output pixel is the mean of the
/// input area it covers).
pub fn resample_area(data: &[f32], w: usize, h: usize, nw: usize, nh: usize) -> Vec<f32> {
    let wx = area_weights(w, nw);
    let wy = area_weights(h, nh);
    // Rows first (w -> nw), then columns (h -> nh).
    let mut rows = vec![0.0f32; nw * h * 3];
    rows.par_chunks_mut(nw * 3)
        .enumerate()
        .for_each(|(y, out)| {
            let src = &data[y * w * 3..(y + 1) * w * 3];
            for (x, taps) in wx.iter().enumerate() {
                let mut acc = [0.0f32; 3];
                for &(i, k) in taps {
                    for c in 0..3 {
                        acc[c] += src[i * 3 + c] * k;
                    }
                }
                out[x * 3..x * 3 + 3].copy_from_slice(&acc);
            }
        });
    let mut out = vec![0.0f32; nw * nh * 3];
    out.par_chunks_mut(nw * 3).enumerate().for_each(|(y, dst)| {
        for &(j, k) in &wy[y] {
            let src = &rows[j * nw * 3..(j + 1) * nw * 3];
            for (d, s) in dst.iter_mut().zip(src) {
                *d += s * k;
            }
        }
    });
    out
}

fn superpixel(frame: &RawFrame, cfa: &pe_raw::Cfa) -> RawFrame {
    let block = if cfa.width == 2 && cfa.height == 2 {
        2
    } else {
        3
    };
    let (w, h) = (frame.width / block, frame.height / block);
    let mut data = vec![0.0f32; w * h * 3];
    data.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let (mut sum, mut n) = ([0.0f32; 3], [0u32; 3]);
            for dy in 0..block {
                for dx in 0..block {
                    let (yy, xx) = (y * block + dy, x * block + dx);
                    let c = cfa.color_at(yy, xx) as usize;
                    sum[c] += frame.data[yy * frame.width + xx];
                    n[c] += 1;
                }
            }
            for c in 0..3 {
                row[x * 3 + c] = if n[c] > 0 { sum[c] / n[c] as f32 } else { 0.0 };
            }
        }
    });
    RawFrame {
        width: w,
        height: h,
        layout: Layout::LinearRgb,
        data,
        scale: frame.scale * block as f32,
        ..frame.clone_header()
    }
}

/// Render to display-referred, sRGB-encoded values in [0, 1].
pub fn render(frame: &RawFrame, settings: &Settings) -> Result<RgbImage, RenderError> {
    Ok(encode_output(
        &render_display(frame, settings)?,
        ColorSpace::Srgb,
    ))
}

/// Stages 1-5 and orientation: display-referred *linear* Rec.2020 (values may lie outside [0, 1] gamut
/// in other spaces), with the point curve applied. Resizing and sharpening for export happen on this.
pub fn render_display(frame: &RawFrame, settings: &Settings) -> Result<RgbImage, RenderError> {
    let settings = settings.clone().clamped();
    let scene = render_scene(frame, &settings)?;
    let (w, h) = (scene.width, scene.height);
    let mut data = scene.data;

    // Dehaze's airlight is estimated on the scene before noise reduction and sharpening (the GPU host
    // computes it from the same scene pixels).
    let airlight =
        (settings.dehaze != 0.0).then(|| local::airlight(&local::airlight_samples(&data, w, h)));
    let dp = detail::DetailParams::new(&settings, [0.0; 3], frame.scale);
    if dp.enabled() {
        let noise = if dp.nr {
            // Measured without exposure (so exposure changes need no new measurement) and scaled:
            // noise of sqrt(g x) is sqrt(g) times that of sqrt(x).
            let g = settings.exposure.exp2();
            let n = detail::noise_sigma(w, h, |x, y| {
                let i = (y * w + x) * 3;
                [data[i] / g, data[i + 1] / g, data[i + 2] / g]
            });
            n.map(|v| v * g.sqrt())
        } else {
            [0.0; 3]
        };
        detail::apply(
            &mut data,
            w,
            h,
            &detail::DetailParams::new(&settings, noise, frame.scale),
        );
    }
    if let Some(a) = airlight {
        local::dehaze(&mut data, w, h, settings.dehaze, a);
    }
    if let Some(gains) = local::local_gains(&data, w, h, &settings) {
        data.par_chunks_mut(3)
            .zip(gains.par_iter())
            .for_each(|(px, g)| {
                px[0] *= g;
                px[1] *= g;
                px[2] *= g;
            });
    }

    let params = tone::ToneParams::new(settings.contrast, settings.whites, settings.blacks);
    let look_params = look::LookParams::new(&settings);
    // The point curve is defined on sRGB-encoded display values, independent of the output space.
    let curve = tone::CurveLut::new(&settings.tone_curve);
    data.par_chunks_mut(3).for_each(|px| {
        let mut d = look_params.apply(params.apply([px[0], px[1], px[2]], REC2020_LUMA));
        if let Some(c) = &curve {
            for v in &mut d {
                *v = pe_colour::srgb_decode(c.apply(pe_colour::srgb_encode(v.clamp(0.0, 1.0))));
            }
        }
        px.copy_from_slice(&d);
    });

    let geo = geometry::GeometryParams::new(&settings, w, h, frame.orientation);
    let (w, h, mut data) = if geo.active {
        geometry::resample(&data, &geo)
    } else if geo.orientation != 1 {
        orient::apply(geo.orientation, w, h, &data)
    } else {
        (w, h, data)
    };
    // Effects work in output coordinates.
    let fx = effects::EffectsParams::new(&settings, w, h);
    if fx.enabled() {
        data.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
            for (x, px) in row.chunks_mut(3).enumerate() {
                px.copy_from_slice(&fx.apply([px[0], px[1], px[2]], x, y, w, h, REC2020_LUMA));
            }
        });
    }
    Ok(RgbImage {
        width: w,
        height: h,
        data,
    })
}

/// Convert display-linear Rec.2020 to encoded values in [0, 1] for `space`, bringing out-of-gamut colours
/// in by desaturating towards luminance.
pub fn encode_output(img: &RgbImage, space: ColorSpace) -> RgbImage {
    let m = space.from_space(&REC2020_TO_XYZ_D65);
    let luma = space.luma();
    let mut data = img.data.clone();
    data.par_chunks_mut(3).for_each(|px| {
        let lin = tone::gamut_map(apply(&m, [px[0], px[1], px[2]]), luma);
        for (o, v) in px.iter_mut().zip(lin) {
            *o = space.encode(v);
        }
    });
    RgbImage {
        width: img.width,
        height: img.height,
        data,
    }
}

/// Stages 1-3: scene-linear Rec.2020 after white balance, highlight reconstruction and exposure.
pub fn render_scene(frame: &RawFrame, settings: &Settings) -> Result<RgbImage, RenderError> {
    let cam = match (&frame.layout, settings.demosaic) {
        (Layout::Mosaic(cfa), Demosaic::Auto) => {
            demosaic::high_quality(&frame.data, frame.width, frame.height, cfa)
        }
        (Layout::Mosaic(cfa), Demosaic::Fast) => {
            demosaic::neighbourhood_average(&frame.data, frame.width, frame.height, cfa)
        }
        (Layout::LinearRgb, _) => frame.data.clone(),
    };
    let wb = wb::multipliers(frame, &settings.white_balance, &cam);
    let to_work: Mat3 = pe_colour::camera_to_space(&frame.xyz_to_cam, &REC2020_TO_XYZ_D65)
        .ok_or(RenderError::SingularMatrix)?;
    let gain = settings.exposure.exp2();

    let mut data = cam;
    let geo = geometry::GeometryParams::new(settings, frame.width, frame.height, frame.orientation);
    let lens = geo.lens_vignette != 0.0;
    data.par_chunks_mut(frame.width * 3)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, px) in row.chunks_mut(3).enumerate() {
                let mut c = scene_pixel([px[0], px[1], px[2]], wb, &to_work, gain);
                if lens {
                    let g = geo.lens_gain(x, y);
                    c = c.map(|v| v * g);
                }
                px.copy_from_slice(&c);
            }
        });
    Ok(RgbImage {
        width: frame.width,
        height: frame.height,
        data,
    })
}

/// Camera RGB -> scene-linear Rec.2020 for one pixel: white balance with highlight reconstruction,
/// colour matrix, exposure gain.
#[inline]
pub fn scene_pixel(cam: [f32; 3], wb: [f32; 3], to_work: &Mat3, gain: f32) -> [f32; 3] {
    apply(to_work, reconstruct_highlights(cam, wb)).map(|v| v * gain)
}

/// White-balance one camera pixel and repair clipped channels.
///
/// A clipped channel only tells us the true value is *at least* the clip level. After white balance,
/// unclipped channels can exceed it, which tints blown highlights (classically magenta). We raise each
/// clipped channel to the brightest unclipped one (or, if all are clipped, all to the brightest), so
/// highlights trend to neutral and keep their brightness above 1.0 for the tone stage to roll off.
#[inline]
pub fn reconstruct_highlights(cam: [f32; 3], wb: [f32; 3]) -> [f32; 3] {
    let clipped = [
        cam[0] >= CLIP_THRESHOLD,
        cam[1] >= CLIP_THRESHOLD,
        cam[2] >= CLIP_THRESHOLD,
    ];
    let mut c = [
        cam[0].max(0.0) * wb[0],
        cam[1].max(0.0) * wb[1],
        cam[2].max(0.0) * wb[2],
    ];
    if clipped.iter().any(|&k| k) {
        let reference = (0..3)
            .filter(|&i| !clipped[i])
            .map(|i| c[i])
            .fold(f32::NEG_INFINITY, f32::max);
        let reference = if reference.is_finite() {
            reference
        } else {
            c[0].max(c[1]).max(c[2])
        };
        for i in 0..3 {
            if clipped[i] {
                c[i] = c[i].max(reference);
            }
        }
    }
    c
}

/// Quantise encoded [0, 1] values to 16-bit.
pub fn to_u16(img: &RgbImage) -> Vec<u16> {
    img.data
        .par_iter()
        .map(|&v| (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blown_green_does_not_turn_magenta() {
        // Green clipped while red/blue are not: after WB, red/blue exceed green -> magenta without repair.
        let wb = [2.0, 1.0, 1.6];
        let c = reconstruct_highlights([0.8, 1.0, 0.85], wb);
        assert!(c[1] >= c[0] && c[1] >= c[2], "{c:?}");
    }

    #[test]
    fn fully_clipped_pixel_is_neutral() {
        let c = reconstruct_highlights([1.0, 1.0, 1.0], [2.0, 1.0, 1.6]);
        assert_eq!(c, [2.0, 2.0, 2.0]);
    }

    #[test]
    fn unclipped_pixel_is_just_white_balanced() {
        assert_eq!(
            reconstruct_highlights([0.25, 0.5, 0.5], [2.0, 1.0, 1.5]),
            [0.5, 0.5, 0.75]
        );
    }

    #[test]
    fn draft_render_halves_bayer_and_matches_full_render_on_flat_field() {
        let cfa = pe_raw::Cfa::bayer("RGGB").unwrap();
        let (w, h) = (64, 48);
        let rgb = [0.2f32, 0.4, 0.3];
        let data = (0..w * h)
            .map(|i| rgb[cfa.color_at(i / w, i % w) as usize])
            .collect();
        let frame = RawFrame {
            make: "Test".into(),
            model: "Flat".into(),
            width: w,
            height: h,
            layout: Layout::Mosaic(cfa),
            data,
            wb: [1.5, 1.0, 1.8],
            xyz_to_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            orientation: 1,
            scale: 1.0,
        };
        let s = Settings::default();
        let draft = render_draft(&frame, &s).unwrap();
        let full = render(&frame, &s).unwrap();
        assert_eq!((draft.width, draft.height), (32, 24));
        for c in 0..3 {
            assert!((draft.data[c] - full.data[c]).abs() < 1e-4, "channel {c}");
        }
    }

    #[test]
    fn area_resample_preserves_mean_and_flat_colour() {
        let (w, h) = (7, 5);
        let data: Vec<f32> = (0..w * h).flat_map(|i| [i as f32, 1.0, 2.0]).collect();
        let out = super::resample_area(&data, w, h, 3, 2);
        let mean = |d: &[f32]| d.chunks(3).map(|p| p[0]).sum::<f32>() / (d.len() / 3) as f32;
        assert!((mean(&out) - mean(&data)).abs() < 1e-3);
        assert!(out
            .chunks(3)
            .all(|p| (p[1] - 1.0).abs() < 1e-5 && (p[2] - 2.0).abs() < 1e-5));
    }
}
