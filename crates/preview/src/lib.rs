//! Previews: thumbnail generation and the thumbnail store.
//!
//! M1 provides grid thumbnails built from the camera's embedded preview (fast, no raw decode),
//! stored as JPEG blobs in a small SQLite database next to the catalog. Rendered preview pyramids
//! keyed by settings digest arrive in M4.

use std::path::Path;

use image::{imageops::FilterType, RgbImage};
use pe_raw::meta::{self, FileKind, Rgb8};
use rusqlite::{params, Connection, OptionalExtension};

/// Long edge of grid thumbnails, in pixels.
pub const THUMB_LONG_EDGE: u32 = 512;

#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error("decode: {0}")]
    Decode(#[from] pe_raw::RawError),
    #[error("no embedded preview")]
    NoPreview,
    #[error("image: {0}")]
    Image(#[from] image::ImageError),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("render: {0}")]
    Render(String),
}

/// Rotate/flip an image according to EXIF orientation 1..=8.
pub fn orient(img: RgbImage, orientation: u8) -> RgbImage {
    use image::imageops::{flip_horizontal, flip_vertical, rotate180, rotate270, rotate90};
    match orientation {
        2 => flip_horizontal(&img),
        3 => rotate180(&img),
        4 => flip_vertical(&img),
        5 => flip_horizontal(&rotate90(&img)),
        6 => rotate90(&img),
        7 => flip_horizontal(&rotate270(&img)),
        8 => rotate270(&img),
        _ => img,
    }
}

/// A quick display image for a file, oriented for display, with a long edge of at least `min_long_edge`
/// where the source allows. Raws use their embedded preview; if a raw has none, it is rendered with the
/// pipeline (fast demosaic, default settings).
pub fn quick_image(
    path: &Path,
    orientation: u8,
    min_long_edge: u32,
) -> Result<RgbImage, PreviewError> {
    match meta::file_kind(path) {
        Some(FileKind::Raw) => match meta::raw_embedded_preview(path, min_long_edge)? {
            Some(rgb) => Ok(orient(to_image(rgb)?, orientation)),
            None => render_fallback(path),
        },
        _ => Ok(orient(to_image(meta::decode_jpeg(path)?)?, orientation)),
    }
}

fn to_image(rgb: Rgb8) -> Result<RgbImage, PreviewError> {
    RgbImage::from_raw(rgb.width, rgb.height, rgb.data).ok_or(PreviewError::NoPreview)
}

/// Draft pipeline render (reduced resolution, default settings) as 8-bit sRGB, oriented.
pub fn render_fallback(path: &Path) -> Result<RgbImage, PreviewError> {
    let frame = pe_raw::decode_file(path)?;
    let settings = pe_pipeline::Settings {
        demosaic: pe_pipeline::Demosaic::Fast,
        ..Default::default()
    };
    let out = pe_pipeline::render_draft(&frame, &settings)
        .map_err(|e| PreviewError::Render(e.to_string()))?;
    let data = out.data.iter().map(|v| (v * 255.0 + 0.5) as u8).collect();
    RgbImage::from_raw(out.width as u32, out.height as u32, data).ok_or(PreviewError::NoPreview)
}

/// Downscale (never upscale) so the long edge is at most `long_edge`.
pub fn fit(img: &RgbImage, long_edge: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    let s = long_edge as f32 / w.max(h) as f32;
    if s >= 1.0 {
        return img.clone();
    }
    let (nw, nh) = (
        ((w as f32 * s).round() as u32).max(1),
        ((h as f32 * s).round() as u32).max(1),
    );
    if s < 0.25 {
        // Large reductions: fast box-sampling, which also averages away aliasing.
        image::imageops::thumbnail(img, nw, nh)
    } else {
        image::imageops::resize(img, nw, nh, FilterType::Triangle)
    }
}

/// Encode RGB as baseline JPEG.
pub fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>, PreviewError> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(img)?;
    Ok(out)
}

/// Grid thumbnail JPEG for a file.
pub fn make_thumbnail(path: &Path, orientation: u8) -> Result<(u32, u32, Vec<u8>), PreviewError> {
    let t = fit(
        &quick_image(path, orientation, THUMB_LONG_EDGE)?,
        THUMB_LONG_EDGE,
    );
    Ok((t.width(), t.height(), encode_jpeg(&t, 82)?))
}

/// SQLite store of thumbnails keyed by image id.
pub struct ThumbStore {
    conn: Connection,
}

impl ThumbStore {
    pub fn open(path: &Path) -> Result<Self, PreviewError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS thumbs (
                 image_id INTEGER PRIMARY KEY,
                 width INTEGER NOT NULL,
                 height INTEGER NOT NULL,
                 jpeg BLOB NOT NULL
             );",
        )?;
        Ok(Self { conn })
    }

    pub fn put(
        &self,
        image_id: i64,
        width: u32,
        height: u32,
        jpeg: &[u8],
    ) -> Result<(), PreviewError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO thumbs (image_id, width, height, jpeg) VALUES (?1, ?2, ?3, ?4)",
            params![image_id, width, height, jpeg],
        )?;
        Ok(())
    }

    pub fn get(&self, image_id: i64) -> Result<Option<Vec<u8>>, PreviewError> {
        Ok(self
            .conn
            .query_row(
                "SELECT jpeg FROM thumbs WHERE image_id = ?1",
                [image_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn has(&self, image_id: i64) -> Result<bool, PreviewError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM thumbs WHERE image_id = ?1",
                [image_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn remove(&self, image_id: i64) -> Result<(), PreviewError> {
        self.conn
            .execute("DELETE FROM thumbs WHERE image_id = ?1", [image_id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_and_orient() {
        let img = RgbImage::new(400, 200);
        assert_eq!(fit(&img, 100).dimensions(), (100, 50));
        assert_eq!(fit(&img, 1000).dimensions(), (400, 200));
        assert_eq!(orient(img.clone(), 6).dimensions(), (200, 400));
        assert_eq!(orient(img, 3).dimensions(), (400, 200));
    }

    #[test]
    fn store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = ThumbStore::open(&dir.path().join("t.sqlite")).unwrap();
        assert!(s.get(1).unwrap().is_none());
        s.put(1, 2, 3, b"abc").unwrap();
        assert_eq!(s.get(1).unwrap().as_deref(), Some(&b"abc"[..]));
        assert!(s.has(1).unwrap());
        s.remove(1).unwrap();
        assert!(!s.has(1).unwrap());
    }
}
