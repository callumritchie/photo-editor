//! Render pipeline (CPU reference implementation).
//!
//! M0 scope: demosaic -> white balance -> camera-to-sRGB -> exposure -> output encoding.
//! Later milestones add tone mapping, local adjustments, detail and colour stages, and a GPU twin
//! of every stage verified against this CPU path.

pub mod demosaic;

use pe_raw::{Layout, RawFrame};
use rayon::prelude::*;

/// Develop settings understood by the M0 pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Exposure adjustment in stops (EV).
    pub exposure_ev: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self { exposure_ev: 0.0 }
    }
}

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

/// Render a raw frame to scene-linear sRGB (values may exceed 1.0 before output clipping).
pub fn render_linear(frame: &RawFrame, settings: &Settings) -> Result<RgbImage, RenderError> {
    let cam_rgb = match &frame.layout {
        Layout::Mosaic(cfa) => {
            demosaic::neighbourhood_average(&frame.data, frame.width, frame.height, cfa)
        }
        Layout::LinearRgb => frame.data.clone(),
    };
    let to_srgb =
        pe_colour::camera_to_srgb(&frame.xyz_to_cam).ok_or(RenderError::SingularMatrix)?;
    let gain = 2f32.powf(settings.exposure_ev);
    let wb = frame.wb;
    // Simple highlight handling (M0): after white balance, clip every channel at the level where the
    // least-amplified channel saturates, so blown highlights stay neutral instead of turning magenta.
    // M2 replaces this with proper highlight reconstruction.
    let clip = wb[0].min(wb[1]).min(wb[2]);

    let mut data = cam_rgb;
    data.par_chunks_mut(3).for_each(|px| {
        let c = [
            (px[0] * wb[0]).clamp(0.0, clip),
            (px[1] * wb[1]).clamp(0.0, clip),
            (px[2] * wb[2]).clamp(0.0, clip),
        ];
        let rgb = pe_colour::apply(&to_srgb, c);
        px[0] = rgb[0] * gain;
        px[1] = rgb[1] * gain;
        px[2] = rgb[2] * gain;
    });
    Ok(RgbImage {
        width: frame.width,
        height: frame.height,
        data,
    })
}

/// Encode a linear sRGB image to 16-bit sRGB code values (clipped to [0, 1]).
pub fn encode_srgb16(img: &RgbImage) -> Vec<u16> {
    img.data
        .par_iter()
        .map(|&v| (pe_colour::srgb_encode(v.clamp(0.0, 1.0)) * 65535.0 + 0.5) as u16)
        .collect()
}
