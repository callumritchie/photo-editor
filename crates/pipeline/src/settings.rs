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
            demosaic: Demosaic::Auto,
            apply_orientation: true,
        }
    }
}

impl Settings {
    /// Clamp all values into their documented ranges.
    pub fn clamped(mut self) -> Self {
        self.exposure = self.exposure.clamp(-5.0, 5.0);
        for v in [
            &mut self.contrast,
            &mut self.highlights,
            &mut self.shadows,
            &mut self.whites,
            &mut self.blacks,
        ] {
            *v = v.clamp(-100.0, 100.0);
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
