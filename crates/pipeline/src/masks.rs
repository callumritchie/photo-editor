//! Masks and local adjustments.
//!
//! A mask is a list of components combined in order (add = union, subtract, intersect; each may be
//! inverted), scaled by an amount, with its own adjustments. Shapes:
//! - **Linear gradient**: full effect at the start point, none at the end, smooth in between.
//! - **Radial gradient**: an ellipse (centre, radii, angle) with a feathered edge; full effect inside.
//! - **Brush**: strokes (points, size, feather, flow; painting or erasing), rasterised at
//!   [`BRUSH_LONG_EDGE`] px by [`rasterise_brush`], which the CPU and GPU paths both use.
//! - **Luminance range** / **colour range**: selected from the photo's own tones (lightness after the
//!   global tone stage) or colours (OKLab distance to picked colours).
//! - **Raster**: a stored mask image (subject or sky selection), referenced by id.
//!
//! Positions are fractions (0..1) of the oriented, uncropped frame, so crops and straightening do not
//! move masks relative to the picture.
//!
//! Local adjustments (this project's own formulation) act on scene-linear values after the global
//! local-tone stage: white balance as a chromatic adaptation (Bradford) of the working space,
//! exposure as a gain, contrast / highlights / shadows / whites / blacks as luminance-dependent gains
//! in log2 stops, clarity and texture by adding the same detail terms as the global sliders, and
//! saturation as an OKLab chroma gain after the colour stage.

use std::collections::HashMap;
use std::sync::Arc;

use pe_colour::{invert, mul, ColorSpace, Mat3, REC2020_TO_XYZ_D65};
use serde::{Deserialize, Serialize};

use crate::look::{LAB_TO_LMS, LMS_TO_LAB};
use crate::tone::MID_GREY;

/// Long edge of brush rasters, in pixels.
pub const BRUSH_LONG_EDGE: usize = 1024;
/// Most masks, and components per mask, the renderers handle.
pub const MAX_MASKS: usize = 16;
pub const MAX_COMPONENTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Combine {
    #[default]
    Add,
    Subtract,
    Intersect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stroke {
    /// Points in frame fractions.
    pub points: Vec<[f32; 2]>,
    /// Brush radius as a fraction of the frame's short side.
    pub size: f32,
    /// 0..1: softness of the edge.
    #[serde(default = "half")]
    pub feather: f32,
    /// 0..1: strength.
    #[serde(default = "one")]
    pub flow: f32,
    #[serde(default)]
    pub erase: bool,
}

fn half() -> f32 {
    0.5
}
fn one() -> f32 {
    1.0
}
fn hundred() -> f32 {
    100.0
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Shape {
    Linear {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
    },
    Radial {
        cx: f32,
        cy: f32,
        /// Radii as fractions of the frame width and height.
        rx: f32,
        ry: f32,
        #[serde(default)]
        angle: f32,
        #[serde(default = "half")]
        feather: f32,
    },
    Brush {
        strokes: Vec<Stroke>,
    },
    /// Lightness range 0..1 with a soft edge of `smoothness` (0..1).
    Luminance {
        lo: f32,
        hi: f32,
        #[serde(default = "half")]
        smoothness: f32,
    },
    /// Picked colours as OKLab (L, a, b); tolerance 0..100.
    Color {
        colors: Vec<[f32; 3]>,
        #[serde(default = "fifty")]
        tolerance: f32,
    },
    Raster {
        raster_id: String,
        /// What it selects ("Subject", "Sky"), for display.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        label: String,
    },
}

fn fifty() -> f32 {
    50.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaskComponent {
    #[serde(default)]
    pub mode: Combine,
    #[serde(default)]
    pub invert: bool,
    pub shape: Shape,
}

/// Local adjustments: -100..100 each, except exposure in stops (-4..4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct LocalAdjust {
    pub temperature: f32,
    pub tint: f32,
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub texture: f32,
    pub clarity: f32,
    /// + removes haze, - adds it.
    pub dehaze: f32,
    pub saturation: f32,
    /// + sharpens, - softens.
    pub sharpness: f32,
    /// 0..100: local noise reduction.
    pub noise: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mask {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 0..100.
    #[serde(default = "hundred")]
    pub amount: f32,
    pub components: Vec<MaskComponent>,
    #[serde(default)]
    pub adjust: LocalAdjust,
}

/// A stored mask image (fractions of the oriented frame), values 0..1.
#[derive(Debug, Clone, PartialEq)]
pub struct Raster {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

impl Raster {
    /// Bilinear sample at frame fractions (fx, fy).
    #[inline]
    pub fn sample(&self, fx: f32, fy: f32) -> f32 {
        let (w, h) = (self.width, self.height);
        let x = (fx * w as f32 - 0.5).clamp(0.0, (w - 1) as f32);
        let y = (fy * h as f32 - 0.5).clamp(0.0, (h - 1) as f32);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let (tx, ty) = (x - x0 as f32, y - y0 as f32);
        let at = |xx: usize, yy: usize| self.data[yy * w + xx];
        let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
        let bot = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
        top * (1.0 - ty) + bot * ty
    }
}

/// Rasters available to masks, by id (subject / sky selections). Not part of the settings JSON.
pub type Rasters = Arc<HashMap<String, Raster>>;

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Rasterise brush strokes for a frame of aspect `aspect` (width / height).
pub fn rasterise_brush(strokes: &[Stroke], aspect: f32) -> Raster {
    let (w, h) = if aspect >= 1.0 {
        (
            BRUSH_LONG_EDGE,
            ((BRUSH_LONG_EDGE as f32 / aspect).round() as usize).max(1),
        )
    } else {
        (
            ((BRUSH_LONG_EDGE as f32 * aspect).round() as usize).max(1),
            BRUSH_LONG_EDGE,
        )
    };
    let mut data = vec![0.0f32; w * h];
    let short = w.min(h) as f32;
    for s in strokes {
        let r = (s.size * short).max(0.5);
        let inner = r * (1.0 - s.feather.clamp(0.0, 1.0));
        let pts: Vec<(f32, f32)> = s
            .points
            .iter()
            .map(|p| (p[0] * w as f32, p[1] * h as f32))
            .collect();
        if pts.is_empty() {
            continue;
        }
        let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in &pts {
            minx = minx.min(x);
            miny = miny.min(y);
            maxx = maxx.max(x);
            maxy = maxy.max(y);
        }
        let x0 = ((minx - r).floor().max(0.0)) as usize;
        let y0 = ((miny - r).floor().max(0.0)) as usize;
        let x1 = ((maxx + r).ceil() as usize).min(w);
        let y1 = ((maxy + r).ceil() as usize).min(h);
        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let mut d = f32::MAX;
                if pts.len() == 1 {
                    d = ((px - pts[0].0).powi(2) + (py - pts[0].1).powi(2)).sqrt();
                }
                for seg in pts.windows(2) {
                    let ((ax, ay), (bx, by)) = (seg[0], seg[1]);
                    let (dx, dy) = (bx - ax, by - ay);
                    let len2 = dx * dx + dy * dy;
                    let t = if len2 > 0.0 {
                        (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let (qx, qy) = (ax + t * dx, ay + t * dy);
                    d = d.min(((px - qx).powi(2) + (py - qy).powi(2)).sqrt());
                }
                if d >= r {
                    continue;
                }
                let v = s.flow.clamp(0.0, 1.0) * (1.0 - smoothstep(inner, r, d));
                let m = &mut data[y * w + x];
                *m = if s.erase { *m * (1.0 - v) } else { m.max(v) };
            }
        }
    }
    Raster {
        width: w,
        height: h,
        data,
    }
}

/// [`rasterise_brush`] with a small process-wide cache (keyed by the strokes' JSON and the aspect),
/// so re-rendering while other sliders move does not rasterise again.
pub fn brush_raster_cached(strokes: &[Stroke], aspect: f32) -> Arc<Raster> {
    use std::sync::Mutex;
    static CACHE: Mutex<Vec<(String, Arc<Raster>)>> = Mutex::new(Vec::new());
    let key = format!(
        "{aspect:.6}:{}",
        serde_json::to_string(strokes).unwrap_or_default()
    );
    if let Some((_, r)) = CACHE.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return r.clone();
    }
    let r = Arc::new(rasterise_brush(strokes, aspect));
    let mut c = CACHE.lock().unwrap();
    c.push((key, r.clone()));
    if c.len() > 8 {
        c.remove(0);
    }
    r
}

/// A component prepared for evaluation (positions in oriented-frame pixels).
#[derive(Debug, Clone)]
pub enum PreparedShape {
    Linear {
        p0: [f32; 2],
        d: [f32; 2],
        inv_len2: f32,
    },
    Radial {
        c: [f32; 2],
        cos: f32,
        sin: f32,
        inv_rx: f32,
        inv_ry: f32,
        inner: f32,
    },
    Raster(Arc<Raster>),
    Luminance {
        lo: f32,
        hi: f32,
        soft: f32,
    },
    Color {
        colors: Vec<[f32; 3]>,
        tol: f32,
    },
    /// Missing raster: matches nothing.
    Nothing,
}

#[derive(Debug, Clone)]
pub struct PreparedComponent {
    pub mode: Combine,
    pub invert: bool,
    pub shape: PreparedShape,
}

/// Derived coefficients of one mask's adjustments.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdjustParams {
    pub wb: Option<Mat3>,
    pub exposure: f32,
    pub contrast: f32,
    pub shadows: f32,
    pub highlights: f32,
    pub whites: f32,
    pub blacks: f32,
    pub clarity: f32,
    pub texture: f32,
    pub saturation: f32,
    /// -1..1, -1..1, 0..1.
    pub dehaze: f32,
    pub sharpness: f32,
    pub noise: f32,
}

/// Local noise reduction blends towards the picture denoised with these strengths; local dehaze
/// towards the picture dehazed at full strength (or, when negative, towards the airlight by up to
/// this fraction).
pub const LOCAL_NR_STRENGTH: f32 = 70.0;
pub const LOCAL_HAZE_MAX: f32 = 0.5;

impl AdjustParams {
    pub fn new(a: &LocalAdjust) -> Self {
        let wb = (a.temperature != 0.0 || a.tint != 0.0).then(|| {
            // Neutral under a light of this colour becomes neutral under D65: positive temperature
            // adapts from a bluer light, which warms the picture.
            let t = 6504.0 * (a.temperature / 100.0 * 0.6).exp2();
            let src = pe_colour::cct::temp_tint_to_xy(t, -a.tint);
            let d65 = pe_colour::cct::temp_tint_to_xy(6504.0, 0.0);
            let adapt = pe_colour::space::bradford(src, d65);
            let to_xyz = REC2020_TO_XYZ_D65;
            let adapt32: Mat3 = adapt.map(|r| r.map(|v| v as f32));
            let from_xyz = invert(&to_xyz).expect("invertible");
            mul(&from_xyz, &mul(&adapt32, &to_xyz))
        });
        Self {
            wb,
            exposure: a.exposure,
            contrast: a.contrast / 100.0 * 0.6,
            shadows: a.shadows / 100.0 * 1.5,
            highlights: a.highlights / 100.0 * 1.5,
            whites: a.whites / 100.0,
            blacks: a.blacks / 100.0,
            clarity: a.clarity / 100.0 * crate::local::CLARITY_MAX,
            texture: a.texture / 100.0 * crate::local::TEXTURE_MAX,
            saturation: a.saturation / 100.0,
            dehaze: a.dehaze / 100.0,
            sharpness: a.sharpness / 100.0,
            noise: (a.noise / 100.0).clamp(0.0, 1.0),
        }
    }

    /// Luminance gain in log2 stops at log2 luminance relative to mid-grey `l`, given detail terms.
    #[inline]
    pub fn log_gain(&self, l: f32, clarity_detail: f32, texture_detail: f32) -> f32 {
        self.exposure
            + self.contrast * l
            + self.shadows * (1.0 - smoothstep(-5.0, 0.5, l))
            + self.highlights * smoothstep(-0.5, 3.0, l)
            + self.whites * smoothstep(1.5, 4.5, l)
            + self.blacks * (1.0 - smoothstep(-7.0, -3.0, l))
            + self.clarity * clarity_detail
            + self.texture * texture_detail
    }
}

#[derive(Debug, Clone)]
pub struct PreparedMask {
    pub amount: f32,
    pub components: Vec<PreparedComponent>,
    pub adjust: AdjustParams,
}

/// Where and what a pixel is, for evaluating masks.
#[derive(Debug, Clone, Copy)]
pub struct PixelContext {
    /// Oriented-frame pixel position (centres at +0.5).
    pub x: f32,
    pub y: f32,
    /// The pixel after the global tone stage (for range components).
    pub display: [f32; 3],
    /// Clarity and texture detail terms at this pixel (0 when not computed).
    pub clarity: f32,
    pub texture: f32,
    /// Local detail targets (see [`LocalTargets`]); zeros when not computed.
    pub nr_delta: [f32; 3],
    pub dehaze_delta: [f32; 3],
    pub sharp_gain: f32,
    /// The global local-tone gain at this pixel (deltas are relative to the picture before it).
    pub gain: f32,
    pub airlight: [f32; 3],
}

impl PixelContext {
    /// A context with no detail terms, gain 1 (for tests and simple callers).
    pub fn neutral() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            display: [0.0; 3],
            clarity: 0.0,
            texture: 0.0,
            nr_delta: [0.0; 3],
            dehaze_delta: [0.0; 3],
            sharp_gain: 0.0,
            gain: 1.0,
            airlight: [1.0; 3],
        }
    }
}

/// Per-pixel targets for local noise reduction, dehaze and sharpness, computed from the scene before
/// the global local-tone gain: differences to a denoised and a fully dehazed picture, and the
/// luminance gain an unsharp mask would apply (minus 1).
#[derive(Debug, Clone, Default)]
pub struct LocalTargets {
    pub nr_delta: Option<Vec<f32>>,
    pub dehaze_delta: Option<Vec<f32>>,
    pub sharp_gain: Option<Vec<f32>>,
}

/// Everything needed to evaluate a photo's masks.
#[derive(Debug, Clone)]
pub struct MaskParams {
    pub masks: Vec<PreparedMask>,
    pub frame_w: f32,
    pub frame_h: f32,
    /// Whether any mask needs tone-based values (luminance / colour range) or the detail planes.
    pub needs_display: bool,
    pub needs_clarity: bool,
    pub needs_texture: bool,
    pub needs_saturation: bool,
    pub needs_noise: bool,
    pub needs_dehaze: bool,
    pub needs_sharpness: bool,
    pub lms_from_rgb: Mat3,
    pub rgb_from_lms: Mat3,
}

impl MaskParams {
    /// `frame_w`/`frame_h`: oriented frame size in pixels.
    pub fn new(masks: &[Mask], rasters: &Rasters, frame_w: usize, frame_h: usize) -> Self {
        let (fw, fh) = (frame_w as f32, frame_h as f32);
        let aspect = fw / fh.max(1.0);
        let short = fw.min(fh);
        let prepared: Vec<PreparedMask> = masks
            .iter()
            .filter(|m| m.enabled && m.amount > 0.0 && !m.components.is_empty())
            .take(MAX_MASKS)
            .map(|m| PreparedMask {
                amount: (m.amount / 100.0).clamp(0.0, 1.0),
                adjust: AdjustParams::new(&m.adjust),
                components: m
                    .components
                    .iter()
                    .take(MAX_COMPONENTS)
                    .map(|c| PreparedComponent {
                        mode: c.mode,
                        invert: c.invert,
                        shape: match &c.shape {
                            Shape::Linear { x0, y0, x1, y1 } => {
                                let d = [(x1 - x0) * fw, (y1 - y0) * fh];
                                let len2 = (d[0] * d[0] + d[1] * d[1]).max(1e-6);
                                PreparedShape::Linear {
                                    p0: [x0 * fw, y0 * fh],
                                    d,
                                    inv_len2: 1.0 / len2,
                                }
                            }
                            Shape::Radial {
                                cx,
                                cy,
                                rx,
                                ry,
                                angle,
                                feather,
                            } => {
                                let a = angle.to_radians();
                                PreparedShape::Radial {
                                    c: [cx * fw, cy * fh],
                                    cos: a.cos(),
                                    sin: a.sin(),
                                    inv_rx: 1.0 / (rx * fw).max(1e-3),
                                    inv_ry: 1.0 / (ry * fh).max(1e-3),
                                    inner: 1.0 - feather.clamp(0.0, 1.0),
                                }
                            }
                            Shape::Brush { strokes } => {
                                PreparedShape::Raster(brush_raster_cached(strokes, aspect))
                            }
                            Shape::Raster { raster_id, .. } => match rasters.get(raster_id) {
                                Some(r) => PreparedShape::Raster(Arc::new(r.clone())),
                                None => PreparedShape::Nothing,
                            },
                            Shape::Luminance { lo, hi, smoothness } => PreparedShape::Luminance {
                                lo: lo.min(*hi),
                                hi: hi.max(*lo),
                                soft: 0.01 + 0.25 * smoothness.clamp(0.0, 1.0),
                            },
                            Shape::Color { colors, tolerance } => PreparedShape::Color {
                                colors: colors.clone(),
                                tol: 0.02 + tolerance.clamp(0.0, 100.0) / 100.0 * 0.25,
                            },
                        },
                    })
                    .collect(),
            })
            .collect();
        let _ = short;
        let any = |f: &dyn Fn(&PreparedMask) -> bool| prepared.iter().any(f);
        let srgb_from_rec = ColorSpace::Srgb.from_space(&REC2020_TO_XYZ_D65);
        let lms_from_rgb = mul(&crate::look::srgb_to_lms(), &srgb_from_rec);
        Self {
            needs_display: any(&|m| {
                m.components.iter().any(|c| {
                    matches!(
                        c.shape,
                        PreparedShape::Luminance { .. } | PreparedShape::Color { .. }
                    )
                })
            }),
            needs_clarity: any(&|m| m.adjust.clarity != 0.0),
            needs_texture: any(&|m| m.adjust.texture != 0.0),
            needs_saturation: any(&|m| m.adjust.saturation != 0.0),
            needs_noise: any(&|m| m.adjust.noise > 0.0),
            needs_dehaze: any(&|m| m.adjust.dehaze > 0.0),
            needs_sharpness: any(&|m| m.adjust.sharpness != 0.0),
            masks: prepared,
            frame_w: fw,
            frame_h: fh,
            rgb_from_lms: invert(&lms_from_rgb).expect("invertible"),
            lms_from_rgb,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.masks.is_empty()
    }

    /// OKLab of a display-linear Rec.2020 colour.
    #[inline]
    pub fn oklab(&self, d: [f32; 3]) -> [f32; 3] {
        let lms = pe_colour::apply(&self.lms_from_rgb, d).map(f32::cbrt);
        pe_colour::apply(&LMS_TO_LAB, lms)
    }

    /// Mask value of `m` at oriented-frame pixel position (px, py) (centres at +0.5); `display` is
    /// the pixel after the global tone stage (only used by range components).
    pub fn value(&self, m: &PreparedMask, px: f32, py: f32, display: [f32; 3]) -> f32 {
        let mut acc = 0.0f32;
        let mut lab: Option<[f32; 3]> = None;
        for (k, c) in m.components.iter().enumerate() {
            let mut v = match &c.shape {
                PreparedShape::Linear { p0, d, inv_len2 } => {
                    let t = ((px - p0[0]) * d[0] + (py - p0[1]) * d[1]) * inv_len2;
                    1.0 - smoothstep(0.0, 1.0, t)
                }
                PreparedShape::Radial {
                    c,
                    cos,
                    sin,
                    inv_rx,
                    inv_ry,
                    inner,
                } => {
                    let (dx, dy) = (px - c[0], py - c[1]);
                    let u = (dx * cos + dy * sin) * inv_rx;
                    let w = (-dx * sin + dy * cos) * inv_ry;
                    let r = (u * u + w * w).sqrt();
                    1.0 - smoothstep(*inner, 1.0, r)
                }
                PreparedShape::Raster(r) => r.sample(px / self.frame_w, py / self.frame_h),
                PreparedShape::Luminance { lo, hi, soft } => {
                    let l = self.oklab(display)[0];
                    smoothstep(lo - soft, *lo, l) * (1.0 - smoothstep(*hi, hi + soft, l))
                }
                PreparedShape::Color { colors, tol } => {
                    let p = *lab.get_or_insert_with(|| self.oklab(display));
                    let mut best = 0.0f32;
                    for q in colors {
                        let d = 2.0 * ((p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt()
                            + 0.5 * (p[0] - q[0]).abs();
                        best = best.max(1.0 - smoothstep(tol * 0.5, *tol, d));
                    }
                    best
                }
                PreparedShape::Nothing => 0.0,
            };
            if c.invert {
                v = 1.0 - v;
            }
            acc = if k == 0 {
                match c.mode {
                    Combine::Add => v,
                    // A leading subtract or intersect starts from an empty selection.
                    _ => 0.0,
                }
            } else {
                match c.mode {
                    Combine::Add => acc + v - acc * v,
                    Combine::Subtract => acc * (1.0 - v),
                    Combine::Intersect => acc * v,
                }
            };
        }
        acc * m.amount
    }

    /// Apply the scene-linear part of every mask to one pixel. `values[i]` receives mask i's value
    /// (for the saturation step later).
    pub fn apply_scene(
        &self,
        c: [f32; 3],
        at: &PixelContext,
        values: &mut [f32; MAX_MASKS],
        luma: [f32; 3],
    ) -> [f32; 3] {
        let mut c = c;
        let (clarity_detail, texture_detail) = (at.clarity, at.texture);
        for (i, m) in self.masks.iter().enumerate() {
            let v = self.value(m, at.x, at.y, at.display);
            values[i] = v;
            if v <= 0.0 {
                continue;
            }
            let a = &m.adjust;
            if a.noise > 0.0 {
                let k = v * a.noise * at.gain;
                c = [0, 1, 2].map(|j| (c[j] + k * at.nr_delta[j]).max(0.0));
            }
            if a.dehaze > 0.0 {
                let k = v * a.dehaze * at.gain;
                c = [0, 1, 2].map(|j| (c[j] + k * at.dehaze_delta[j]).max(0.0));
            } else if a.dehaze < 0.0 {
                let k = v * -a.dehaze * LOCAL_HAZE_MAX;
                c = [0, 1, 2].map(|j| c[j] + k * (at.airlight[j] - c[j]));
            }
            if a.sharpness != 0.0 {
                let k = (1.0 + v * a.sharpness * at.sharp_gain).max(0.0);
                c = c.map(|x| x * k);
            }
            if let Some(wb) = &m.adjust.wb {
                let a = pe_colour::apply(wb, c);
                c = [0, 1, 2].map(|k| c[k] + v * (a[k] - c[k]));
            }
            let y = (c[0] * luma[0] + c[1] * luma[1] + c[2] * luma[2]).max(1e-6);
            let l = (y / MID_GREY).log2();
            let g = (v * m.adjust.log_gain(l, clarity_detail, texture_detail)).exp2();
            c = c.map(|x| x * g);
        }
        c
    }

    /// Apply masked saturation to a display-linear pixel (after the colour stage).
    pub fn apply_saturation(&self, d: [f32; 3], values: &[f32; MAX_MASKS]) -> [f32; 3] {
        let mut gain = 1.0f32;
        for (i, m) in self.masks.iter().enumerate() {
            gain *= (1.0 + values[i] * m.adjust.saturation).max(0.0);
        }
        if gain == 1.0 {
            return d;
        }
        let [l, a, b] = self.oklab(d);
        let lms = pe_colour::apply(&LAB_TO_LMS, [l, a * gain, b * gain]).map(|v| v * v * v);
        pe_colour::apply(&self.rgb_from_lms, lms)
    }
}

/// Oriented-frame pixel position of sensor pixel (x, y) under EXIF orientation `code` (inverse of
/// the mapping used by `orient` / `geometry`). `w`, `h`: sensor size.
#[inline]
pub fn frame_position(x: usize, y: usize, w: usize, h: usize, code: u8) -> (f32, f32) {
    frame_position_at(x as f32 + 0.5, y as f32 + 0.5, w, h, code)
}

/// [`frame_position`] for a continuous sensor position.
#[inline]
pub fn frame_position_at(sx: f32, sy: f32, w: usize, h: usize, code: u8) -> (f32, f32) {
    let (sw, sh) = (w as f32, h as f32);
    match code {
        2 => (sw - sx, sy),
        3 => (sw - sx, sh - sy),
        4 => (sx, sh - sy),
        5 => (sy, sx),
        6 => (sh - sy, sx),
        7 => (sh - sy, sw - sx),
        8 => (sy, sw - sx),
        _ => (sx, sy),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(components: Vec<MaskComponent>) -> Mask {
        Mask {
            id: "m".into(),
            name: String::new(),
            enabled: true,
            amount: 100.0,
            components,
            adjust: LocalAdjust::default(),
        }
    }

    fn comp(mode: Combine, invert: bool, shape: Shape) -> MaskComponent {
        MaskComponent {
            mode,
            invert,
            shape,
        }
    }

    fn eval(m: &Mask, fx: f32, fy: f32) -> f32 {
        let p = MaskParams::new(std::slice::from_ref(m), &Rasters::default(), 200, 100);
        p.value(&p.masks[0], fx * 200.0, fy * 100.0, [0.18; 3])
    }

    #[test]
    fn linear_and_radial_shapes() {
        let lin = mask(vec![comp(
            Combine::Add,
            false,
            Shape::Linear {
                x0: 0.0,
                y0: 0.5,
                x1: 1.0,
                y1: 0.5,
            },
        )]);
        assert!((eval(&lin, 0.0, 0.2) - 1.0).abs() < 1e-5);
        assert!((eval(&lin, 0.5, 0.9) - 0.5).abs() < 1e-5);
        assert!(eval(&lin, 1.0, 0.5).abs() < 1e-5);
        let rad = mask(vec![comp(
            Combine::Add,
            false,
            Shape::Radial {
                cx: 0.5,
                cy: 0.5,
                rx: 0.25,
                ry: 0.25,
                angle: 0.0,
                feather: 0.0,
            },
        )]);
        assert_eq!(eval(&rad, 0.5, 0.5), 1.0);
        assert_eq!(
            eval(&rad, 0.70, 0.5),
            1.0,
            "inside (rx is a fraction of width)"
        );
        assert_eq!(eval(&rad, 0.80, 0.5), 0.0, "outside");
        assert_eq!(eval(&rad, 0.5, 0.80), 0.0, "ry is a fraction of height");
    }

    #[test]
    fn composition_and_invert() {
        let left = Shape::Linear {
            x0: 0.0,
            y0: 0.0,
            x1: 0.0001,
            y1: 0.0,
        }; // ~1 at x=0, 0 elsewhere
        let disc = Shape::Radial {
            cx: 0.5,
            cy: 0.5,
            rx: 0.3,
            ry: 0.6,
            angle: 0.0,
            feather: 0.0,
        };
        let sub = mask(vec![
            comp(Combine::Add, true, left.clone()),
            comp(Combine::Subtract, false, disc.clone()),
        ]);
        assert_eq!(
            eval(&sub, 0.9, 0.5),
            1.0,
            "inverted gradient selects the right side"
        );
        assert_eq!(eval(&sub, 0.5, 0.5), 0.0, "disc subtracted");
        let inter = mask(vec![
            comp(Combine::Add, true, left.clone()),
            comp(Combine::Intersect, false, disc.clone()),
        ]);
        assert_eq!(eval(&inter, 0.5, 0.5), 1.0);
        assert_eq!(eval(&inter, 0.95, 0.5), 0.0);
        let union = mask(vec![
            comp(Combine::Add, false, disc),
            comp(Combine::Add, false, left),
        ]);
        assert!(eval(&union, 0.0, 0.0) > 0.99 && eval(&union, 0.5, 0.5) > 0.99);
        let mut half = union.clone();
        half.amount = 50.0;
        assert!((eval(&half, 0.5, 0.5) - 0.5).abs() < 1e-5, "amount scales");
    }

    #[test]
    fn brush_raster_paint_and_erase() {
        let paint = Stroke {
            points: vec![[0.2, 0.5], [0.8, 0.5]],
            size: 0.1,
            feather: 0.0,
            flow: 1.0,
            erase: false,
        };
        let erase = Stroke {
            points: vec![[0.5, 0.5]],
            size: 0.1,
            feather: 0.0,
            flow: 1.0,
            erase: true,
        };
        let r = rasterise_brush(&[paint.clone(), erase], 2.0);
        assert_eq!((r.width, r.height), (1024, 512));
        assert_eq!(r.sample(0.3, 0.5), 1.0, "on the stroke");
        assert_eq!(r.sample(0.3, 0.9), 0.0, "off the stroke");
        assert_eq!(r.sample(0.5, 0.5), 0.0, "erased");
        let m = mask(vec![comp(
            Combine::Add,
            false,
            Shape::Brush {
                strokes: vec![paint],
            },
        )]);
        assert_eq!(eval(&m, 0.7, 0.5), 1.0);
    }

    #[test]
    fn luminance_and_colour_ranges() {
        let p = MaskParams::new(
            &[mask(vec![comp(
                Combine::Add,
                false,
                Shape::Luminance {
                    lo: 0.6,
                    hi: 1.0,
                    smoothness: 0.0,
                },
            )])],
            &Rasters::default(),
            10,
            10,
        );
        assert!(
            p.value(&p.masks[0], 1.0, 1.0, [0.8; 3]) > 0.99,
            "bright selected"
        );
        assert_eq!(p.value(&p.masks[0], 1.0, 1.0, [0.02; 3]), 0.0, "dark not");
        let red = [0.6, 0.1, 0.08];
        let target = p.oklab(red);
        let c = MaskParams::new(
            &[mask(vec![comp(
                Combine::Add,
                false,
                Shape::Color {
                    colors: vec![target],
                    tolerance: 20.0,
                },
            )])],
            &Rasters::default(),
            10,
            10,
        );
        assert!(c.value(&c.masks[0], 0.0, 0.0, red) > 0.99);
        assert_eq!(c.value(&c.masks[0], 0.0, 0.0, [0.1, 0.2, 0.6]), 0.0);
    }

    #[test]
    fn local_adjustments_change_only_where_masked() {
        let m = Mask {
            adjust: LocalAdjust {
                exposure: 1.0,
                temperature: 60.0,
                saturation: -100.0,
                ..Default::default()
            },
            ..mask(vec![comp(
                Combine::Add,
                false,
                Shape::Linear {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 0.0,
                    y1: 0.5,
                },
            )])
        };
        let p = MaskParams::new(&[m], &Rasters::default(), 100, 100);
        let luma = pe_colour::REC2020_LUMA;
        let mut vals = [0.0; MAX_MASKS];
        let grey = [0.18; 3];
        let top = p.apply_scene(
            grey,
            &PixelContext {
                x: 50.0,
                y: 0.5,
                display: grey,
                ..PixelContext::neutral()
            },
            &mut vals,
            luma,
        );
        assert!(top[0] > top[2], "warmer: {top:?}");
        assert!(top[1] > 0.3, "brighter by about a stop: {top:?}");
        let sat = p.apply_saturation([0.5, 0.2, 0.1], &vals);
        assert!(
            (sat[0] - sat[1]).abs() < 1e-3 && (sat[1] - sat[2]).abs() < 1e-3,
            "desaturated"
        );
        let bottom = p.apply_scene(
            grey,
            &PixelContext {
                x: 50.0,
                y: 99.5,
                display: grey,
                ..PixelContext::neutral()
            },
            &mut vals,
            luma,
        );
        assert_eq!(bottom, grey, "unmasked area untouched");
    }

    #[test]
    fn frame_position_inverts_orientation() {
        let (w, h) = (6, 4);
        for code in 1..=8u8 {
            let g = crate::geometry::GeometryParams::new(&crate::Settings::default(), w, h, code);
            for (x, y) in [(0, 0), (5, 3), (2, 1)] {
                let (px, py) = frame_position(x, y, w, h, g.orientation);
                // Map back through geometry's orientation step: should land on the sensor pixel centre.
                let (sw, sh) = (w as f32, h as f32);
                let (sx, sy) = match g.orientation {
                    2 => (sw - px, py),
                    3 => (sw - px, sh - py),
                    4 => (px, sh - py),
                    5 => (py, px),
                    6 => (py, sh - px),
                    7 => (sw - py, sh - px),
                    8 => (sw - py, px),
                    _ => (px, py),
                };
                assert!(
                    (sx - (x as f32 + 0.5)).abs() < 1e-5 && (sy - (y as f32 + 0.5)).abs() < 1e-5,
                    "code {code}"
                );
            }
        }
    }
}
