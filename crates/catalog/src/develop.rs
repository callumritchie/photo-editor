//! Develop state per image: current settings, history (one full snapshot per committed change),
//! named snapshots, and presets. All changes go through [`DevelopCommand`].
//!
//! History model:
//! - The first change to an image records an "Original" step holding its previous settings.
//! - Each commit appends a step and makes it current. Committing after an undo discards the steps that
//!   were ahead of the current one, like a conventional undo stack.
//! - Undo/redo move the current pointer one step back/forward. Going to a step does the same without
//!   adding a step.

use pe_develop::{apply_groups, canonical_json, describe_change, digest, Group};
use pe_pipeline::Settings;
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::time::now_ms;
use crate::{Catalog, CatalogError, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "camelCase")]
pub enum DevelopCommand {
    /// Commit new settings (one user gesture). `label` defaults to a description of the change.
    #[serde(rename_all = "camelCase")]
    Set {
        settings: Settings,
        label: Option<String>,
    },
    /// Copy the given groups from `settings` (paste, sync).
    #[serde(rename_all = "camelCase")]
    Paste {
        settings: Settings,
        groups: Vec<Group>,
        label: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ApplyPreset {
        preset_id: i64,
    },
    Reset,
    #[serde(rename_all = "camelCase")]
    GoToStep {
        step_id: i64,
    },
    Undo,
    Redo,
    ClearHistory,
    #[serde(rename_all = "camelCase")]
    CreateSnapshot {
        name: String,
    },
    #[serde(rename_all = "camelCase")]
    ApplySnapshot {
        snapshot_id: i64,
    },
    #[serde(rename_all = "camelCase")]
    DeleteSnapshot {
        snapshot_id: i64,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStep {
    pub id: i64,
    pub label: String,
    pub created_at: i64,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevelopState {
    pub image_id: i64,
    pub settings: Settings,
    pub digest: String,
    /// Whether the settings differ from defaults.
    pub edited: bool,
    pub current_step: Option<i64>,
    pub history: Vec<HistoryStep>,
    pub snapshots: Vec<Snapshot>,
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub groups: Vec<Group>,
    pub settings: Settings,
}

fn parse_settings(s: &str) -> Result<Settings> {
    serde_json::from_str(s).map_err(|e| CatalogError::Invalid(format!("bad stored settings: {e}")))
}

/// Current settings and current step for an image inside a transaction.
fn current(tx: &Transaction, image_id: i64) -> Result<(Settings, Option<i64>)> {
    let row: Option<(String, Option<i64>)> = tx
        .query_row(
            "SELECT settings, current_step FROM develop WHERE image_id = ?1",
            [image_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((s, step)) => Ok((parse_settings(&s)?, step)),
        None => Ok((Settings::default(), None)),
    }
}

fn write_current(tx: &Transaction, image_id: i64, s: &Settings, step: Option<i64>) -> Result<()> {
    tx.execute(
        "INSERT INTO develop (image_id, settings, digest, current_step, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (image_id) DO UPDATE SET settings = ?2, digest = ?3, current_step = ?4, updated_at = ?5",
        params![image_id, canonical_json(s), digest(s), step, now_ms()],
    )?;
    Ok(())
}

fn add_step(tx: &Transaction, image_id: i64, label: &str, s: &Settings) -> Result<i64> {
    tx.execute(
        "INSERT INTO history (image_id, created_at, label, settings, digest) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![image_id, now_ms(), label, canonical_json(s), digest(s)],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Commit `new` as the image's current settings with a history step. No-op if unchanged.
fn commit(tx: &Transaction, image_id: i64, new: &Settings, label: Option<&str>) -> Result<()> {
    let new = new.clone().clamped();
    let (old, step) = current(tx, image_id)?;
    if digest(&old) == digest(&new) {
        return Ok(());
    }
    let has_history: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM history WHERE image_id = ?1)",
        [image_id],
        |r| r.get(0),
    )?;
    let mut step = step;
    if !has_history {
        step = Some(add_step(tx, image_id, "Original", &old)?);
    }
    if let Some(cur) = step {
        tx.execute(
            "DELETE FROM history WHERE image_id = ?1 AND id > ?2",
            params![image_id, cur],
        )?;
    }
    let label = label
        .map(str::to_owned)
        .or_else(|| describe_change(&old, &new))
        .unwrap_or_else(|| "Edit".into());
    let id = add_step(tx, image_id, &label, &new)?;
    write_current(tx, image_id, &new, Some(id))
}

fn go_to(tx: &Transaction, image_id: i64, step_id: i64) -> Result<()> {
    let s: Option<String> = tx
        .query_row(
            "SELECT settings FROM history WHERE id = ?1 AND image_id = ?2",
            params![step_id, image_id],
            |r| r.get(0),
        )
        .optional()?;
    let s = s.ok_or_else(|| {
        CatalogError::Invalid(format!("no history step {step_id} for image {image_id}"))
    })?;
    write_current(tx, image_id, &parse_settings(&s)?, Some(step_id))
}

fn neighbour(
    tx: &Transaction,
    image_id: i64,
    from: Option<i64>,
    forward: bool,
) -> Result<Option<i64>> {
    let Some(cur) = from else { return Ok(None) };
    let sql = if forward {
        "SELECT MIN(id) FROM history WHERE image_id = ?1 AND id > ?2"
    } else {
        "SELECT MAX(id) FROM history WHERE image_id = ?1 AND id < ?2"
    };
    Ok(tx.query_row(sql, params![image_id, cur], |r| r.get(0))?)
}

impl Catalog {
    /// Current settings for an image (defaults if never edited).
    pub fn develop_settings(&self, image_id: i64) -> Result<Settings> {
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT settings FROM develop WHERE image_id = ?1",
                [image_id],
                |r| r.get(0),
            )
            .optional()?;
        s.map(|s| parse_settings(&s))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    pub fn develop_state(&self, image_id: i64) -> Result<DevelopState> {
        let settings = self.develop_settings(image_id)?;
        let current_step: Option<i64> = self
            .conn
            .query_row(
                "SELECT current_step FROM develop WHERE image_id = ?1",
                [image_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let history: Vec<HistoryStep> = self
            .conn
            .prepare(
                "SELECT id, label, created_at, digest FROM history WHERE image_id = ?1 ORDER BY id",
            )?
            .query_map([image_id], |r| {
                Ok(HistoryStep {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    created_at: r.get(2)?,
                    digest: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let snapshots: Vec<Snapshot> = self
            .conn
            .prepare("SELECT id, name, created_at, digest FROM snapshots WHERE image_id = ?1 ORDER BY id")?
            .query_map([image_id], |r| {
                Ok(Snapshot { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)?, digest: r.get(3)? })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let pos = current_step.and_then(|c| history.iter().position(|h| h.id == c));
        let d = digest(&settings);
        Ok(DevelopState {
            image_id,
            edited: d != digest(&Settings::default()),
            digest: d,
            can_undo: pos.is_some_and(|p| p > 0),
            can_redo: pos.is_some_and(|p| p + 1 < history.len()),
            settings,
            current_step,
            history,
            snapshots,
        })
    }

    /// Run a develop command on each image (one transaction), returning the new states.
    pub fn develop_execute(
        &mut self,
        image_ids: &[i64],
        cmd: &DevelopCommand,
    ) -> Result<Vec<DevelopState>> {
        let preset = match cmd {
            DevelopCommand::ApplyPreset { preset_id } => Some(self.preset(*preset_id)?),
            _ => None,
        };
        let tx = self.conn.transaction()?;
        for &id in image_ids {
            match cmd {
                DevelopCommand::Set { settings, label } => {
                    commit(&tx, id, settings, label.as_deref())?
                }
                DevelopCommand::Paste {
                    settings,
                    groups,
                    label,
                } => {
                    let (mut s, _) = current(&tx, id)?;
                    apply_groups(&mut s, settings, groups);
                    commit(
                        &tx,
                        id,
                        &s,
                        Some(label.as_deref().unwrap_or("Paste Settings")),
                    )?;
                }
                DevelopCommand::ApplyPreset { .. } => {
                    let p = preset.as_ref().expect("loaded above");
                    let (mut s, _) = current(&tx, id)?;
                    apply_groups(&mut s, &p.settings, &p.groups);
                    commit(&tx, id, &s, Some(&format!("Preset: {}", p.name)))?;
                }
                DevelopCommand::Reset => commit(&tx, id, &Settings::default(), Some("Reset"))?,
                DevelopCommand::GoToStep { step_id } => go_to(&tx, id, *step_id)?,
                DevelopCommand::Undo | DevelopCommand::Redo => {
                    let (_, step) = current(&tx, id)?;
                    if let Some(target) =
                        neighbour(&tx, id, step, matches!(cmd, DevelopCommand::Redo))?
                    {
                        go_to(&tx, id, target)?;
                    }
                }
                DevelopCommand::ClearHistory => {
                    tx.execute("DELETE FROM history WHERE image_id = ?1", [id])?;
                    tx.execute(
                        "UPDATE develop SET current_step = NULL WHERE image_id = ?1",
                        [id],
                    )?;
                }
                DevelopCommand::CreateSnapshot { name } => {
                    let (s, _) = current(&tx, id)?;
                    tx.execute(
                        "INSERT INTO snapshots (uuid, image_id, name, created_at, settings, digest)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            uuid::Uuid::new_v4().to_string(),
                            id,
                            name,
                            now_ms(),
                            canonical_json(&s),
                            digest(&s)
                        ],
                    )?;
                }
                DevelopCommand::ApplySnapshot { snapshot_id } => {
                    let row: Option<(String, String)> = tx
                        .query_row(
                            "SELECT name, settings FROM snapshots WHERE id = ?1 AND image_id = ?2",
                            params![snapshot_id, id],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    let (name, s) = row.ok_or_else(|| {
                        CatalogError::Invalid(format!("no snapshot {snapshot_id}"))
                    })?;
                    commit(
                        &tx,
                        id,
                        &parse_settings(&s)?,
                        Some(&format!("Snapshot: {name}")),
                    )?;
                }
                DevelopCommand::DeleteSnapshot { snapshot_id } => {
                    tx.execute(
                        "DELETE FROM snapshots WHERE id = ?1 AND image_id = ?2",
                        params![snapshot_id, id],
                    )?;
                }
            }
        }
        tx.commit()?;
        image_ids.iter().map(|&id| self.develop_state(id)).collect()
    }

    pub fn presets(&self) -> Result<Vec<Preset>> {
        let rows: Vec<(i64, String, String, String, String)> = self
            .conn
            .prepare(
                "SELECT id, uuid, name, groups, settings FROM presets ORDER BY name COLLATE NOCASE",
            )?
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(id, uuid, name, groups, settings)| {
                Ok(Preset {
                    id,
                    uuid,
                    name,
                    groups: serde_json::from_str(&groups)
                        .map_err(|e| CatalogError::Invalid(e.to_string()))?,
                    settings: parse_settings(&settings)?,
                })
            })
            .collect()
    }

    pub fn preset(&self, id: i64) -> Result<Preset> {
        self.presets()?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| CatalogError::Invalid(format!("no preset {id}")))
    }

    /// Save (or replace, by name) a preset holding `groups` of `settings`.
    pub fn save_preset(
        &mut self,
        name: &str,
        groups: &[Group],
        settings: &Settings,
    ) -> Result<Preset> {
        if name.trim().is_empty() || groups.is_empty() {
            return Err(CatalogError::Invalid(
                "a preset needs a name and at least one group".into(),
            ));
        }
        let groups_json =
            serde_json::to_string(groups).map_err(|e| CatalogError::Invalid(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO presets (uuid, name, groups, settings, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (name) DO UPDATE SET groups = ?3, settings = ?4",
            params![uuid::Uuid::new_v4().to_string(), name.trim(), groups_json, canonical_json(settings), now_ms()],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM presets WHERE name = ?1",
            [name.trim()],
            |r| r.get(0),
        )?;
        self.preset(id)
    }

    pub fn delete_preset(&mut self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM presets WHERE id = ?1", [id])?;
        Ok(())
    }
}
