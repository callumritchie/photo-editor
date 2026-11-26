//! Colour stage: vibrance and saturation, the colour mixer (hue / saturation / luminance per hue band),
//! black-and-white conversion with a per-band mix, and colour grading by tonal region.
//!
//! Design (this project's own):
//! - Runs on display-linear Rec.2020 values after the tone stage, in OKLab / OKLCh (Björn Ottosson,
//!   "A perceptual color space for image processing", 2020), where lightness, chroma and hue are close
//!   to independent, so moving one slider does not drag the others along.
//! - Hue bands: eight centres on the OKLCh hue circle; a pixel belongs to the two bands either side of
//!   its hue with smooth (smoothstep) weights that sum to one. Hue-specific changes fade out towards
//!   neutral greys, which have no meaningful hue.
//! - Vibrance boosts low-chroma colours most and spares skin hues; saturation scales chroma uniformly.
//! - Black and white: OKLab lightness, brightened or darkened per hue band by the mix sliders.
//! - Colour grading: shadows / midtones / highlights weights from lightness (balance moves the split,
//!   blending widens the transitions); each region adds a tint in the a/b plane and shifts lightness.
//!
//! [`LookParams`] holds every derived constant, so the GPU shader only repeats the per-pixel maths.

use pe_colour::{invert, mul, ColorSpace, Mat3, REC2020_TO_XYZ_D65};

use crate::settings::{Settings, Treatment, HUE_BANDS};

/// Band centres on the OKLCh hue circle, in degrees: red, orange, yellow, green, aqua, blue, purple,
/// magenta.
pub const BAND_CENTRES: [f32; HUE_BANDS] = [25.0, 60.0, 105.0, 145.0, 195.0, 260.0, 300.0, 340.0];

/// Hue rotation at +/-100 on a colour-mixer hue slider, in degrees.
pub const HUE_SHIFT_MAX: f32 = 30.0;
/// Lightness change at +/-100 on a mixer luminance slider, in stops of OKLab L.
pub const LUM_STOPS_MAX: f32 = 0.6;
/// Lightness change at +/-100 on a black-and-white mix slider, in stops of OKLab L.
pub const BW_STOPS_MAX: f32 = 0.8;
/// Chroma (OKLab a/b length) added by a grading region at saturation 100.
pub const GRADE_CHROMA_MAX: f32 = 0.08;
/// Lightness shift of a grading region at luminance +/-100.
pub const GRADE_LUM_MAX: f32 = 0.1;

/// OKLab: linear sRGB -> LMS (Ottosson 2020).
const SRGB_TO_LMS: Mat3 = [
    [0.412_221_46, 0.536_332_55, 0.051_445_995],
    [0.211_903_5, 0.680_699_5, 0.107_396_96],
    [0.088_302_46, 0.281_718_85, 0.629_978_7],
];
/// OKLab: cube-rooted LMS -> Lab.
pub const LMS_TO_LAB: Mat3 = [
    [0.210_454_26, 0.793_617_8, -0.004_072_047],
    [1.977_998_5, -2.428_592_2, 0.450_593_7],
    [0.025_904_037, 0.782_771_77, -0.808_675_77],
];
/// OKLab: Lab -> cube-rooted LMS.
pub const LAB_TO_LMS: Mat3 = [
    [1.0, 0.396_337_78, 0.215_803_76],
    [1.0, -0.105_561_346, -0.063_854_17],
    [1.0, -0.089_484_18, -1.291_485_5],
];

/// OKLab's linear sRGB -> LMS matrix.
pub fn srgb_to_lms() -> Mat3 {
    SRGB_TO_LMS
}

/// Derived constants for the colour stage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LookParams {
    pub enabled: bool,
    pub monochrome: bool,
    /// Uniform chroma gain from Saturation.
    pub saturation_gain: f32,
    /// Vibrance amount, -1..1.
    pub vibrance: f32,
    /// Linear Rec.2020 -> LMS, and back.
    pub lms_from_rgb: Mat3,
    pub rgb_from_lms: Mat3,
    pub centres: [f32; HUE_BANDS],
    /// Per band: hue shift in degrees, chroma gain - 1, lightness change in stops.
    pub band_hue: [f32; HUE_BANDS],
    pub band_sat: [f32; HUE_BANDS],
    pub band_lum: [f32; HUE_BANDS],
    /// Per band: black-and-white lightness change in stops.
    pub band_bw: [f32; HUE_BANDS],
    /// Grading tints (a, b, lightness shift) for shadows, midtones, highlights, global.
    pub grade: [[f32; 3]; 4],
    /// Half-width of the region transitions and the shift of the split points (in OKLab L).
    pub grade_width: f32,
    pub grade_balance: f32,
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl LookParams {
    pub fn new(s: &Settings) -> Self {
        let srgb_from_rec = ColorSpace::Srgb.from_space(&REC2020_TO_XYZ_D65);
        let lms_from_rgb = mul(&SRGB_TO_LMS, &srgb_from_rec);
        let rgb_from_lms = invert(&lms_from_rgb).expect("OKLab matrix is invertible");
        let g = &s.grading;
        let tint = |w: &crate::settings::GradeWheel| {
            let c = w.saturation / 100.0 * GRADE_CHROMA_MAX;
            let h = w.hue.to_radians();
            [
                c * h.cos(),
                c * h.sin(),
                w.luminance / 100.0 * GRADE_LUM_MAX,
            ]
        };
        let grade = [
            tint(&g.shadows),
            tint(&g.midtones),
            tint(&g.highlights),
            tint(&g.global),
        ];
        let monochrome = s.treatment == Treatment::Monochrome;
        let any = |a: &[f32]| a.iter().any(|v| *v != 0.0);
        let enabled = monochrome
            || s.vibrance != 0.0
            || s.saturation != 0.0
            || any(&s.hsl.hue)
            || any(&s.hsl.saturation)
            || any(&s.hsl.luminance)
            || grade.iter().any(|t| any(t));
        Self {
            enabled,
            monochrome,
            saturation_gain: 1.0 + s.saturation / 100.0,
            vibrance: s.vibrance / 100.0,
            lms_from_rgb,
            rgb_from_lms,
            centres: BAND_CENTRES,
            band_hue: s.hsl.hue.map(|v| v / 100.0 * HUE_SHIFT_MAX),
            band_sat: s.hsl.saturation.map(|v| v / 100.0),
            band_lum: s.hsl.luminance.map(|v| v / 100.0 * LUM_STOPS_MAX),
            band_bw: s.bw_mix.map(|v| v / 100.0 * BW_STOPS_MAX),
            grade,
            grade_width: 0.05 + 0.25 * g.blending / 100.0,
            grade_balance: g.balance / 100.0 * 0.2,
        }
    }

    /// The two bands around hue `h` (degrees, 0..360) and the weight of the second.
    #[inline]
    pub fn bands(&self, h: f32) -> (usize, usize, f32) {
        let c = &self.centres;
        let mut i = HUE_BANDS - 1;
        for (k, &ck) in c.iter().enumerate() {
            if h >= ck {
                i = k;
            }
        }
        let j = (i + 1) % HUE_BANDS;
        let start = c[i];
        let mut span = c[j] - start;
        let mut d = h - start;
        if span <= 0.0 {
            span += 360.0;
        }
        if d < 0.0 {
            d += 360.0;
        }
        (i, j, smoothstep(0.0, 1.0, d / span))
    }

    /// Display-linear Rec.2020 in, display-linear Rec.2020 out.
    #[inline]
    pub fn apply(&self, d: [f32; 3]) -> [f32; 3] {
        if !self.enabled {
            return d;
        }
        let lms = pe_colour::apply(&self.lms_from_rgb, d).map(f32::cbrt);
        let [mut l, a, b] = pe_colour::apply(&LMS_TO_LAB, lms);
        let c0 = (a * a + b * b).sqrt();
        let mut h = b.atan2(a).to_degrees();
        if h < 0.0 {
            h += 360.0;
        }
        let (i, j, t) = self.bands(h);
        let mix = |v: &[f32; HUE_BANDS]| v[i] * (1.0 - t) + v[j] * t;
        let chroma_w = smoothstep(0.0, 0.06, c0);

        // Colour mixer.
        h += mix(&self.band_hue);
        let mut c = c0 * (1.0 + mix(&self.band_sat)).max(0.0);
        l *= (mix(&self.band_lum) * chroma_w).exp2();

        // Saturation and vibrance.
        c *= self.saturation_gain;
        if self.vibrance != 0.0 {
            let low = 1.0 - smoothstep(0.0, 0.2, c);
            let skin = 1.0 - 0.5 * (1.0 - smoothstep(0.0, 30.0, (h - 60.0).abs()));
            let v = if self.vibrance > 0.0 {
                self.vibrance * skin
            } else {
                self.vibrance
            };
            c *= (1.0 + v * low).max(0.0);
        }

        // Black and white.
        if self.monochrome {
            l *= (mix(&self.band_bw) * chroma_w).exp2();
            c = 0.0;
        }

        let hr = h.to_radians();
        let (mut a2, mut b2) = (c * hr.cos(), c * hr.sin());

        // Colour grading.
        let (wd, bal) = (self.grade_width, self.grade_balance);
        let ws = 1.0 - smoothstep(0.33 + bal - wd, 0.33 + bal + wd, l);
        let wh = smoothstep(0.66 + bal - wd, 0.66 + bal + wd, l);
        let wm = (1.0 - ws - wh).max(0.0);
        let mut dl = 0.0;
        for (w, g) in [
            (ws, self.grade[0]),
            (wm, self.grade[1]),
            (wh, self.grade[2]),
            (1.0, self.grade[3]),
        ] {
            a2 += w * g[0];
            b2 += w * g[1];
            dl += w * g[2];
        }
        l = (l + dl).max(0.0);

        let lms2 = pe_colour::apply(&LAB_TO_LMS, [l, a2, b2]).map(|v| v * v * v);
        pe_colour::apply(&self.rgb_from_lms, lms2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{GradeWheel, Hsl};

    fn rec2020_from_srgb(c: [f32; 3]) -> [f32; 3] {
        let m = invert(&ColorSpace::Srgb.from_space(&REC2020_TO_XYZ_D65)).unwrap();
        pe_colour::apply(&m, c)
    }

    fn close(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
        (0..3).all(|k| (a[k] - b[k]).abs() < tol)
    }

    fn enabled_noop() -> LookParams {
        // Enabled but with every amount at zero: exercises the round trip through OKLab.
        let mut p = LookParams::new(&Settings::default());
        p.enabled = true;
        p
    }

    #[test]
    fn neutral_settings_are_identity() {
        let p = LookParams::new(&Settings::default());
        assert!(!p.enabled);
        let q = enabled_noop();
        for c in [
            [0.2, 0.4, 0.6],
            [0.9, 0.1, 0.05],
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
        ] {
            assert!(close(q.apply(c), c, 1e-4), "{c:?} -> {:?}", q.apply(c));
        }
    }

    #[test]
    fn oklab_of_white_and_red_match_published_values() {
        let p = enabled_noop();
        let lab = |rgb| {
            let lms = pe_colour::apply(&p.lms_from_rgb, rec2020_from_srgb(rgb)).map(f32::cbrt);
            pe_colour::apply(&LMS_TO_LAB, lms)
        };
        // Ottosson's reference: sRGB white -> (1, 0, 0); sRGB red -> (0.628, 0.225, 0.126).
        assert!(close(lab([1.0, 1.0, 1.0]), [1.0, 0.0, 0.0], 2e-3));
        assert!(close(lab([1.0, 0.0, 0.0]), [0.628, 0.225, 0.126], 2e-3));
    }

    #[test]
    fn saturation_and_monochrome_make_neutrals() {
        let red = rec2020_from_srgb([0.8, 0.2, 0.1]);
        for s in [
            Settings {
                saturation: -100.0,
                ..Default::default()
            },
            Settings {
                treatment: Treatment::Monochrome,
                ..Default::default()
            },
        ] {
            let o = LookParams::new(&s).apply(red);
            assert!(
                (o[0] - o[1]).abs() < 1e-4 && (o[1] - o[2]).abs() < 1e-4,
                "{o:?}"
            );
        }
    }

    #[test]
    fn mixer_targets_its_band_only() {
        let s = Settings {
            hsl: Hsl {
                luminance: [-100.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                ..Default::default()
            },
            ..Default::default()
        };
        let p = LookParams::new(&s);
        let red = rec2020_from_srgb([0.8, 0.15, 0.12]);
        let blue = rec2020_from_srgb([0.1, 0.2, 0.8]);
        let grey = [0.3, 0.3, 0.3];
        let y = |c: [f32; 3]| c[0] * 0.2627 + c[1] * 0.678 + c[2] * 0.0593;
        assert!(y(p.apply(red)) < y(red) * 0.8, "red darkened");
        assert!(close(p.apply(blue), blue, 1e-3), "blue untouched");
        assert!(close(p.apply(grey), grey, 1e-4), "grey untouched");
    }

    #[test]
    fn band_weights_are_continuous_around_the_circle() {
        let p = enabled_noop();
        for k in 0..3600 {
            let h = k as f32 / 10.0;
            let (i, j, t) = p.bands(h);
            assert!(
                j == (i + 1) % HUE_BANDS && (0.0..=1.0).contains(&t),
                "h={h}"
            );
            if (h - p.centres[i]).abs() < 1e-3 {
                assert!(t < 1e-3, "at a centre the band has full weight");
            }
        }
    }

    #[test]
    fn grading_tints_shadows_more_than_highlights() {
        let s = Settings {
            grading: crate::settings::Grading {
                shadows: GradeWheel {
                    hue: 250.0,
                    saturation: 100.0,
                    luminance: 0.0,
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let p = LookParams::new(&s);
        let blue_shift = |v: f32| {
            let o = p.apply([v, v, v]);
            o[2] - o[0]
        };
        assert!(blue_shift(0.02) > 0.005, "dark grey turns blue");
        assert!(
            blue_shift(0.9).abs() < blue_shift(0.02) * 0.1,
            "bright grey barely changes"
        );
    }
}
