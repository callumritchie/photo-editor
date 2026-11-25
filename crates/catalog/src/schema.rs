//! Catalog database schema and migrations.
//!
//! Conventions (this project's own):
//! - Integer primary keys plus a UUID for anything that may be referenced from outside the database.
//! - Typed columns with CHECK constraints; foreign keys enforced.
//! - Event times are UTC milliseconds since the Unix epoch. Capture time is the camera's *local* wall-clock
//!   time as ISO-8601 text, with the UTC offset stored separately when known.
//! - Paths are stored as a root (absolute) plus a folder path relative to it, using '/' separators, so
//!   a whole tree can be relocated by updating one row.

use rusqlite::Connection;

pub const SCHEMA_VERSION: i64 = 2;

const V1: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE roots (
    id   INTEGER PRIMARY KEY,
    uuid TEXT NOT NULL UNIQUE,
    path TEXT NOT NULL UNIQUE
);

CREATE TABLE folders (
    id       INTEGER PRIMARY KEY,
    uuid     TEXT NOT NULL UNIQUE,
    root_id  INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,              -- '' for the root itself, else 'a/b'
    UNIQUE (root_id, rel_path)
);

CREATE TABLE files (
    id         INTEGER PRIMARY KEY,
    uuid       TEXT NOT NULL UNIQUE,
    folder_id  INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    size       INTEGER NOT NULL,
    mtime_ms   INTEGER NOT NULL,
    quick_hash TEXT NOT NULL,            -- lowercase name : size : mtime seconds (duplicate detection)
    UNIQUE (folder_id, name)
);
CREATE INDEX files_quick_hash ON files (quick_hash);

CREATE TABLE images (
    id             INTEGER PRIMARY KEY,
    uuid           TEXT NOT NULL UNIQUE,
    file_id        INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    imported_at    INTEGER NOT NULL,     -- UTC ms
    capture_time   TEXT,                 -- local 'YYYY-MM-DDTHH:MM:SS'
    capture_offset TEXT,                 -- e.g. '+01:00'
    rating         INTEGER NOT NULL DEFAULT 0 CHECK (rating BETWEEN 0 AND 5),
    flag           INTEGER NOT NULL DEFAULT 0 CHECK (flag IN (-1, 0, 1)),   -- -1 rejected, 1 picked
    label          TEXT CHECK (label IN ('red', 'yellow', 'green', 'blue', 'purple')),
    orientation    INTEGER NOT NULL DEFAULT 1 CHECK (orientation BETWEEN 1 AND 8),
    camera_make    TEXT,
    camera_model   TEXT,
    lens           TEXT,
    iso            INTEGER,
    exposure_time  REAL,
    f_number       REAL,
    focal_length   REAL
);
CREATE INDEX images_file ON images (file_id);
CREATE INDEX images_capture ON images (capture_time);

CREATE TABLE imports (
    id          INTEGER PRIMARY KEY,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,
    mode        TEXT NOT NULL
);

CREATE TABLE import_images (
    import_id INTEGER NOT NULL REFERENCES imports(id) ON DELETE CASCADE,
    image_id  INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    PRIMARY KEY (import_id, image_id)
);
"#;

/// v2: non-destructive editing. Settings are canonical JSON (see `pe_develop::canonical_json`).
const V2: &str = r#"
CREATE TABLE develop (
    image_id     INTEGER PRIMARY KEY REFERENCES images(id) ON DELETE CASCADE,
    settings     TEXT NOT NULL,
    digest       TEXT NOT NULL,
    current_step INTEGER,                -- history.id the current settings came from (NULL = none)
    updated_at   INTEGER NOT NULL
);

CREATE TABLE history (
    id         INTEGER PRIMARY KEY,
    image_id   INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    label      TEXT NOT NULL,
    settings   TEXT NOT NULL,
    digest     TEXT NOT NULL
);
CREATE INDEX history_image ON history (image_id, id);

CREATE TABLE snapshots (
    id         INTEGER PRIMARY KEY,
    uuid       TEXT NOT NULL UNIQUE,
    image_id   INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    settings   TEXT NOT NULL,
    digest     TEXT NOT NULL
);
CREATE INDEX snapshots_image ON snapshots (image_id);

CREATE TABLE presets (
    id         INTEGER PRIMARY KEY,
    uuid       TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL UNIQUE,
    groups     TEXT NOT NULL,            -- JSON array of group names
    settings   TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#;

/// Connection-level settings applied on every open.
pub fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )
}

/// Create or upgrade the schema to [`SCHEMA_VERSION`].
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(V1)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('created_by', ?1)",
            [concat!("photo-editor ", env!("CARGO_PKG_VERSION"))],
        )?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
    }
    if version < 2 {
        let tx = conn.transaction()?;
        tx.execute_batch(V2)?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
    }
    Ok(())
}
