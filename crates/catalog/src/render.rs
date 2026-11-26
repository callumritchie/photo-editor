//! Cached renders: loupe previews and edited grid thumbnails, produced by background jobs.
//!
//! The catalog describes the work ([`RenderJob`]); a worker renders it without holding the catalog
//! (decoding a raw takes hundreds of milliseconds) and writes the results through its own
//! [`RenderStores`] connections. A job whose settings changed while it ran simply stores a result
//! under the old digest, which then reads as stale and is rendered again on the next request.

use std::path::{Path, PathBuf};

use pe_preview::{PreviewStore, ThumbStore, EMBEDDED};

use crate::{Catalog, Result, Settings};

/// Default size budget for cached loupe previews (about 1,200 photos at 2048 px).
pub const PREVIEW_BUDGET_BYTES: i64 = 1 << 30;

/// Everything needed to render one photo's previews.
#[derive(Debug, Clone)]
pub struct RenderJob {
    pub id: i64,
    pub path: PathBuf,
    pub orientation: u8,
    pub settings: Settings,
    /// Digest of `settings`: what the loupe preview must show.
    pub digest: String,
    /// What the grid thumbnail must show: [`EMBEDDED`] for unedited photos, else `digest`.
    pub thumb_digest: String,
}

/// Separate connections to the preview and thumbnail stores, for use from a worker thread.
pub struct RenderStores {
    thumbs: ThumbStore,
    previews: PreviewStore,
    saved: u32,
}

impl RenderStores {
    pub fn open(catalog_dir: &Path) -> Result<Self> {
        Ok(Self {
            thumbs: ThumbStore::open(&catalog_dir.join("thumbs.sqlite"))?,
            previews: PreviewStore::open(&catalog_dir.join("previews.sqlite"))?,
            saved: 0,
        })
    }

    /// True when both cached levels already show the job's settings.
    pub fn is_fresh(&self, job: &RenderJob) -> Result<bool> {
        let preview_ok = self.previews.digest(job.id)?.as_deref() == Some(&job.digest[..]);
        let thumb_ok = job.thumb_digest == EMBEDDED
            || self.thumbs.get_with_digest(job.id)?.map(|t| t.1).as_deref()
                == Some(&job.thumb_digest[..]);
        Ok(preview_ok && thumb_ok)
    }

    /// Render and store. Unedited photos keep the camera's embedded thumbnail. Returns whether the
    /// grid thumbnail changed.
    pub fn run(&mut self, job: &RenderJob, gpu: Option<&pe_gpu::Gpu>) -> Result<bool> {
        let r = pe_preview::render_previews(&job.path, job.orientation, &job.settings, gpu)?;
        let (w, h, jpeg) = &r.preview;
        self.previews.put(job.id, *w, *h, jpeg, &job.digest)?;
        let thumb_changed = job.thumb_digest != EMBEDDED;
        if thumb_changed {
            let (w, h, jpeg) = &r.thumb;
            self.thumbs.put(job.id, *w, *h, jpeg, &job.thumb_digest)?;
        }
        self.saved += 1;
        if self.saved.is_multiple_of(25) {
            self.previews.evict_to(PREVIEW_BUDGET_BYTES)?;
        }
        Ok(thumb_changed)
    }
}

impl Catalog {
    /// Current settings, their digest, and the digest the grid thumbnail should carry.
    fn render_digests(&self, id: i64) -> Result<(Settings, String, String)> {
        let settings = self.develop_settings(id)?;
        let digest = pe_develop::digest(&settings);
        let thumb = if digest == pe_develop::digest(&Settings::default()) {
            EMBEDDED.to_string()
        } else {
            digest.clone()
        };
        Ok((settings, digest, thumb))
    }

    pub fn render_job(&self, id: i64) -> Result<Option<RenderJob>> {
        let Some(img) = self.image(id)? else {
            return Ok(None);
        };
        let (mut settings, digest, thumb_digest) = self.render_digests(id)?;
        self.attach_rasters(&mut settings)?;
        Ok(Some(RenderJob {
            id,
            path: PathBuf::from(img.path),
            orientation: img.orientation,
            settings,
            digest,
            thumb_digest,
        }))
    }

    /// Cached loupe preview and whether it shows the current settings (`false`: render again).
    pub fn preview(&self, id: i64) -> Result<Option<(Vec<u8>, bool)>> {
        let Some((jpeg, have)) = self.previews.get(id)? else {
            return Ok(None);
        };
        let (_, digest, _) = self.render_digests(id)?;
        Ok(Some((jpeg, have == digest)))
    }

    /// Grid thumbnail without rendering: the stored one (possibly stale), or for unedited photos the
    /// camera preview made on the spot. The flag says whether a render job is needed.
    pub fn thumbnail_quick(&self, id: i64) -> Result<(Option<Vec<u8>>, bool)> {
        let (_, _, wanted) = self.render_digests(id)?;
        let stored = self.thumbs.get_with_digest(id)?;
        if let Some((jpeg, have)) = &stored {
            if *have == wanted {
                return Ok((Some(jpeg.clone()), false));
            }
        }
        if wanted == EMBEDDED || stored.is_none() {
            // Embedded previews are quick; also a placeholder for edited photos with no thumbnail yet.
            let Some(img) = self.image(id)? else {
                return Ok((None, false));
            };
            if let Ok((w, h, jpeg)) =
                pe_preview::make_thumbnail(Path::new(&img.path), img.orientation)
            {
                self.thumbs.put(id, w, h, &jpeg, EMBEDDED)?;
                return Ok((Some(jpeg), wanted != EMBEDDED));
            }
            return Ok((None, wanted != EMBEDDED));
        }
        Ok((stored.map(|s| s.0), true))
    }
}
