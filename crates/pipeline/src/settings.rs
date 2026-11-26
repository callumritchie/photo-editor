//! Develop settings understood by the pipeline.
//!
//! Slider semantics and ranges are this project's own design. Ranges deliberately follow the common
//! conventions of raw editors (stops for exposure, -100..100 for tone sliders, kelvin/tint for white
//! balance) so presets are easy to reason about.

use serde::{Deserialize, Serialize};

/// White balance mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum WhiteBalance {
    /// Multipliers recorded by the camera.
    #[default]
    AsShot,
    /// Estimated from image content (grey-world over mid-tones).
    Auto,
    /// Explicit correlated colour temperature (kelvin) and tint (see `pe_colour::cct`).
    Custom { temperature: f32, tint: f32 },
}

/// Demosaic algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Demosaic {
    /// Best available for the sensor layout (edge-directed for Bayer, colour-difference for others).
    #[default]
    Auto,
    /// Neighbourhood average: fastest, softest. Useful as a reference.
    Fast,
}

/// Colour or black-and-white rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Treatment {
    #[default]
    Color,
    Monochrome,
}

/// Number of hue bands in the colour mixer and black-and-white mix, in order: red, orange, yellow,
/// green, aqua, blue, purple, magenta.
pub const HUE_BANDS: usize = 8;

/// Colour mixer: per hue band, -100..100 each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Hsl {
    pub hue: [f32; HUE_BANDS],
    pub saturation: [f32; HUE_BANDS],
    pub luminance: [f32; HUE_BANDS],
}

/// One colour-grading region: a tint (hue in degrees 0..360, saturation 0..100) and a luminance
/// shift (-100..100).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct GradeWheel {
    pub hue: f32,
    pub saturation: f32,
    pub luminance: f32,
}

/// Colour grading by tonal region.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Grading {
    pub shadows: GradeWheel,
    pub midtones: GradeWheel,
    pub highlights: GradeWheel,
    pub global: GradeWheel,
    /// Overlap between regions, 0..100 (50 = default).
    pub blending: f32,
    /// Moves the shadow/highlight split: - favours shadows, + highlights. -100..100.
    pub balance: f32,
}

impl Default for Grading {
    fn default() -> Self {
        Self {
            shadows: GradeWheel::default(),
            midtones: GradeWheel::default(),
            highlights: GradeWheel::default(),
            global: GradeWheel::default(),
            blending: 50.0,
            balance: 0.0,
        }
    }
}

/// Capture sharpening.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Sharpening {
    /// 0..150.
    pub amount: f32,
    /// Blur radius in sensor pixels, 0.5..3.
    pub radius: f32,
    /// 0..100: low suppresses halos, high keeps fine texture.
    pub detail: f32,
    /// 0..100: restrict sharpening to edges.
    pub masking: f32,
}

impl Default for Sharpening {
    fn default() -> Self {
        Self {
            amount: 0.0,
            radius: 1.0,
            detail: 25.0,
            masking: 0.0,
        }
    }
}

/// Noise reduction strengths.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NoiseReduction {
    /// 0..100.
    pub luminance: f32,
    /// 0..100: how much fine luminance detail to keep.
    pub detail: f32,
    /// 0..100.
    pub color: f32,
}

impl Default for NoiseReduction {
    fn default() -> Self {
        Self {
            luminance: 0.0,
            detail: 50.0,
            color: 0.0,
        }
    }
}

/// Post-crop vignette.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Vignette {
    /// -100..100: - darkens the edges, + lightens them.
    pub amount: f32,
    /// 0..100: how far from the centre it starts.
    pub midpoint: f32,
    /// -100..100: - towards a rounded rectangle, + towards a circle.
    pub roundness: f32,
    /// 0..100: softness of the transition.
    pub feather: f32,
    /// 0..100: spare bright areas when darkening.
    pub highlights: f32,
}

impl Default for Vignette {
    fn default() -> Self {
        Self {
            amount: 0.0,
            midpoint: 50.0,
            roundness: 0.0,
            feather: 50.0,
            highlights: 0.0,
        }
    }
}

/// Film-like grain.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Grain {
    /// 0..100.
    pub amount: f32,
    /// 0..100: grain size relative to the image.
    pub size: f32,
    /// 0..100: mix of fine, irregular grain.
    pub roughness: f32,
}

impl Default for Grain {
    fn default() -> Self {
        Self {
            amount: 0.0,
            size: 25.0,
            roughness: 50.0,
        }
    }
}

/// Current rendering algorithm generation.
pub const PROCESS_VERSION: u32 = 1;

/// All settings, with defaults that produce a neutral, pleasing render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Rendering algorithm generation these settings were made with. Lets future algorithm changes keep
    /// existing edits looking the same. Current: [`PROCESS_VERSION`].
    pub process_version: u32,
    pub white_balance: WhiteBalance,
    /// Exposure in stops (EV), applied in scene-linear light. Range -5..5.
    pub exposure: f32,
    /// Global contrast around mid-grey. Range -100..100.
    pub contrast: f32,
    /// Brighten (+) or darken (-) bright regions while keeping local detail. Range -100..100.
    pub highlights: f32,
    /// Brighten (+) or darken (-) dark regions while keeping local detail. Range -100..100.
    pub shadows: f32,
    /// Move the white point: + clips earlier (brighter), - holds more highlight range. Range -100..100.
    pub whites: f32,
    /// Move the black point: - crushes, + lifts. Range -100..100.
    pub blacks: f32,
    /// Point curve on display-encoded values, as (input, output) pairs on 0..255. Empty = identity.
    pub tone_curve: Vec<[f32; 2]>,
    /// Fine detail: + enhances, - smooths. -100..100.
    pub texture: f32,
    /// Midtone local contrast: + adds punch, - softens. -100..100.
    pub clarity: f32,
    /// Remove (+) or add (-) atmospheric haze. -100..100.
    pub dehaze: f32,
    /// Boost (+) or mute (-) colour, mostly in less saturated areas and sparing skin. -100..100.
    pub vibrance: f32,
    /// Colour intensity of everything equally. -100..100 (-100 = greyscale).
    pub saturation: f32,
    pub hsl: Hsl,
    pub treatment: Treatment,
    /// Black-and-white mix: how light each hue band becomes in monochrome. -100..100 each.
    pub bw_mix: [f32; HUE_BANDS],
    pub grading: Grading,
    pub sharpening: Sharpening,
    pub noise_reduction: NoiseReduction,
    pub vignette: Vignette,
    pub grain: Grain,
    pub demosaic: Demosaic,
    /// Apply the camera orientation (rotate/flip) to the output.
    pub apply_orientation: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            process_version: PROCESS_VERSION,
            white_balance: WhiteBalance::AsShot,
            exposure: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            tone_curve: Vec::new(),
            texture: 0.0,
            clarity: 0.0,
            dehaze: 0.0,
            vibrance: 0.0,
            saturation: 0.0,
            hsl: Hsl::default(),
            treatment: Treatment::Color,
            bw_mix: [0.0; HUE_BANDS],
            grading: Grading::default(),
            sharpening: Sharpening::default(),
            noise_reduction: NoiseReduction::default(),
            vignette: Vignette::default(),
            grain: Grain::default(),
            demosaic: Demosaic::Auto,
            apply_orientation: true,
        }
    }
}

impl Settings {
    /// Clamp all values into their documented ranges.
    pub fn clamped(mut self) -> Self {
        self.exposure = self.exposure.clamp(-5.0, 5.0);
        let g = &mut self.grading;
        let mut pm100: Vec<&mut f32> = vec![
            &mut self.contrast,
            &mut self.highlights,
            &mut self.shadows,
            &mut self.whites,
            &mut self.blacks,
            &mut self.texture,
            &mut self.clarity,
            &mut self.dehaze,
            &mut self.vibrance,
            &mut self.saturation,
            &mut g.balance,
        ];
        pm100.extend(self.hsl.hue.iter_mut());
        pm100.extend(self.hsl.saturation.iter_mut());
        pm100.extend(self.hsl.luminance.iter_mut());
        pm100.extend(self.bw_mix.iter_mut());
        for v in pm100 {
            *v = v.clamp(-100.0, 100.0);
        }
        g.blending = g.blending.clamp(0.0, 100.0);
        let sh = &mut self.sharpening;
        sh.amount = sh.amount.clamp(0.0, 150.0);
        sh.radius = sh.radius.clamp(0.5, 3.0);
        sh.detail = sh.detail.clamp(0.0, 100.0);
        sh.masking = sh.masking.clamp(0.0, 100.0);
        let v = &mut self.vignette;
        v.amount = v.amount.clamp(-100.0, 100.0);
        v.roundness = v.roundness.clamp(-100.0, 100.0);
        for x in [&mut v.midpoint, &mut v.feather, &mut v.highlights] {
            *x = x.clamp(0.0, 100.0);
        }
        let gr = &mut self.grain;
        for x in [&mut gr.amount, &mut gr.size, &mut gr.roughness] {
            *x = x.clamp(0.0, 100.0);
        }
        let n = &mut self.noise_reduction;
        for v in [&mut n.luminance, &mut n.detail, &mut n.color] {
            *v = v.clamp(0.0, 100.0);
        }
        for w in [
            &mut g.shadows,
            &mut g.midtones,
            &mut g.highlights,
            &mut g.global,
        ] {
            w.hue = w.hue.rem_euclid(360.0);
            w.saturation = w.saturation.clamp(0.0, 100.0);
            w.luminance = w.luminance.clamp(-100.0, 100.0);
        }
        if let WhiteBalance::Custom { temperature, tint } = &mut self.white_balance {
            *temperature = temperature.clamp(pe_colour::cct::T_MIN, pe_colour::cct::T_MAX);
            *tint = tint.clamp(-150.0, 150.0);
        }
        for p in &mut self.tone_curve {
            p[0] = p[0].clamp(0.0, 255.0);
            p[1] = p[1].clamp(0.0, 255.0);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_and_defaults() {
        let s: Settings = serde_json::from_str(r#"{"exposure": 0.5, "whiteBalance": {"mode": "custom", "temperature": 5200, "tint": 8}}"#).unwrap();
        assert_eq!(s.exposure, 0.5);
        assert_eq!(s.contrast, 0.0);
        assert_eq!(
            s.white_balance,
            WhiteBalance::Custom {
                temperature: 5200.0,
                tint: 8.0
            }
        );
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
