//! Raw file decoding into a normalised, decoder-independent [`RawFrame`].
//!
//! The rest of the codebase only sees `RawFrame`; the decoder backend (currently the
//! LGPL-2.1 `rawler` crate) is an implementation detail behind [`decode_file`].

pub mod dng;
pub mod meta;

use std::path::Path;

/// Errors from raw decoding.
#[derive(Debug, thiserror::Error)]
pub enum RawError {
    #[error("decoder error: {0}")]
    Decoder(String),
    #[error("unsupported raw layout: {0}")]
    Unsupported(String),
}

/// Colour filter array pattern, `width` x `height`, row-major list of colour indices
/// (0 = red, 1 = green, 2 = blue).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cfa {
    pub width: usize,
    pub height: usize,
    pub colors: Vec<u8>,
}

impl Cfa {
    /// Colour index at image position (`row`, `col`).
    #[inline]
    pub fn color_at(&self, row: usize, col: usize) -> u8 {
        self.colors[(row % self.height) * self.width + (col % self.width)]
    }

    /// The pattern as seen from a crop origin at (`x`, `y`).
    pub fn shifted(&self, x: usize, y: usize) -> Cfa {
        let mut colors = Vec::with_capacity(self.colors.len());
        for r in 0..self.height {
            for c in 0..self.width {
                colors.push(self.color_at(r + y, c + x));
            }
        }
        Cfa {
            width: self.width,
            height: self.height,
            colors,
        }
    }

    /// Standard 2x2 Bayer patterns by name (e.g. "RGGB").
    pub fn bayer(name: &str) -> Option<Cfa> {
        let colors: Option<Vec<u8>> = name
            .chars()
            .map(|ch| match ch {
                'R' => Some(0),
                'G' => Some(1),
                'B' => Some(2),
                _ => None,
            })
            .collect();
        let colors = colors?;
        (colors.len() == 4).then_some(Cfa {
            width: 2,
            height: 2,
            colors,
        })
    }
}

/// Sensor data layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    /// One sample per pixel behind a colour filter array.
    Mosaic(Cfa),
    /// Three samples per pixel (already demosaiced, e.g. linear DNG), interleaved RGB.
    LinearRgb,
}

/// A decoded raw image, black-subtracted and scaled so that sensor saturation is 1.0.
/// Values are camera-native colour and scene-linear. Values below 0 (noise around black) are kept.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub make: String,
    pub model: String,
    /// Dimensions after cropping to the camera's recommended area.
    pub width: usize,
    pub height: usize,
    pub layout: Layout,
    /// `width * height` samples (Mosaic) or `width * height * 3` (LinearRgb).
    pub data: Vec<f32>,
    /// As-shot white-balance multipliers in camera space, normalised so green = 1.
    pub wb: [f32; 3],
    /// XYZ (D65) to camera-native matrix, 3x3, as published for the camera model.
    pub xyz_to_cam: [[f32; 3]; 3],
    /// EXIF orientation code 1..=8 (1 = normal). Not yet applied to `data`.
    pub orientation: u8,
    /// Sensor pixels per frame pixel along each axis: 1 for a decoded raw, more for reduced
    /// previews. Pixel-scale effects (sharpening radius) use it to look alike at any size.
    pub scale: f32,
    /// Lens and shooting parameters, when recorded (for lens profiles).
    pub lens: Option<LensInfo>,
    /// An already-rendered (display-referred) picture such as a JPEG, decoded to linear light.
    /// Rendering then skips the filmic display transform so untouched pictures look as they were.
    pub rendered: bool,
    /// Stops added to the rendering (DNG BaselineExposure); 0 otherwise.
    pub exposure_bias: f32,
    /// A DNG's own lens distortion / chromatic aberration correction (WarpRectilinear).
    pub dng_warp: Option<dng::WarpModel>,
    /// The file carries its own lens corrections (DNG opcodes), so database profiles do not apply.
    pub own_lens_corrections: bool,
}

/// The lens a photo was taken with.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LensInfo {
    pub name: String,
    /// Millimetres; 0 when unknown.
    pub focal_length: f32,
    /// f-number; 0 when unknown.
    pub aperture: f32,
}

impl RawFrame {
    /// A copy of everything except the pixel data (which is left empty).
    pub fn clone_header(&self) -> RawFrame {
        RawFrame {
            make: self.make.clone(),
            model: self.model.clone(),
            width: self.width,
            height: self.height,
            layout: self.layout.clone(),
            data: Vec::new(),
            wb: self.wb,
            xyz_to_cam: self.xyz_to_cam,
            orientation: self.orientation,
            scale: self.scale,
            lens: self.lens.clone(),
            rendered: self.rendered,
            exposure_bias: self.exposure_bias,
            dng_warp: self.dng_warp,
            own_lens_corrections: self.own_lens_corrections,
        }
    }
}

/// Decode a raw file (or a JPEG, see [`decode_rendered`]) from disk.
pub fn decode_file(path: impl AsRef<Path>) -> Result<RawFrame, RawError> {
    if meta::file_kind(path.as_ref()) == Some(meta::FileKind::Jpeg) {
        return decode_rendered(path.as_ref());
    }
    let img = rawler::decode_file(path.as_ref()).map_err(|e| RawError::Decoder(e.to_string()))?;
    let (mut frame, stage) = from_rawler_staged(img)?;
    let is_dng = path
        .as_ref()
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("dng"));
    if is_dng {
        if let Some(x) = dng::read_extras_file(path.as_ref()) {
            apply_dng_extras(&mut frame, &x, stage);
        }
    }
    frame.lens = meta::raw_metadata(path.as_ref()).ok().and_then(|m| {
        Some(LensInfo {
            name: m.lens?,
            focal_length: m.focal_length.unwrap_or(0.0) as f32,
            aperture: m.f_number.unwrap_or(0.0) as f32,
        })
    });
    Ok(frame)
}

/// Trim and collapse runs of spaces in a camera name (some DNGs pad them).
fn tidy(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Apply what a DNG says beyond its pixels: gain opcodes to the samples and the warp as geometry.
/// BaselineExposure is deliberately not applied: native raws get no such offset, so applying it would
/// make a DNG render brighter or darker than the same camera's native raw (`exposure_bias` stays 0).
pub fn apply_dng_extras(frame: &mut RawFrame, x: &dng::DngExtras, stage: dng::Stage) {
    let cpp = if matches!(frame.layout, Layout::LinearRgb) {
        3
    } else {
        1
    };
    dng::apply_gains(x, &mut frame.data, frame.width, frame.height, cpp, stage);
    frame.dng_warp = dng::warp_model(x, frame.width, frame.height, stage);
    frame.own_lens_corrections = x
        .opcodes
        .iter()
        .flatten()
        .any(|o| !matches!(o, dng::Opcode::GainMap(_)))
        || frame.dng_warp.is_some();
}

/// The colour space an embedded ICC profile describes, recognised by its description (sRGB when
/// there is none or it is not recognised).
pub fn space_of_icc(icc: &[u8]) -> pe_colour::ColorSpace {
    use pe_colour::ColorSpace;
    // Descriptions are ASCII (ICC v2 'desc') or UTF-16BE (v4 'mluc').
    let has = |name: &str| {
        let ascii = name.as_bytes();
        let utf16: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        icc.windows(ascii.len())
            .any(|w| w.eq_ignore_ascii_case(ascii))
            || icc.windows(utf16.len()).any(|w| w == utf16.as_slice())
    };
    if has("Display P3") || has("P3 D65") {
        ColorSpace::DisplayP3
    } else if has("Adobe RGB") || has("AdobeRGB") {
        ColorSpace::AdobeRgb
    } else if has("ProPhoto") || has("ROMM") {
        ColorSpace::ProPhoto
    } else {
        ColorSpace::Srgb
    }
}

/// Decode an already-rendered picture (JPEG) to linear light in its own colour space, as a frame
/// with neutral white balance and [`RawFrame::rendered`] set.
pub fn decode_rendered(path: &Path) -> Result<RawFrame, RawError> {
    use image::ImageDecoder;
    let err = |e: &dyn std::fmt::Display| RawError::Decoder(e.to_string());
    let file = std::fs::File::open(path).map_err(|e| err(&e))?;
    let mut dec = image::codecs::jpeg::JpegDecoder::new(std::io::BufReader::new(file))
        .map_err(|e| err(&e))?;
    let icc = dec.icc_profile().ok().flatten();
    let img = image::DynamicImage::from_decoder(dec)
        .map_err(|e| err(&e))?
        .to_rgb8();
    let space = icc
        .as_deref()
        .map(space_of_icc)
        .unwrap_or(pe_colour::ColorSpace::Srgb);
    let lut: Vec<f32> = (0..256).map(|v| space.decode(v as f32 / 255.0)).collect();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let data = img
        .into_raw()
        .into_iter()
        .map(|v| lut[v as usize])
        .collect();
    let to_xyz = space.to_xyz_d65();
    let xyz_to_cam = pe_colour::invert(&to_xyz)
        .ok_or_else(|| RawError::Unsupported("colour space matrix".into()))?;
    let m = meta::image_metadata(path);
    Ok(RawFrame {
        make: m.make.clone().unwrap_or_default(),
        model: m.model.clone().unwrap_or_default(),
        width: w,
        height: h,
        layout: Layout::LinearRgb,
        data,
        wb: [1.0, 1.0, 1.0],
        xyz_to_cam,
        orientation: m.orientation,
        scale: 1.0,
        lens: m.lens.clone().map(|name| LensInfo {
            name,
            focal_length: m.focal_length.unwrap_or(0.0) as f32,
            aperture: m.f_number.unwrap_or(0.0) as f32,
        }),
        rendered: true,
        exposure_bias: 0.0,
        dng_warp: None,
        own_lens_corrections: false,
    })
}

/// [`from_rawler`], also returning where the frame sits within the active area (the DNG stage image).
fn from_rawler_staged(img: rawler::RawImage) -> Result<(RawFrame, dng::Stage), RawError> {
    use rawler::rawimage::RawPhotometricInterpretation as P;

    let (full_w, full_h, cpp) = (img.width, img.height, img.cpp);
    let raw: Vec<f32> = img.data.as_f32().into_owned();

    // Crop: prefer the recommended crop, then the active area, else the full sensor.
    let rect = img.crop_area.or(img.active_area);
    let (cx, cy, cw, ch) = match rect {
        Some(r) => (r.p.x, r.p.y, r.d.w, r.d.h),
        None => (0, 0, full_w, full_h),
    };
    let (ax, ay, aw, ah) = match img.active_area {
        Some(r) => (r.p.x, r.p.y, r.d.w, r.d.h),
        None => (0, 0, full_w, full_h),
    };
    let stage = dng::Stage {
        x0: cx.saturating_sub(ax),
        y0: cy.saturating_sub(ay),
        width: aw,
        height: ah,
    };
    if cx + cw > full_w || cy + ch > full_h || cw == 0 || ch == 0 {
        return Err(RawError::Unsupported(format!(
            "bad crop {cx},{cy} {cw}x{ch} in {full_w}x{full_h}"
        )));
    }

    // Levels: black is a repeating per-position (and per-channel) pattern; white is per channel.
    let bl = &img.blacklevel;
    let bl_vals = bl.as_vec();
    let (bw, bh, bc) = (bl.width.max(1), bl.height.max(1), bl.cpp.max(1));
    let black_at = |row: usize, col: usize, c: usize| -> f32 {
        let idx = ((row % bh) * bw + (col % bw)) * bc + (c % bc);
        bl_vals.get(idx).copied().unwrap_or(0.0)
    };
    let white_vals: Vec<f32> = img.whitelevel.as_vec();
    let white_at = |c: usize| -> f32 {
        white_vals
            .get(c)
            .or(white_vals.first())
            .copied()
            .unwrap_or(65535.0)
    };

    let layout = match (&img.photometric, cpp) {
        (P::Cfa(cfg), 1) => {
            let pattern = &cfg.cfa;
            let (pw, ph) = (pattern.width, pattern.height);
            if pw == 0 || ph == 0 {
                return Err(RawError::Unsupported("empty CFA pattern".into()));
            }
            let mut colors = Vec::with_capacity(pw * ph);
            for r in 0..ph {
                for c in 0..pw {
                    // Decoder colour indices: 0 = R, 1 = G, 2 = B, 3 = second green -> treat as green.
                    colors.push(match pattern.color_at(r, c) {
                        0 => 0,
                        2 => 2,
                        _ => 1,
                    });
                }
            }
            Layout::Mosaic(
                Cfa {
                    width: pw,
                    height: ph,
                    colors,
                }
                .shifted(cx, cy),
            )
        }
        (_, 3) => Layout::LinearRgb,
        (p, n) => {
            return Err(RawError::Unsupported(format!(
                "photometric {p:?} with {n} samples/pixel"
            )))
        }
    };

    let mut data = Vec::with_capacity(cw * ch * cpp);
    for row in cy..cy + ch {
        for col in cx..cx + cw {
            for c in 0..cpp {
                let v = raw[(row * full_w + col) * cpp + c];
                let b = black_at(row, col, c);
                let range = (white_at(c) - b).max(1.0);
                data.push((v - b) / range);
            }
        }
    }

    let wb_raw = img.wb_coeffs;
    let wb = if wb_raw[..3].iter().all(|v| v.is_finite() && *v > 0.0) {
        [wb_raw[0] / wb_raw[1], 1.0, wb_raw[2] / wb_raw[1]]
    } else {
        let n = img.neutralwb();
        [n[0] / n[1], 1.0, n[2] / n[1]]
    };

    let xyz_to_cam = colour_matrix(&img);
    if xyz_to_cam.iter().flatten().all(|v| *v == 0.0) {
        return Err(RawError::Unsupported("camera has no colour matrix".into()));
    }

    Ok((
        RawFrame {
            make: tidy(&img.clean_make),
            model: tidy(&img.clean_model),
            width: cw,
            height: ch,
            layout,
            data,
            wb,
            xyz_to_cam,
            orientation: orientation_code(&img.orientation),
            scale: 1.0,
            lens: None,
            rendered: false,
            exposure_bias: 0.0,
            dng_warp: None,
            own_lens_corrections: false,
        },
        stage,
    ))
}

/// Pick the XYZ->camera matrix, preferring daylight-like calibration illuminants (closest to the
/// D65 output white), and falling back to the decoder's legacy single matrix.
fn colour_matrix(img: &rawler::RawImage) -> [[f32; 3]; 3] {
    use rawler::imgop::xyz::Illuminant as I;
    let preferred = [
        I::D65,
        I::Daylight,
        I::D55,
        I::D75,
        I::D50,
        I::Flash,
        I::FineWeather,
        I::C,
        I::B,
        I::A,
    ];
    if let Some((_, flat)) = img.color_matrix_find_first(preferred) {
        if flat.len() >= 9 {
            return [
                [flat[0], flat[1], flat[2]],
                [flat[3], flat[4], flat[5]],
                [flat[6], flat[7], flat[8]],
            ];
        }
    }
    let m = img.xyz_to_cam;
    [m[0], m[1], m[2]]
}

fn orientation_code(o: &rawler::Orientation) -> u8 {
    use rawler::Orientation as O;
    match o {
        O::Normal | O::Unknown => 1,
        O::HorizontalFlip => 2,
        O::Rotate180 => 3,
        O::VerticalFlip => 4,
        O::Transpose => 5,
        O::Rotate90 => 6,
        O::Transverse => 7,
        O::Rotate270 => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfa_shift_moves_origin() {
        let rggb = Cfa::bayer("RGGB").unwrap();
        assert_eq!(rggb.shifted(1, 0), Cfa::bayer("GRBG").unwrap());
        assert_eq!(rggb.shifted(0, 1), Cfa::bayer("GBRG").unwrap());
        assert_eq!(rggb.shifted(1, 1), Cfa::bayer("BGGR").unwrap());
    }
}
