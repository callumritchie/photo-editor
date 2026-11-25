//! Fast metadata and embedded-preview extraction (no full raw decode).

use std::path::Path;

use crate::RawError;

/// File extensions treated as camera raw files.
pub const RAW_EXTENSIONS: &[&str] = &[
    "3fr", "arw", "cr2", "cr3", "crw", "dcr", "dng", "erf", "iiq", "kdc", "mef", "mos", "mrw",
    "nef", "nrw", "orf", "ori", "pef", "raf", "raw", "rw2", "rwl", "sr2", "srf", "srw", "x3f",
];
/// Rendered formats we can catalog and thumbnail.
pub const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg"];

/// Kind of a supported file, by extension (case-insensitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Raw,
    Jpeg,
}

pub fn file_kind(path: &Path) -> Option<FileKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if RAW_EXTENSIONS.contains(&ext.as_str()) {
        Some(FileKind::Raw)
    } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        Some(FileKind::Jpeg)
    } else {
        None
    }
}

/// Descriptive metadata harvested at import.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    /// Local capture time as "YYYY-MM-DDTHH:MM:SS" (no zone).
    pub capture_time: Option<String>,
    /// UTC offset of the capture time, e.g. "+01:00", when recorded.
    pub capture_offset: Option<String>,
    pub iso: Option<u32>,
    /// Seconds.
    pub exposure_time: Option<f64>,
    pub f_number: Option<f64>,
    /// Millimetres.
    pub focal_length: Option<f64>,
    /// EXIF orientation 1..=8.
    pub orientation: u8,
}

/// An 8-bit RGB image.
#[derive(Debug, Clone)]
pub struct Rgb8 {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Convert an EXIF date "YYYY:MM:DD HH:MM:SS" to "YYYY-MM-DDTHH:MM:SS".
pub fn exif_date_to_iso(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches('\0');
    let b = s.as_bytes();
    if b.len() < 19 || !b[..4].iter().all(u8::is_ascii_digit) || s.starts_with("0000") {
        return None;
    }
    Some(format!(
        "{}-{}-{}T{}",
        &s[0..4],
        &s[5..7],
        &s[8..10],
        &s[11..19]
    ))
}

/// Read descriptive metadata from a raw file.
pub fn raw_metadata(path: &Path) -> Result<Metadata, RawError> {
    let src =
        rawler::rawsource::RawSource::new(path).map_err(|e| RawError::Decoder(e.to_string()))?;
    let dec = rawler::get_decoder(&src).map_err(|e| RawError::Decoder(e.to_string()))?;
    let md = dec
        .raw_metadata(&src, &rawler::decoders::RawDecodeParams::default())
        .map_err(|e| RawError::Decoder(e.to_string()))?;
    let ex = &md.exif;
    let r = |v: &Option<rawler::formats::tiff::Rational>| {
        v.as_ref().map(|r| r.n as f64 / r.d.max(1) as f64)
    };
    let lens = md
        .lens
        .as_ref()
        .map(|l| {
            format!("{} {}", l.lens_make, l.lens_model)
                .trim()
                .to_string()
        })
        .or_else(|| ex.lens_model.clone())
        .filter(|s| !s.is_empty());
    Ok(Metadata {
        make: Some(md.make.clone()).filter(|s| !s.is_empty()),
        model: Some(md.model.clone()).filter(|s| !s.is_empty()),
        lens,
        capture_time: ex
            .date_time_original
            .as_deref()
            .or(ex.create_date.as_deref())
            .and_then(exif_date_to_iso),
        capture_offset: ex
            .offset_time_original
            .clone()
            .filter(|s| !s.trim().is_empty()),
        iso: ex.iso_speed_ratings.map(u32::from).or(ex.iso_speed),
        exposure_time: r(&ex.exposure_time),
        f_number: r(&ex.fnumber),
        focal_length: r(&ex.focal_length),
        orientation: ex
            .orientation
            .map(|o| o as u8)
            .filter(|o| (1..=8).contains(o))
            .unwrap_or(1),
    })
}

/// A preview image embedded in a raw file (not rotated): the first of thumbnail, preview and full-size
/// embedded images whose long edge reaches `min_long_edge`, else the largest available. `None` if the
/// file has no extractable preview.
pub fn raw_embedded_preview(path: &Path, min_long_edge: u32) -> Result<Option<Rgb8>, RawError> {
    let src =
        rawler::rawsource::RawSource::new(path).map_err(|e| RawError::Decoder(e.to_string()))?;
    let dec = rawler::get_decoder(&src).map_err(|e| RawError::Decoder(e.to_string()))?;
    let params = rawler::decoders::RawDecodeParams::default();
    let mut best: Option<image::DynamicImage> = None;
    for which in 0..3 {
        let img = match which {
            0 => dec.thumbnail_image(&src, &params),
            1 => dec.preview_image(&src, &params),
            _ => dec.full_image(&src, &params),
        };
        if let Some(img) = img.ok().flatten() {
            let long = img.width().max(img.height());
            let bigger = best
                .as_ref()
                .is_none_or(|b| long > b.width().max(b.height()));
            if bigger {
                best = Some(img);
            }
            if long >= min_long_edge {
                break;
            }
        }
    }
    Ok(best.map(|img| {
        let rgb = img.to_rgb8();
        Rgb8 {
            width: rgb.width(),
            height: rgb.height(),
            data: rgb.into_raw(),
        }
    }))
}

/// Decode a JPEG file to RGB (not rotated).
pub fn decode_jpeg(path: &Path) -> Result<Rgb8, RawError> {
    let img = image::ImageReader::open(path)
        .map_err(|e| RawError::Decoder(e.to_string()))?
        .with_guessed_format()
        .map_err(|e| RawError::Decoder(e.to_string()))?
        .decode()
        .map_err(|e| RawError::Decoder(e.to_string()))?
        .to_rgb8();
    Ok(Rgb8 {
        width: img.width(),
        height: img.height(),
        data: img.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_dates() {
        assert_eq!(
            exif_date_to_iso("2021:06:26 22:17:11").as_deref(),
            Some("2021-06-26T22:17:11")
        );
        assert_eq!(exif_date_to_iso("0000:00:00 00:00:00"), None);
        assert_eq!(exif_date_to_iso("garbage"), None);
    }

    #[test]
    fn kinds() {
        assert_eq!(file_kind(Path::new("a/B.NEF")), Some(FileKind::Raw));
        assert_eq!(file_kind(Path::new("x.jpeg")), Some(FileKind::Jpeg));
        assert_eq!(file_kind(Path::new("x.txt")), None);
        assert_eq!(file_kind(Path::new("noext")), None);
    }
}
