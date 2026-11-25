//! Photo catalog: a folder `Name.pecat/` holding `catalog.sqlite` (records) and `thumbs.sqlite`
//! (grid thumbnails).

pub mod develop;
pub mod import;
pub mod schema;
mod time;

use std::path::{Path, PathBuf};

use pe_preview::ThumbStore;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

pub use develop::{DevelopCommand, DevelopState, HistoryStep, Preset, Snapshot};
pub use import::{ImportMode, ImportOptions, ImportProgress, ImportReport};
pub use pe_develop::Group;
pub use pe_pipeline::Settings;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("preview: {0}")]
    Preview(#[from] pe_preview::PreviewError),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, CatalogError>;

/// Flag state of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Flag {
    Rejected = -1,
    None = 0,
    Picked = 1,
}

impl Flag {
    fn from_i64(v: i64) -> Self {
        match v {
            -1 => Flag::Rejected,
            1 => Flag::Picked,
            _ => Flag::None,
        }
    }
}

pub const LABELS: [&str; 5] = ["red", "yellow", "green", "blue", "purple"];

/// One image as listed in the grid.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRow {
    pub id: i64,
    pub uuid: String,
    pub folder_id: i64,
    pub path: String,
    pub name: String,
    pub capture_time: Option<String>,
    pub rating: u8,
    pub flag: Flag,
    pub label: Option<String>,
    pub orientation: u8,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<u32>,
    pub exposure_time: Option<f64>,
    pub f_number: Option<f64>,
    pub focal_length: Option<f64>,
}

/// A folder with its image count (direct children only).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRow {
    pub id: i64,
    pub root_path: String,
    pub rel_path: String,
    pub path: String,
    pub image_count: i64,
}

/// Grid query.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Query {
    /// Restrict to one folder (and, if `recursive`, its subfolders).
    pub folder_id: Option<i64>,
    pub recursive: bool,
    pub min_rating: u8,
    /// Only images with this flag (-1, 0, 1).
    pub flag: Option<i8>,
    pub label: Option<String>,
}

pub struct Catalog {
    dir: PathBuf,
    pub(crate) conn: Connection,
    thumbs: ThumbStore,
}

const IMAGE_SELECT: &str = "SELECT i.id, i.uuid, f.folder_id, r.path, fo.rel_path, f.name, i.capture_time, i.rating, i.flag,
       i.label, i.orientation, i.camera_make, i.camera_model, i.lens, i.iso, i.exposure_time, i.f_number, i.focal_length
FROM images i JOIN files f ON f.id = i.file_id JOIN folders fo ON fo.id = f.folder_id JOIN roots r ON r.id = fo.root_id";

fn join_path(root: &str, rel: &str, name: &str) -> String {
    let mut p = PathBuf::from(root);
    if !rel.is_empty() {
        p.push(rel);
    }
    p.push(name);
    p.to_string_lossy().into_owned()
}

fn image_from_row(r: &rusqlite::Row) -> rusqlite::Result<ImageRow> {
    let root: String = r.get(3)?;
    let rel: String = r.get(4)?;
    let name: String = r.get(5)?;
    let make: Option<String> = r.get(11)?;
    let model: Option<String> = r.get(12)?;
    let camera = match (make, model) {
        (Some(a), Some(b)) if b.starts_with(&a) => Some(b),
        (Some(a), Some(b)) => Some(format!("{a} {b}")),
        (a, b) => a.or(b),
    };
    Ok(ImageRow {
        id: r.get(0)?,
        uuid: r.get(1)?,
        folder_id: r.get(2)?,
        path: join_path(&root, &rel, &name),
        name,
        capture_time: r.get(6)?,
        rating: r.get::<_, i64>(7)? as u8,
        flag: Flag::from_i64(r.get(8)?),
        label: r.get(9)?,
        orientation: r.get::<_, i64>(10)? as u8,
        camera,
        lens: r.get(13)?,
        iso: r.get::<_, Option<i64>>(14)?.map(|v| v as u32),
        exposure_time: r.get(15)?,
        f_number: r.get(16)?,
        focal_length: r.get(17)?,
    })
}

impl Catalog {
    /// Create a new catalog directory. Fails if it already exists.
    pub fn create(dir: &Path) -> Result<Self> {
        if dir.exists() {
            return Err(CatalogError::Invalid(format!(
                "{} already exists",
                dir.display()
            )));
        }
        std::fs::create_dir_all(dir)?;
        Self::open(dir)
    }

    /// Open (and migrate if needed) an existing catalog directory.
    pub fn open(dir: &Path) -> Result<Self> {
        if !dir.is_dir() {
            return Err(CatalogError::Invalid(format!(
                "{} is not a catalog",
                dir.display()
            )));
        }
        let mut conn = Connection::open(dir.join("catalog.sqlite"))?;
        schema::configure(&conn)?;
        schema::migrate(&mut conn)?;
        let thumbs = ThumbStore::open(&dir.join("thumbs.sqlite"))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            conn,
            thumbs,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Images matching `q`, ordered by capture time then file name.
    pub fn images(&self, q: &Query) -> Result<Vec<ImageRow>> {
        let mut sql = format!("{IMAGE_SELECT} WHERE i.rating >= ?1");
        let mut args: Vec<rusqlite::types::Value> = vec![(q.min_rating as i64).into()];
        if let Some(fid) = q.folder_id {
            if q.recursive {
                sql += " AND f.folder_id IN (SELECT c.id FROM folders c, folders p WHERE p.id = ?2 AND c.root_id = p.root_id
                          AND (p.rel_path = '' OR c.rel_path = p.rel_path OR c.rel_path LIKE p.rel_path || '/%'))";
            } else {
                sql += " AND f.folder_id = ?2";
            }
            args.push(fid.into());
        }
        if let Some(flag) = q.flag {
            sql += &format!(" AND i.flag = ?{}", args.len() + 1);
            args.push((flag as i64).into());
        }
        if let Some(label) = &q.label {
            sql += &format!(" AND i.label = ?{}", args.len() + 1);
            args.push(label.clone().into());
        }
        sql += " ORDER BY i.capture_time IS NULL, i.capture_time, f.name, i.id";
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), image_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn image(&self, id: i64) -> Result<Option<ImageRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("{IMAGE_SELECT} WHERE i.id = ?1"),
                [id],
                image_from_row,
            )
            .optional()?)
    }

    pub fn image_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0))?)
    }

    pub fn folders(&self) -> Result<Vec<FolderRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT fo.id, r.path, fo.rel_path,
                    (SELECT COUNT(*) FROM files f JOIN images i ON i.file_id = f.id WHERE f.folder_id = fo.id)
             FROM folders fo JOIN roots r ON r.id = fo.root_id ORDER BY r.path, fo.rel_path",
        )?;
        let rows = stmt.query_map([], |r| {
            let root: String = r.get(1)?;
            let rel: String = r.get(2)?;
            Ok(FolderRow {
                id: r.get(0)?,
                path: join_path(&root, &rel, ""),
                root_path: root,
                rel_path: rel,
                image_count: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_rating(&mut self, ids: &[i64], rating: u8) -> Result<()> {
        if rating > 5 {
            return Err(CatalogError::Invalid(format!(
                "rating {rating} out of range"
            )));
        }
        self.update_each(
            ids,
            "UPDATE images SET rating = ?2 WHERE id = ?1",
            rating as i64,
        )
    }

    pub fn set_flag(&mut self, ids: &[i64], flag: Flag) -> Result<()> {
        self.update_each(
            ids,
            "UPDATE images SET flag = ?2 WHERE id = ?1",
            flag as i64,
        )
    }

    pub fn set_label(&mut self, ids: &[i64], label: Option<&str>) -> Result<()> {
        if let Some(l) = label {
            if !LABELS.contains(&l) {
                return Err(CatalogError::Invalid(format!("unknown label {l}")));
            }
        }
        let tx = self.conn.transaction()?;
        for id in ids {
            tx.execute(
                "UPDATE images SET label = ?2 WHERE id = ?1",
                params![id, label],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn update_each(&mut self, ids: &[i64], sql: &str, value: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(sql)?;
            for id in ids {
                stmt.execute(params![id, value])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Grid thumbnail JPEG, generated and stored on first request if missing.
    pub fn thumbnail(&self, id: i64) -> Result<Option<Vec<u8>>> {
        // Unedited photos show the camera preview; edited ones a render with their settings.
        let settings = self.develop_settings(id)?;
        let d = pe_develop::digest(&settings);
        let wanted = if d == pe_develop::digest(&Settings::default()) {
            pe_preview::EMBEDDED.to_string()
        } else {
            d
        };
        if let Some((jpeg, have)) = self.thumbs.get_with_digest(id)? {
            if have == wanted {
                return Ok(Some(jpeg));
            }
        }
        let Some(img) = self.image(id)? else {
            return Ok(None);
        };
        let path = Path::new(&img.path);
        let made = if wanted == pe_preview::EMBEDDED {
            pe_preview::make_thumbnail(path, img.orientation)
        } else {
            // Rendered with the edits; formats the pipeline cannot render (e.g. JPEG, for now) fall back
            // to the plain image.
            pe_preview::make_rendered_thumbnail(path, &settings)
                .or_else(|_| pe_preview::make_thumbnail(path, img.orientation))
        };
        match made {
            Ok((w, h, jpeg)) => {
                self.thumbs.put(id, w, h, &jpeg, &wanted)?;
                Ok(Some(jpeg))
            }
            Err(_) => Ok(None),
        }
    }

    pub(crate) fn thumbs(&self) -> &ThumbStore {
        &self.thumbs
    }

    /// SQLite integrity check of the catalog database ("ok" when healthy).
    pub fn integrity_check(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?)
    }
}
