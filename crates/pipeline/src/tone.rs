//! Tone stages: local shadows/highlights, contrast, filmic display transform, black level,
//! gamut mapping and the point curve.
//!
//! Design (this project's own):
//! - Work on scene-linear RGB in a wide working space (Rec.2020). Luminance `Y` uses Rec.2020 weights.
//! - **Shadows / highlights**: split log2-luminance into a smooth *base* (edge-preserving fast guided
//!   filter) and *detail*. Shift the base by up to +/-`LOCAL_MAX_EV` stops, weighted by how dark or
//!   bright the base is relative to mid-grey, then apply the shift to RGB as a gain. Detail is untouched,
//!   so local contrast survives.
//! - **Contrast**: a power curve on luminance pivoting at scene mid-grey (0.18), applied as RGB gain.
//! - **Display transform**: Narkowicz's (2015) curve fit to the ACES reference rendering, applied to the
//!   max(R,G,B) norm with RGB ratios preserved (hue-stable), normalised so a chosen scene *white point*
//!   maps to display 1.0 (the Whites slider moves it). Colours approaching white are desaturated so
//!   very bright saturated light rolls off to white instead of clipping as a flat colour.
//! - **Blacks**: an offset of the display black level.
//! - **Gamut mapping**: after conversion to the output space, out-of-gamut (negative) components are
//!   brought in by desaturating towards the pixel's luminance.
//! - **Point curve**: monotone cubic (Fritsch & Carlson 1980) through user points, on encoded values.

pub const MID_GREY: f32 = 0.18;
/// Maximum base-layer shift for Shadows/Highlights at +/-100, in stops.
pub const LOCAL_MAX_EV: f32 = 1.5;

#[inline]
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
pub fn luminance(px: &[f32], weights: [f32; 3]) -> f32 {
    px[0] * weights[0] + px[1] * weights[1] + px[2] * weights[2]
}

/// Guided-filter regularisation for Shadows/Highlights, in stops squared: edges above ~half a stop stay.
pub const LOCAL_EPS: f32 = 0.25;

/// Guided-filter radius (~2% of the short side) and the reduction factor it is computed at.
pub fn local_tone_geometry(width: usize, height: usize) -> (usize, usize) {
    let short = width.min(height);
    let radius = (short as f32 * 0.02).round().max(4.0) as usize;
    (radius, (short / 512).max(1))
}

/// Shadows/highlights shift (stops) for a base log2-luminance `d` relative to mid-grey.
#[inline]
pub fn local_tone_shift(d: f32, shadow_ev: f32, highlight_ev: f32) -> f32 {
    let w_shadow = 1.0 - smoothstep(-5.0, 0.5, d);
    let w_high = smoothstep(-0.5, 3.0, d);
    shadow_ev * w_shadow + highlight_ev * w_high
}

/// Base-layer shifts in stops for the Shadows and Highlights sliders.
pub fn local_tone_amounts(shadows: f32, highlights: f32) -> (f32, f32) {
    (
        shadows / 100.0 * LOCAL_MAX_EV,
        highlights / 100.0 * LOCAL_MAX_EV,
    )
}

/// Narkowicz 2015 ACES-fitted curve (scene-linear -> display-linear, unnormalised).
#[inline]
fn aces_fit(x: f32) -> f32 {
    let x = x.max(0.0);
    (x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14)
}

/// Per-pixel parameters derived from the global tone sliders.
#[derive(Debug, Clone, Copy)]
pub struct ToneParams {
    pub contrast_power: f32,
    pub white_point: f32,
    pub white_norm: f32,
    pub black: f32,
}

impl ToneParams {
    pub fn new(contrast: f32, whites: f32, blacks: f32) -> Self {
        // Default scene white point 16 (~4.5 stops above mid-grey); Whites +/-100 moves it by 2 stops.
        let white_point = 16.0 * (-whites / 50.0).exp2();
        Self {
            contrast_power: 1.0 + contrast / 100.0 * 0.6,
            white_point,
            white_norm: 1.0 / aces_fit(white_point),
            black: -blacks / 100.0 * 0.05,
        }
    }

    /// Scene-linear RGB (working space) -> display-linear RGB (working space, may be out of gamut).
    #[inline]
    pub fn apply(&self, px: [f32; 3], weights: [f32; 3]) -> [f32; 3] {
        let mut c = px;
        // Contrast: power law on luminance around mid-grey, as an RGB gain.
        if self.contrast_power != 1.0 {
            let y = luminance(&c, weights);
            if y > 1e-6 {
                let g = (y / MID_GREY).powf(self.contrast_power) * MID_GREY / y;
                c = [c[0] * g, c[1] * g, c[2] * g];
            }
        }
        // Display transform on the max-RGB norm, preserving ratios.
        let n = c[0].max(c[1]).max(c[2]);
        if n <= 0.0 {
            return [0.0; 3];
        }
        let yd = (aces_fit(n) * self.white_norm).min(1.0);
        let mut out = [c[0] / n * yd, c[1] / n * yd, c[2] / n * yd];
        // Roll bright colours off to white.
        let t = smoothstep(0.8, 1.0, yd);
        if t > 0.0 {
            for v in &mut out {
                *v += (yd - *v) * t;
            }
        }
        // Black level.
        if self.black != 0.0 {
            for v in &mut out {
                *v = ((*v - self.black) / (1.0 - self.black)).max(0.0);
            }
        }
        out
    }

    pub fn white_point(&self) -> f32 {
        self.white_point
    }
}

/// Bring a linear RGB value into [0, 1] by desaturating towards its luminance, then clipping.
#[inline]
pub fn gamut_map(px: [f32; 3], weights: [f32; 3]) -> [f32; 3] {
    let y = luminance(&px, weights).clamp(0.0, 1.0);
    let m = px[0].min(px[1]).min(px[2]);
    let mut c = px;
    if m < 0.0 {
        let t = (-m / (y - m).max(1e-6)).min(1.0);
        for v in &mut c {
            *v += (y - *v) * t;
        }
    }
    [
        c[0].clamp(0.0, 1.0),
        c[1].clamp(0.0, 1.0),
        c[2].clamp(0.0, 1.0),
    ]
}

/// Lookup table for a monotone cubic point curve on 0..1 encoded values.
pub struct CurveLut {
    lut: Vec<f32>,
}

impl CurveLut {
    const SIZE: usize = 4096;

    /// `points` are (input, output) on 0..255. Returns `None` for an empty or identity curve.
    pub fn new(points: &[[f32; 2]]) -> Option<Self> {
        let mut pts: Vec<(f32, f32)> = points
            .iter()
            .map(|p| (p[0] / 255.0, p[1] / 255.0))
            .collect();
        pts.sort_by(|a, b| a.0.total_cmp(&b.0));
        pts.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-6);
        if pts.len() < 2 || pts.iter().all(|(x, y)| (x - y).abs() < 1e-6) {
            return None;
        }
        let n = pts.len();
        let d: Vec<f32> = (0..n - 1)
            .map(|i| (pts[i + 1].1 - pts[i].1) / (pts[i + 1].0 - pts[i].0))
            .collect();
        // Fritsch-Carlson tangents.
        let mut m = vec![0.0f32; n];
        m[0] = d[0];
        m[n - 1] = d[n - 2];
        for i in 1..n - 1 {
            m[i] = if d[i - 1] * d[i] <= 0.0 {
                0.0
            } else {
                (d[i - 1] + d[i]) / 2.0
            };
        }
        for i in 0..n - 1 {
            if d[i] == 0.0 {
                m[i] = 0.0;
                m[i + 1] = 0.0;
                continue;
            }
            let (a, b) = (m[i] / d[i], m[i + 1] / d[i]);
            let s = a * a + b * b;
            if s > 9.0 {
                let t = 3.0 / s.sqrt();
                m[i] = t * a * d[i];
                m[i + 1] = t * b * d[i];
            }
        }
        let eval = |x: f32| -> f32 {
            if x <= pts[0].0 {
                return pts[0].1;
            }
            if x >= pts[n - 1].0 {
                return pts[n - 1].1;
            }
            let i = pts.partition_point(|p| p.0 <= x) - 1;
            let h = pts[i + 1].0 - pts[i].0;
            let t = (x - pts[i].0) / h;
            let (t2, t3) = (t * t, t * t * t);
            (2.0 * t3 - 3.0 * t2 + 1.0) * pts[i].1
                + (t3 - 2.0 * t2 + t) * h * m[i]
                + (-2.0 * t3 + 3.0 * t2) * pts[i + 1].1
                + (t3 - t2) * h * m[i + 1]
        };
        let lut = (0..Self::SIZE)
            .map(|i| eval(i as f32 / (Self::SIZE - 1) as f32).clamp(0.0, 1.0))
            .collect();
        Some(Self { lut })
    }

    /// The table: `SIZE` samples of the curve on [0, 1].
    pub fn table(&self) -> &[f32] {
        &self.lut
    }

    #[inline]
    pub fn apply(&self, v: f32) -> f32 {
        let x = v.clamp(0.0, 1.0) * (Self::SIZE - 1) as f32;
        let i = (x as usize).min(Self::SIZE - 2);
        let t = x - i as f32;
        self.lut[i] * (1.0 - t) + self.lut[i + 1] * t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const W: [f32; 3] = pe_colour::REC2020_LUMA;

    #[test]
    fn display_transform_is_monotone_and_bounded() {
        let p = ToneParams::new(0.0, 0.0, 0.0);
        let mut prev = -1.0;
        for i in 0..200 {
            let x = (i as f32 / 20.0 - 6.0).exp2();
            let y = p.apply([x, x, x], W)[0];
            assert!(y >= prev && (0.0..=1.0).contains(&y), "x={x} y={y}");
            prev = y;
        }
        // Mid-grey lands in the middle of the display range; the white point maps to 1.
        let g = p.apply([MID_GREY; 3], W)[0];
        assert!((0.2..0.35).contains(&g), "grey -> {g}");
        let w = p.white_point();
        assert!((p.apply([w; 3], W)[0] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn contrast_pivots_on_grey() {
        let base = ToneParams::new(0.0, 0.0, 0.0).apply([MID_GREY; 3], W)[0];
        let more = ToneParams::new(80.0, 0.0, 0.0);
        assert!((more.apply([MID_GREY; 3], W)[0] - base).abs() < 1e-4);
        let dark = MID_GREY / 8.0;
        assert!(
            more.apply([dark; 3], W)[0] < ToneParams::new(0.0, 0.0, 0.0).apply([dark; 3], W)[0]
        );
    }

    #[test]
    fn hue_ratios_preserved_below_rolloff() {
        let p = ToneParams::new(0.0, 0.0, 0.0);
        let out = p.apply([0.10, 0.05, 0.02], W);
        assert!((out[1] / out[0] - 0.5).abs() < 1e-4 && (out[2] / out[0] - 0.2).abs() < 1e-4);
    }

    #[test]
    fn gamut_map_removes_negatives_and_keeps_in_gamut_colours() {
        let c = gamut_map([0.8, -0.1, 0.2], pe_colour::SRGB_TO_XYZ_D65[1]);
        assert!(c.iter().all(|v| (0.0..=1.0).contains(v)));
        assert_eq!(gamut_map([0.3, 0.5, 0.2], [0.2, 0.7, 0.1]), [0.3, 0.5, 0.2]);
    }

    #[test]
    fn curve_identity_and_monotone() {
        assert!(CurveLut::new(&[[0.0, 0.0], [255.0, 255.0]]).is_none());
        let c = CurveLut::new(&[[0.0, 0.0], [64.0, 40.0], [192.0, 215.0], [255.0, 255.0]]).unwrap();
        let mut prev = -1.0;
        for i in 0..=100 {
            let y = c.apply(i as f32 / 100.0);
            assert!(y >= prev - 1e-6);
            prev = y;
        }
        assert!((c.apply(64.0 / 255.0) - 40.0 / 255.0).abs() < 1e-3);
    }
}
