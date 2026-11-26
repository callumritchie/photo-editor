//! Previews: thumbnails and loupe previews, and the stores that cache them.
//!
//! Two levels, each a JPEG keyed by what it shows:
//! - grid thumbnails (512 px, `thumbs.sqlite`): the camera's embedded preview for unedited photos
//!   ([`EMBEDDED`]), otherwise a render keyed by the develop-settings digest;
//! - loupe previews (2048 px, `previews.sqlite`): always a render, keyed by the settings digest, with a
//!   size budget and least-recently-used eviction.
//!
//! [`render_previews`] produces both levels from one decode, on the GPU when one is available.

use std::path::Path;

use image::{imageops::FilterType, RgbImage};
use pe_raw::meta::{self, FileKind, Rgb8};
use rusqlite::{params, Connection, OptionalExtension};

/// Digest marking a thumbnail made from the camera's embedded preview (an unedited photo).
pub const EMBEDDED: &str = "embedded";

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

/// Grid thumbnail JPEG rendered with develop settings (for edited photos).
pub fn make_rendered_thumbnail(
    path: &Path,
    settings: &pe_pipeline::Settings,
) -> Result<(u32, u32, Vec<u8>), PreviewError> {
    let frame = pe_raw::decode_file(path)?;
    let out = pe_pipeline::render_draft(&frame, settings)
        .map_err(|e| PreviewError::Render(e.to_string()))?;
    let data = out.data.iter().map(|v| (v * 255.0 + 0.5) as u8).collect();
    let img = RgbImage::from_raw(out.width as u32, out.height as u32, data)
        .ok_or(PreviewError::NoPreview)?;
    let t = fit(&img, THUMB_LONG_EDGE);
    Ok((t.width(), t.height(), encode_jpeg(&t, 82)?))
}

/// SQLite store of thumbnails keyed by image id.
pub struct ThumbStore {
    conn: Connection,
}

impl ThumbStore {
    pub fn open(path: &Path) -> Result<Self, PreviewError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS thumbs (
                 image_id INTEGER PRIMARY KEY,
                 width INTEGER NOT NULL,
                 height INTEGER NOT NULL,
                 jpeg BLOB NOT NULL,
                 digest TEXT NOT NULL DEFAULT 'embedded'
             );",
        )?;
        // Stores created before the digest column existed.
        let has_digest: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('thumbs') WHERE name = 'digest')",
            [],
            |r| r.get(0),
        )?;
        if !has_digest {
            conn.execute_batch(
                "ALTER TABLE thumbs ADD COLUMN digest TEXT NOT NULL DEFAULT 'embedded'",
            )?;
        }
        Ok(Self { conn })
    }

    /// Store a thumbnail. `digest` identifies what it shows: [`EMBEDDED`] for the camera preview,
    /// otherwise the digest of the develop settings it was rendered with.
    pub fn put(
        &self,
        image_id: i64,
        width: u32,
        height: u32,
        jpeg: &[u8],
        digest: &str,
    ) -> Result<(), PreviewError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO thumbs (image_id, width, height, jpeg, digest) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![image_id, width, height, jpeg, digest],
        )?;
        Ok(())
    }

    /// Thumbnail and the digest it was made for.
    pub fn get_with_digest(
        &self,
        image_id: i64,
    ) -> Result<Option<(Vec<u8>, String)>, PreviewError> {
        Ok(self
            .conn
            .query_row(
                "SELECT jpeg, digest FROM thumbs WHERE image_id = ?1",
                [image_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
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

/// Long edge of loupe previews, in pixels.
pub const PREVIEW_LONG_EDGE: u32 = 2048;

/// A loupe preview and grid thumbnail rendered together: (width, height, JPEG) each.
pub struct Rendered {
    pub preview: (u32, u32, Vec<u8>),
    pub thumb: (u32, u32, Vec<u8>),
}

/// Render a photo with `settings` at preview size (on `gpu` if given) and encode both cache levels.
/// Raws and edited JPEGs go through the pipeline; untouched JPEGs are shown as they are.
pub fn render_previews(
    path: &Path,
    orientation: u8,
    settings: &pe_pipeline::Settings,
    gpu: Option<&pe_gpu::Gpu>,
) -> Result<Rendered, PreviewError> {
    // Raws, and edited JPEGs, go through the pipeline; an untouched JPEG is shown as it is.
    let pipeline = matches!(meta::file_kind(path), Some(FileKind::Raw))
        || *settings != pe_pipeline::Settings::default();
    let img = if pipeline {
        let frame = pe_raw::decode_file(path)?;
        let small = pe_pipeline::prepare_preview(&frame, PREVIEW_LONG_EDGE as usize);
        drop(frame);
        let gpu_rgba = gpu.and_then(|g| {
            let f = g.upload(&small).ok()?;
            g.render(&f, settings).ok()
        });
        match gpu_rgba {
            Some((w, h, rgba)) => {
                let rgb = rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
                RgbImage::from_raw(w, h, rgb).ok_or(PreviewError::NoPreview)?
            }
            None => {
                let out = pe_pipeline::render(&small, settings)
                    .map_err(|e| PreviewError::Render(e.to_string()))?;
                let data = out.data.iter().map(|v| (v * 255.0 + 0.5) as u8).collect();
                RgbImage::from_raw(out.width as u32, out.height as u32, data)
                    .ok_or(PreviewError::NoPreview)?
            }
        }
    } else {
        quick_image(path, orientation, PREVIEW_LONG_EDGE)?
    };
    let p = fit(&img, PREVIEW_LONG_EDGE);
    let t = fit(&img, THUMB_LONG_EDGE);
    Ok(Rendered {
        preview: (p.width(), p.height(), encode_jpeg(&p, 85)?),
        thumb: (t.width(), t.height(), encode_jpeg(&t, 82)?),
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// SQLite store of loupe previews keyed by image id, with a size budget.
pub struct PreviewStore {
    conn: Connection,
}

impl PreviewStore {
    pub fn open(path: &Path) -> Result<Self, PreviewError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS previews (
                 image_id INTEGER PRIMARY KEY,
                 width INTEGER NOT NULL,
                 height INTEGER NOT NULL,
                 digest TEXT NOT NULL,
                 jpeg BLOB NOT NULL,
                 bytes INTEGER NOT NULL,
                 accessed INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS previews_accessed ON previews (accessed);",
        )?;
        Ok(Self { conn })
    }

    pub fn put(
        &self,
        image_id: i64,
        width: u32,
        height: u32,
        jpeg: &[u8],
        digest: &str,
    ) -> Result<(), PreviewError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO previews (image_id, width, height, digest, jpeg, bytes, accessed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![image_id, width, height, digest, jpeg, jpeg.len() as i64, now_secs()],
        )?;
        Ok(())
    }

    /// Preview JPEG and the digest it was rendered for; marks it as recently used.
    pub fn get(&self, image_id: i64) -> Result<Option<(Vec<u8>, String)>, PreviewError> {
        let row: Option<(Vec<u8>, String)> = self
            .conn
            .query_row(
                "SELECT jpeg, digest FROM previews WHERE image_id = ?1",
                [image_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if row.is_some() {
            self.conn.execute(
                "UPDATE previews SET accessed = ?2 WHERE image_id = ?1",
                params![image_id, now_secs()],
            )?;
        }
        Ok(row)
    }

    /// Digest of the stored preview, without reading the image.
    pub fn digest(&self, image_id: i64) -> Result<Option<String>, PreviewError> {
        Ok(self
            .conn
            .query_row(
                "SELECT digest FROM previews WHERE image_id = ?1",
                [image_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn remove(&self, image_id: i64) -> Result<(), PreviewError> {
        self.conn
            .execute("DELETE FROM previews WHERE image_id = ?1", [image_id])?;
        Ok(())
    }

    /// Total size of stored previews in bytes.
    pub fn total_bytes(&self) -> Result<i64, PreviewError> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(SUM(bytes), 0) FROM previews", [], |r| {
                r.get(0)
            })?)
    }

    /// Delete least-recently-used previews until the total is at most `max_bytes`. Returns how many
    /// were removed.
    pub fn evict_to(&self, max_bytes: i64) -> Result<usize, PreviewError> {
        let mut total = self.total_bytes()?;
        if total <= max_bytes {
            return Ok(0);
        }
        let victims: Vec<(i64, i64)> = self
            .conn
            .prepare("SELECT image_id, bytes FROM previews ORDER BY accessed, image_id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut removed = 0;
        for (id, bytes) in victims {
            if total <= max_bytes {
                break;
            }
            self.remove(id)?;
            total -= bytes;
            removed += 1;
        }
        Ok(removed)
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
        s.put(1, 2, 3, b"abc", EMBEDDED).unwrap();
        assert_eq!(s.get(1).unwrap().as_deref(), Some(&b"abc"[..]));
        assert!(s.has(1).unwrap());
        s.remove(1).unwrap();
        assert!(!s.has(1).unwrap());
    }

    #[test]
    fn preview_store_evicts_least_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        let s = PreviewStore::open(&dir.path().join("p.sqlite")).unwrap();
        for id in 1..=3 {
            s.put(id, 1, 1, &[0u8; 100], "d").unwrap();
        }
        // Make 1 the most recently used.
        s.conn
            .execute(
                "UPDATE previews SET accessed = accessed - 10 WHERE image_id != 1",
                [],
            )
            .unwrap();
        assert_eq!(s.total_bytes().unwrap(), 300);
        assert_eq!(s.evict_to(150).unwrap(), 2);
        assert!(s.get(1).unwrap().is_some());
        assert!(s.digest(2).unwrap().is_none() && s.digest(3).unwrap().is_none());
    }
}
