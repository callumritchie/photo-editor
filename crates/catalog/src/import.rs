//! Import: catalog files in place (Add) or copy them into a dated folder tree (Copy).
//!
//! Crash safety:
//! - Work proceeds in batches. Copies are written to a hidden temporary name, given the source's
//!   modification time, then atomically renamed into place.
//! - All catalog records for a batch are written in a single transaction after its copies finish.
//! - Re-running an interrupted import is safe: a destination file that already exists with the same size
//!   and modification time is reused instead of being copied again or duplicated, and images already in
//!   the catalog are recognised by their quick hash.
//!
//! Duplicate detection: a *quick hash* of lowercase file name, size and modification time (seconds).
//! It needs no file reads, so it is cheap on large card imports. Copies keep the source's modification
//! time so their quick hash matches the original's.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use pe_raw::meta::{self, FileKind, Metadata};
use rayon::prelude::*;
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::time::{expand_template, now_ms, system_time_ms, utc_iso_from_ms};
use crate::{Catalog, CatalogError, Result};

/// Where imported files end up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ImportMode {
    /// Catalog files where they are.
    Add,
    /// Copy into `dest`, under subfolders built from `template` (%Y %m %d %H %M %S of capture time).
    Copy { dest: PathBuf, template: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImportOptions {
    pub mode: ImportMode,
    /// Descend into subfolders of source folders.
    pub recursive: bool,
    /// Skip files whose quick hash matches a file already in the catalog (or earlier in this import).
    pub skip_duplicates: bool,
    pub batch_size: usize,
    /// Build grid thumbnails during import (otherwise they are made on first view).
    pub thumbnails: bool,
    /// Testing only: fail after copying, before committing, the batch with this index.
    #[serde(skip)]
    pub simulate_crash_in_batch: Option<usize>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            mode: ImportMode::Add,
            recursive: true,
            skip_duplicates: true,
            batch_size: 64,
            thumbnails: true,
            simulate_crash_in_batch: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProgress {
    pub done: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub import_id: i64,
    pub found: usize,
    pub imported: usize,
    pub duplicates: usize,
    pub already_in_catalog: usize,
    pub failed: Vec<(String, String)>,
    pub thumbnails_failed: usize,
}

#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    /// Directory that becomes (or is inside) the catalog root, for Add mode.
    root_hint: PathBuf,
    name: String,
    size: u64,
    mtime_ms: i64,
    quick_hash: String,
    kind: FileKind,
}

fn quick_hash(name: &str, size: u64, mtime_ms: i64) -> String {
    format!(
        "{}:{}:{}",
        name.to_lowercase(),
        size,
        mtime_ms.div_euclid(1000)
    )
}

fn is_hidden(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

fn collect(
    source: &Path,
    recursive: bool,
    out: &mut Vec<Candidate>,
    failed: &mut Vec<(String, String)>,
) {
    let root_hint = if source.is_dir() {
        source.to_path_buf()
    } else {
        source.parent().unwrap_or(Path::new("/")).to_path_buf()
    };
    let mut stack = vec![source.to_path_buf()];
    while let Some(p) = stack.pop() {
        if p != source && is_hidden(&p) {
            continue;
        }
        if p.is_dir() {
            if p == source || recursive {
                match fs::read_dir(&p) {
                    Ok(rd) => stack.extend(rd.flatten().map(|e| e.path())),
                    Err(e) => failed.push((p.display().to_string(), e.to_string())),
                }
            }
            continue;
        }
        let Some(kind) = meta::file_kind(&p) else {
            continue;
        };
        let Some(name) = p.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            failed.push((
                p.display().to_string(),
                "file name is not valid UTF-8".into(),
            ));
            continue;
        };
        match fs::metadata(&p) {
            Ok(md) => {
                let mtime_ms = md.modified().map(system_time_ms).unwrap_or(0);
                out.push(Candidate {
                    quick_hash: quick_hash(&name, md.len(), mtime_ms),
                    path: p,
                    root_hint: root_hint.clone(),
                    name,
                    size: md.len(),
                    mtime_ms,
                    kind,
                });
            }
            Err(e) => failed.push((p.display().to_string(), e.to_string())),
        }
    }
}

/// Copy `src` to `dest_dir`, reusing an identical existing file. Returns the final path.
fn copy_into(src: &Candidate, dest_dir: &Path) -> std::io::Result<PathBuf> {
    fs::create_dir_all(dest_dir)?;
    let (stem, ext) = match src.name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (src.name.clone(), String::new()),
    };
    for n in 0..10_000 {
        let name = if n == 0 {
            src.name.clone()
        } else {
            format!("{stem}-{n}{ext}")
        };
        let target = dest_dir.join(&name);
        match fs::metadata(&target) {
            Ok(md) => {
                let same_time = md
                    .modified()
                    .map(|t| (system_time_ms(t) - src.mtime_ms).abs() < 2000)
                    .unwrap_or(false);
                if md.len() == src.size && same_time {
                    return Ok(target); // left by an interrupted import: reuse
                }
            }
            Err(_) => {
                let tmp = dest_dir.join(format!(".{name}.pe-partial"));
                fs::copy(&src.path, &tmp)?;
                let mtime = UNIX_EPOCH + Duration::from_millis(src.mtime_ms.max(0) as u64);
                fs::File::options()
                    .write(true)
                    .open(&tmp)?
                    .set_modified(mtime)?;
                fs::rename(&tmp, &target)?;
                return Ok(target);
            }
        }
    }
    Err(std::io::Error::other("could not find a free file name"))
}

/// Find (or create) the root and folder records for directory `dir`. An existing root that contains
/// `dir` is reused; otherwise `root_hint` becomes a new root.
fn ensure_folder(tx: &Transaction, dir: &Path, root_hint: &Path) -> Result<i64> {
    let dir_s = dir
        .to_str()
        .ok_or_else(|| CatalogError::Invalid("path is not UTF-8".into()))?;
    let roots: Vec<(i64, String)> = tx
        .prepare("SELECT id, path FROM roots")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let existing = roots
        .into_iter()
        .filter(|(_, p)| Path::new(dir_s).starts_with(p))
        .max_by_key(|(_, p)| p.len());
    let (root_id, root_path) = match existing {
        Some(r) => r,
        None => {
            let hint = if dir.starts_with(root_hint) {
                root_hint
            } else {
                dir
            };
            let hint_s = hint
                .to_str()
                .ok_or_else(|| CatalogError::Invalid("path is not UTF-8".into()))?;
            tx.execute(
                "INSERT INTO roots (uuid, path) VALUES (?1, ?2)",
                params![uuid::Uuid::new_v4().to_string(), hint_s],
            )?;
            (tx.last_insert_rowid(), hint_s.to_string())
        }
    };
    let rel = Path::new(dir_s)
        .strip_prefix(&root_path)
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default();
    if let Some(id) = tx
        .query_row(
            "SELECT id FROM folders WHERE root_id = ?1 AND rel_path = ?2",
            params![root_id, rel],
            |r| r.get(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    tx.execute(
        "INSERT INTO folders (uuid, root_id, rel_path) VALUES (?1, ?2, ?3)",
        params![uuid::Uuid::new_v4().to_string(), root_id, rel],
    )?;
    Ok(tx.last_insert_rowid())
}

impl Catalog {
    /// Import `sources` (files and/or folders). `progress` is called after each batch.
    pub fn import(
        &mut self,
        sources: &[PathBuf],
        opts: &ImportOptions,
        progress: &mut dyn FnMut(&ImportProgress),
    ) -> Result<ImportReport> {
        let mut report = ImportReport::default();
        let mut found = Vec::new();
        for s in sources {
            let s = fs::canonicalize(s)?;
            collect(&s, opts.recursive, &mut found, &mut report.failed);
        }
        found.sort_by(|a, b| a.path.cmp(&b.path));
        report.found = found.len();

        // What the catalog already holds.
        let mut known_hashes: HashSet<String> = self
            .conn
            .prepare("SELECT quick_hash FROM files")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let known_paths: HashSet<String> = self
            .images(&Default::default())?
            .into_iter()
            .map(|i| i.path)
            .collect();

        let mut todo = Vec::new();
        for c in found {
            if known_paths.contains(c.path.to_string_lossy().as_ref()) {
                report.already_in_catalog += 1;
            } else if opts.skip_duplicates && !known_hashes.insert(c.quick_hash.clone()) {
                report.duplicates += 1;
            } else {
                todo.push(c);
            }
        }

        let mode_name = match &opts.mode {
            ImportMode::Add => "add",
            ImportMode::Copy { .. } => "copy",
        };
        self.conn.execute(
            "INSERT INTO imports (started_at, mode) VALUES (?1, ?2)",
            params![now_ms(), mode_name],
        )?;
        report.import_id = self.conn.last_insert_rowid();

        let total = todo.len();
        let mut done = 0;
        for (batch_index, batch) in todo.chunks(opts.batch_size.max(1)).enumerate() {
            let metas: Vec<Metadata> = batch
                .par_iter()
                .map(|c| match c.kind {
                    FileKind::Raw => meta::raw_metadata(&c.path).unwrap_or_default(),
                    FileKind::Jpeg => Metadata {
                        orientation: 1,
                        ..Default::default()
                    },
                })
                .collect();

            // Place files (copy mode) before touching the database.
            let mut placed: Vec<Option<(PathBuf, PathBuf)>> = Vec::with_capacity(batch.len());
            for (c, m) in batch.iter().zip(&metas) {
                match &opts.mode {
                    ImportMode::Add => placed.push(Some((c.path.clone(), c.root_hint.clone()))),
                    ImportMode::Copy { dest, template } => {
                        let when = m
                            .capture_time
                            .clone()
                            .unwrap_or_else(|| utc_iso_from_ms(c.mtime_ms));
                        let dir = dest.join(expand_template(template, &when));
                        match copy_into(c, &dir) {
                            Ok(p) => placed.push(Some((p, dest.clone()))),
                            Err(e) => {
                                report
                                    .failed
                                    .push((c.path.display().to_string(), e.to_string()));
                                placed.push(None);
                            }
                        }
                    }
                }
            }
            if opts.simulate_crash_in_batch == Some(batch_index) {
                return Err(CatalogError::Invalid("simulated crash".into()));
            }

            let mut inserted = Vec::new();
            let tx = self.conn.transaction()?;
            for ((c, m), place) in batch.iter().zip(&metas).zip(&placed) {
                let Some((path, root_hint)) = place else {
                    continue;
                };
                let root_hint = fs::canonicalize(root_hint).unwrap_or_else(|_| root_hint.clone());
                let path = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                let dir = path.parent().unwrap_or(Path::new("/"));
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&c.name)
                    .to_string();
                let folder_id = ensure_folder(&tx, dir, &root_hint)?;
                let changed = tx.execute(
                    "INSERT INTO files (uuid, folder_id, name, size, mtime_ms, quick_hash)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (folder_id, name) DO NOTHING",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        folder_id,
                        name,
                        c.size as i64,
                        c.mtime_ms,
                        c.quick_hash
                    ],
                )?;
                if changed == 0 {
                    report.already_in_catalog += 1;
                    continue;
                }
                let file_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO images (uuid, file_id, imported_at, capture_time, capture_offset, orientation,
                         camera_make, camera_model, lens, iso, exposure_time, f_number, focal_length)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        file_id,
                        now_ms(),
                        m.capture_time,
                        m.capture_offset,
                        m.orientation.clamp(1, 8),
                        m.make,
                        m.model,
                        m.lens,
                        m.iso,
                        m.exposure_time,
                        m.f_number,
                        m.focal_length
                    ],
                )?;
                let image_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO import_images (import_id, image_id) VALUES (?1, ?2)",
                    params![report.import_id, image_id],
                )?;
                inserted.push((image_id, path.clone(), m.orientation));
            }
            tx.commit()?;
            report.imported += inserted.len();

            if opts.thumbnails {
                let thumbs: Vec<_> = inserted
                    .par_iter()
                    .map(|(id, path, o)| (*id, pe_preview::make_thumbnail(path, *o)))
                    .collect();
                for (id, t) in thumbs {
                    match t {
                        Ok((w, h, jpeg)) => {
                            self.thumbs().put(id, w, h, &jpeg, pe_preview::EMBEDDED)?
                        }
                        Err(_) => report.thumbnails_failed += 1,
                    }
                }
            }
            done += batch.len();
            progress(&ImportProgress { done, total });
        }

        self.conn.execute(
            "UPDATE imports SET finished_at = ?2 WHERE id = ?1",
            params![report.import_id, now_ms()],
        )?;
        Ok(report)
    }
}
