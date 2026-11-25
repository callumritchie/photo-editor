//! Export from the catalog: resolve images to files, develop settings and metadata, then hand off to
//! `pe-export`. Also stores named export presets.

use std::path::PathBuf;

use pe_export::{ExportItem, ExportProgress, ExportReport, ExportSettings, PhotoMeta};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::time::now_ms;
use crate::{Catalog, CatalogError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportPreset {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub settings: ExportSettings,
}

impl Catalog {
    /// Everything needed to export these images, in the given order (unknown ids are skipped).
    pub fn export_items(&self, ids: &[i64]) -> Result<Vec<ExportItem>> {
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            let Some(img) = self.image(id)? else { continue };
            let meta = self.conn.query_row(
                "SELECT camera_make, camera_model, lens, capture_time, capture_offset, iso, exposure_time,
                        f_number, focal_length FROM images WHERE id = ?1",
                [id],
                |r| {
                    Ok(PhotoMeta {
                        make: r.get(0)?,
                        model: r.get(1)?,
                        lens: r.get(2)?,
                        capture_time: r.get(3)?,
                        capture_offset: r.get(4)?,
                        iso: r.get::<_, Option<i64>>(5)?.map(|v| v as u32),
                        exposure_time: r.get(6)?,
                        f_number: r.get(7)?,
                        focal_length: r.get(8)?,
                    })
                },
            )?;
            out.push(ExportItem {
                source: PathBuf::from(img.path),
                settings: self.develop_settings(id)?,
                meta,
            });
        }
        Ok(out)
    }

    /// Render and write the given images. Rendering runs without holding catalog state beyond the
    /// initial lookup.
    pub fn export(
        &self,
        ids: &[i64],
        settings: &ExportSettings,
        progress: &mut dyn FnMut(&ExportProgress),
    ) -> Result<ExportReport> {
        let items = self.export_items(ids)?;
        Ok(pe_export::export(&items, settings, progress))
    }

    pub fn export_presets(&self) -> Result<Vec<ExportPreset>> {
        let rows: Vec<(i64, String, String, String)> = self
            .conn
            .prepare(
                "SELECT id, uuid, name, settings FROM export_presets ORDER BY name COLLATE NOCASE",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(id, uuid, name, s)| {
                Ok(ExportPreset {
                    id,
                    uuid,
                    name,
                    settings: serde_json::from_str(&s)
                        .map_err(|e| CatalogError::Invalid(e.to_string()))?,
                })
            })
            .collect()
    }

    pub fn save_export_preset(
        &mut self,
        name: &str,
        settings: &ExportSettings,
    ) -> Result<ExportPreset> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CatalogError::Invalid(
                "an export preset needs a name".into(),
            ));
        }
        let json =
            serde_json::to_string(settings).map_err(|e| CatalogError::Invalid(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO export_presets (uuid, name, settings, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (name) DO UPDATE SET settings = ?3",
            params![uuid::Uuid::new_v4().to_string(), name, json, now_ms()],
        )?;
        self.export_presets()?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| CatalogError::Invalid("preset not saved".into()))
    }

    pub fn delete_export_preset(&mut self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM export_presets WHERE id = ?1", [id])?;
        Ok(())
    }
}
