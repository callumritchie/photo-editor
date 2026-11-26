//! Export: render each photo with its develop settings, resize in linear light, apply output
//! sharpening, convert to the chosen colour space, and write JPEG/PNG/TIFF (or copy the original) with
//! an embedded ICC profile and optional EXIF metadata.

pub mod encode;
pub mod exif;
pub mod naming;
pub mod process;
pub mod xmp;

use std::path::{Path, PathBuf};

use pe_colour::{ColorSpace, REC2020_TO_XYZ_D65};
use pe_pipeline::{RgbImage, Settings};
use serde::{Deserialize, Serialize};

pub use encode::TiffCompression;
pub use exif::{MetadataMode, PhotoMeta};
pub use naming::Collision;
pub use process::{Medium, Resize, Sharpen, SharpenAmount};

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode: {0}")]
    Decode(#[from] pe_raw::RawError),
    #[error("render: {0}")]
    Render(#[from] pe_pipeline::RenderError),
    #[error("image: {0}")]
    Image(#[from] image::ImageError),
    #[error("tiff: {0}")]
    Tiff(#[from] tiff::TiffError),
    #[error("internal: {0}")]
    Internal(&'static str),
}

/// Output file format.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Format {
    /// `quality` 1..=100.
    Jpeg { quality: u8 },
    #[serde(rename_all = "camelCase")]
    Png { sixteen_bit: bool },
    #[serde(rename_all = "camelCase")]
    Tiff {
        sixteen_bit: bool,
        compression: TiffCompression,
    },
    /// Copy the original file unchanged (renamed per the naming template).
    Original,
}

impl Default for Format {
    fn default() -> Self {
        Format::Jpeg { quality: 90 }
    }
}

/// Everything that controls an export. Serialisable, so it doubles as an export preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExportSettings {
    pub destination: PathBuf,
    /// Optional subfolder of `destination`.
    pub subfolder: Option<String>,
    /// File name template without extension; see [`naming`].
    pub file_naming: String,
    pub start_number: u32,
    pub collision: Collision,
    pub format: Format,
    pub color_space: ColorSpace,
    pub resize: Resize,
    pub dont_enlarge: bool,
    pub sharpen: Option<Sharpen>,
    /// Resolution recorded in the file (and used for print sharpening).
    pub ppi: u32,
    pub metadata: MetadataMode,
    pub artist: Option<String>,
    pub copyright: Option<String>,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            destination: PathBuf::from("."),
            subfolder: None,
            file_naming: "{name}".into(),
            start_number: 1,
            collision: Collision::Rename,
            format: Format::default(),
            color_space: ColorSpace::Srgb,
            resize: Resize::Original,
            dont_enlarge: true,
            sharpen: Some(Sharpen {
                amount: SharpenAmount::Standard,
                medium: Medium::Screen,
            }),
            ppi: 240,
            metadata: MetadataMode::All,
            artist: None,
            copyright: None,
        }
    }
}

/// One photo to export.
#[derive(Debug, Clone)]
pub struct ExportItem {
    pub source: PathBuf,
    pub settings: Settings,
    pub meta: PhotoMeta,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportProgress {
    pub done: usize,
    pub total: usize,
}

fn is_raw(p: &Path) -> bool {
    matches!(
        pe_raw::meta::file_kind(p),
        Some(pe_raw::meta::FileKind::Raw)
    )
}

/// Display-linear Rec.2020 image for a source file. Raws go through the full pipeline with their
/// settings; rendered files (JPEG) are taken as sRGB as-is.
pub fn display_image(item: &ExportItem) -> Result<RgbImage, ExportError> {
    if is_raw(&item.source) {
        let frame = pe_raw::decode_file(&item.source)?;
        return Ok(pe_pipeline::render_display(&frame, &item.settings)?);
    }
    let rgb = pe_raw::meta::decode_jpeg(&item.source)?;
    // sRGB linear -> Rec.2020 linear.
    let to_work = ColorSpace::Srgb.from_space(&REC2020_TO_XYZ_D65);
    let to_work = pe_colour::invert(&to_work).ok_or(ExportError::Internal("matrix"))?;
    let data = rgb
        .data
        .chunks(3)
        .flat_map(|p| {
            let lin = [0, 1, 2].map(|c| pe_colour::srgb_decode(p[c] as f32 / 255.0));
            pe_colour::apply(&to_work, lin)
        })
        .collect();
    Ok(RgbImage {
        width: rgb.width as usize,
        height: rgb.height as usize,
        data,
    })
}

/// Render and encode one item to bytes (not for [`Format::Original`]). Returns (bytes, extension).
pub fn render_file(
    item: &ExportItem,
    s: &ExportSettings,
) -> Result<(Vec<u8>, &'static str), ExportError> {
    let mut img = display_image(item)?;
    let (w, h) = process::target_size(
        img.width as u32,
        img.height as u32,
        s.resize,
        s.dont_enlarge,
    );
    img = process::resize(&img, w, h);
    if let Some(sh) = s.sharpen {
        process::sharpen(&mut img, sh, s.ppi, pe_colour::REC2020_LUMA);
    }
    let out = pe_pipeline::encode_output(&img, s.color_space);
    let enc = encode::Encoded {
        width: out.width as u32,
        height: out.height as u32,
        data: &out.data,
    };
    let icc = pe_colour::icc::profile(s.color_space);
    let exif = exif::build(
        &item.meta,
        s.metadata,
        s.artist.as_deref(),
        s.copyright.as_deref(),
        s.ppi,
        s.color_space == ColorSpace::Srgb,
    );
    let xmp = xmp::build(
        &item.meta.keywords,
        s.metadata,
        s.artist.as_deref(),
        s.copyright.as_deref(),
    );
    Ok(match s.format {
        Format::Jpeg { quality } => (
            encode::jpeg(&enc, quality, &icc, exif.as_deref(), xmp.as_deref())?,
            "jpg",
        ),
        Format::Png { sixteen_bit } => (
            encode::png(&enc, sixteen_bit, &icc, exif.as_deref(), xmp.as_deref())?,
            "png",
        ),
        Format::Tiff {
            sixteen_bit,
            compression,
        } => {
            let all = s.metadata == MetadataMode::All;
            let none = s.metadata == MetadataMode::None;
            let tags = encode::TiffTags {
                make: item.meta.make.as_deref().filter(|_| all),
                model: item.meta.model.as_deref().filter(|_| all),
                date_time: item
                    .meta
                    .capture_time
                    .as_ref()
                    .filter(|_| all)
                    .map(|t| t.replacen('-', ":", 2).replace('T', " ")),
                artist: s.artist.as_deref().filter(|_| !none),
                copyright: s.copyright.as_deref().filter(|_| !none),
                ppi: s.ppi,
                xmp: xmp.as_deref(),
            };
            (
                encode::tiff(&enc, sixteen_bit, compression, &icc, &tags)?,
                "tif",
            )
        }
        Format::Original => return Err(ExportError::Internal("original is copied, not rendered")),
    })
}

/// Export all items. Photos are processed one at a time (each render is itself parallel), which keeps
/// memory bounded for large raws.
pub fn export(
    items: &[ExportItem],
    s: &ExportSettings,
    progress: &mut dyn FnMut(&ExportProgress),
) -> ExportReport {
    let mut report = ExportReport::default();
    let dir = match &s.subfolder {
        Some(sub) if !sub.trim().is_empty() => s.destination.join(naming::sanitize(sub)),
        _ => s.destination.clone(),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        report
            .failed
            .push((dir.display().to_string(), e.to_string()));
        return report;
    }
    for (i, item) in items.iter().enumerate() {
        let src = item.source.display().to_string();
        let seq = s.start_number as usize + i;
        let stem = naming::expand(&s.file_naming, &item.source, &item.meta, seq);
        let result = (|| -> Result<Option<PathBuf>, ExportError> {
            if s.format == Format::Original {
                let ext = item
                    .source
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let Some(target) = naming::resolve(&dir, &stem, &ext, s.collision) else {
                    return Ok(None);
                };
                naming::write_atomic(&target, &std::fs::read(&item.source)?)?;
                return Ok(Some(target));
            }
            let (bytes, ext) = render_file(item, s)?;
            let Some(target) = naming::resolve(&dir, &stem, ext, s.collision) else {
                return Ok(None);
            };
            naming::write_atomic(&target, &bytes)?;
            Ok(Some(target))
        })();
        match result {
            Ok(Some(p)) => report.written.push(p.display().to_string()),
            Ok(None) => report.skipped.push(src),
            Err(e) => report.failed.push((src, e.to_string())),
        }
        progress(&ExportProgress {
            done: i + 1,
            total: items.len(),
        });
    }
    report
}

/// Short description of an export setup for UIs, e.g. "JPEG q90, sRGB".
pub fn describe(s: &ExportSettings) -> String {
    let fmt = match s.format {
        Format::Jpeg { quality } => format!("JPEG q{quality}"),
        Format::Png { sixteen_bit } => format!("PNG {}-bit", if sixteen_bit { 16 } else { 8 }),
        Format::Tiff { sixteen_bit, .. } => {
            format!("TIFF {}-bit", if sixteen_bit { 16 } else { 8 })
        }
        Format::Original => "Original".into(),
    };
    format!("{fmt}, {}", s.color_space.name())
}
