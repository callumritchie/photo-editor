//! Library organisation: collections and sets, smart collections, hierarchical keywords, virtual
//! copies and stacks.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::rules::{RuleContext, RuleSet};
use crate::time::now_ms;
use crate::{Catalog, CatalogError, DevelopCommand, Result, Settings};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CollectionKind {
    /// A container for collections and other sets.
    Set,
    /// A hand-picked group of photos.
    Collection,
    /// Photos matching rules, always up to date.
    Smart,
}

impl CollectionKind {
    fn as_str(self) -> &'static str {
        match self {
            CollectionKind::Set => "set",
            CollectionKind::Collection => "collection",
            CollectionKind::Smart => "smart",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "set" => CollectionKind::Set,
            "smart" => CollectionKind::Smart,
            _ => CollectionKind::Collection,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionRow {
    pub id: i64,
    pub uuid: String,
    pub parent_id: Option<i64>,
    pub kind: CollectionKind,
    pub name: String,
    pub rules: Option<RuleSet>,
    /// Photos in it (0 for sets).
    pub image_count: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeywordRow {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    /// Full path, e.g. "Animals > Birds > Owl".
    pub path: String,
    /// Photos with this keyword itself (not counting keywords below it).
    pub image_count: i64,
}

/// A collections table row: id, uuid, parent, kind, name, rules JSON.
type CollectionRecord = (i64, String, Option<i64>, String, String, Option<String>);

/// Separator for keyword paths when typing them.
pub const KEYWORD_SEPARATOR: char = '>';

impl RuleContext for Catalog {
    fn default_digest(&self) -> String {
        pe_develop::digest(&Settings::default())
    }

    fn smart_rules(&self, collection_id: i64) -> Option<RuleSet> {
        let r: Option<String> = self
            .conn
            .query_row(
                "SELECT rules FROM collections WHERE id = ?1 AND kind = 'smart'",
                [collection_id],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten();
        r.and_then(|s| serde_json::from_str(&s).ok())
    }
}

fn invalid(msg: impl Into<String>) -> CatalogError {
    CatalogError::Invalid(msg.into())
}

impl Catalog {
    // ---- collections ------------------------------------------------------------------------------

    /// All collections and sets, parents before children, siblings by name.
    pub fn collections(&self) -> Result<Vec<CollectionRow>> {
        let rows: Vec<CollectionRecord> = self
            .conn
            .prepare(
                "WITH RECURSIVE tree(id, sort) AS (
                   SELECT id, LOWER(name) FROM collections WHERE parent_id IS NULL
                   UNION ALL
                   SELECT c.id, tree.sort || char(1) || LOWER(c.name) FROM collections c JOIN tree ON c.parent_id = tree.id)
                 SELECT c.id, c.uuid, c.parent_id, c.kind, c.name, c.rules
                 FROM collections c JOIN tree ON tree.id = c.id ORDER BY tree.sort",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(id, uuid, parent_id, kind, name, rules)| {
                let kind = CollectionKind::parse(&kind);
                let rules: Option<RuleSet> = rules.and_then(|s| serde_json::from_str(&s).ok());
                let image_count = match kind {
                    CollectionKind::Set => 0,
                    CollectionKind::Collection => self.conn.query_row(
                        "SELECT COUNT(*) FROM collection_images WHERE collection_id = ?1",
                        [id],
                        |r| r.get(0),
                    )?,
                    CollectionKind::Smart => {
                        self.count_matching(rules.as_ref().unwrap_or(&RuleSet::default()))?
                    }
                };
                Ok(CollectionRow {
                    id,
                    uuid,
                    parent_id,
                    kind,
                    name,
                    rules,
                    image_count,
                })
            })
            .collect()
    }

    fn count_matching(&self, rules: &RuleSet) -> Result<i64> {
        let mut args = Vec::new();
        let cond =
            crate::rules::compile(rules, &mut args, self).map_err(|e| invalid(e.to_string()))?;
        Ok(self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM images i JOIN files f ON f.id = i.file_id JOIN folders fo ON fo.id = f.folder_id
                 JOIN roots r ON r.id = fo.root_id WHERE {cond}"
            ),
            rusqlite::params_from_iter(args),
            |r| r.get(0),
        )?)
    }

    fn check_parent(&self, parent_id: Option<i64>) -> Result<()> {
        if let Some(p) = parent_id {
            let kind: Option<String> = self
                .conn
                .query_row("SELECT kind FROM collections WHERE id = ?1", [p], |r| {
                    r.get(0)
                })
                .optional()?;
            if kind.as_deref() != Some("set") {
                return Err(invalid(
                    "collections can only be placed inside a collection set",
                ));
            }
        }
        Ok(())
    }

    fn check_name_free(
        &self,
        parent_id: Option<i64>,
        name: &str,
        except: Option<i64>,
    ) -> Result<()> {
        let clash: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM collections WHERE parent_id IS ?1 AND name = ?2 COLLATE NOCASE
               AND id IS NOT ?3)",
            params![parent_id, name, except],
            |r| r.get(0),
        )?;
        if clash {
            return Err(invalid(format!(
                "there is already a collection named \"{name}\" here"
            )));
        }
        Ok(())
    }

    pub fn create_collection(
        &mut self,
        name: &str,
        kind: CollectionKind,
        parent_id: Option<i64>,
        rules: Option<&RuleSet>,
    ) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("a collection needs a name"));
        }
        self.check_parent(parent_id)?;
        self.check_name_free(parent_id, name, None)?;
        let rules_json = match (kind, rules) {
            (CollectionKind::Smart, Some(r)) => {
                // Validate by compiling once.
                crate::rules::compile(r, &mut Vec::new(), self)
                    .map_err(|e| invalid(e.to_string()))?;
                Some(serde_json::to_string(r).map_err(|e| invalid(e.to_string()))?)
            }
            (CollectionKind::Smart, None) => {
                Some(serde_json::to_string(&RuleSet::default()).unwrap_or_default())
            }
            _ => None,
        };
        self.conn.execute(
            "INSERT INTO collections (uuid, parent_id, kind, name, rules, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![uuid::Uuid::new_v4().to_string(), parent_id, kind.as_str(), name, rules_json, now_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_collection(&mut self, id: i64, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("a collection needs a name"));
        }
        let parent: Option<i64> = self.conn.query_row(
            "SELECT parent_id FROM collections WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?;
        self.check_name_free(parent, name, Some(id))?;
        self.conn.execute(
            "UPDATE collections SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Move a collection or set into another set (or to the top level).
    pub fn move_collection(&mut self, id: i64, parent_id: Option<i64>) -> Result<()> {
        self.check_parent(parent_id)?;
        if let Some(p) = parent_id {
            // Not into itself or its own descendants.
            let cycle: bool = self.conn.query_row(
                "WITH RECURSIVE d(id) AS (SELECT ?1 UNION SELECT c.id FROM collections c JOIN d ON c.parent_id = d.id)
                 SELECT EXISTS (SELECT 1 FROM d WHERE id = ?2)",
                params![id, p],
                |r| r.get(0),
            )?;
            if cycle {
                return Err(invalid("a set cannot be moved inside itself"));
            }
        }
        let name: String =
            self.conn
                .query_row("SELECT name FROM collections WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?;
        self.check_name_free(parent_id, &name, Some(id))?;
        self.conn.execute(
            "UPDATE collections SET parent_id = ?2 WHERE id = ?1",
            params![id, parent_id],
        )?;
        Ok(())
    }

    pub fn set_smart_rules(&mut self, id: i64, rules: &RuleSet) -> Result<()> {
        crate::rules::compile(rules, &mut Vec::new(), self).map_err(|e| invalid(e.to_string()))?;
        let previous: Option<String> = self
            .conn
            .query_row("SELECT rules FROM collections WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        let n = self.conn.execute(
            "UPDATE collections SET rules = ?2 WHERE id = ?1 AND kind = 'smart'",
            params![
                id,
                serde_json::to_string(rules).map_err(|e| invalid(e.to_string()))?
            ],
        )?;
        if n == 0 {
            return Err(invalid("not a smart collection"));
        }
        // Reject rules that (through collection references) refer back to this collection.
        let probe = RuleSet {
            rules: vec![crate::rules::Rule::Condition(crate::rules::Condition {
                field: crate::rules::Field::Collection,
                op: crate::rules::Op::Is,
                value: id.into(),
                value2: serde_json::Value::Null,
            })],
            ..Default::default()
        };
        if crate::rules::compile(&probe, &mut Vec::new(), self).is_err() {
            self.conn.execute(
                "UPDATE collections SET rules = ?2 WHERE id = ?1",
                params![id, previous],
            )?;
            return Err(invalid("a smart collection cannot include itself"));
        }
        Ok(())
    }

    /// Delete a collection or set (and everything in a set). Photos are not affected.
    pub fn delete_collection(&mut self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM collections WHERE id = ?1", [id])?;
        Ok(())
    }

    fn require_static(&self, id: i64) -> Result<()> {
        let kind: Option<String> = self
            .conn
            .query_row("SELECT kind FROM collections WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        match kind.as_deref() {
            Some("collection") => Ok(()),
            Some(_) => Err(invalid("photos can only be added to a regular collection")),
            None => Err(invalid("no such collection")),
        }
    }

    pub fn add_to_collection(&mut self, id: i64, image_ids: &[i64]) -> Result<()> {
        self.require_static(id)?;
        let tx = self.conn.transaction()?;
        let now = now_ms();
        for img in image_ids {
            tx.execute(
                "INSERT OR IGNORE INTO collection_images (collection_id, image_id, added_at) VALUES (?1, ?2, ?3)",
                params![id, img, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_from_collection(&mut self, id: i64, image_ids: &[i64]) -> Result<()> {
        self.require_static(id)?;
        let tx = self.conn.transaction()?;
        for img in image_ids {
            tx.execute(
                "DELETE FROM collection_images WHERE collection_id = ?1 AND image_id = ?2",
                params![id, img],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- keywords ---------------------------------------------------------------------------------

    /// All keywords, parents before children, with paths and direct photo counts.
    pub fn keywords(&self) -> Result<Vec<KeywordRow>> {
        Ok(self
            .conn
            .prepare(
                "WITH RECURSIVE tree(id, path, sort) AS (
                   SELECT id, name, LOWER(name) FROM keywords WHERE parent_id IS NULL
                   UNION ALL
                   SELECT k.id, tree.path || ' > ' || k.name, tree.sort || char(1) || LOWER(k.name)
                   FROM keywords k JOIN tree ON k.parent_id = tree.id)
                 SELECT k.id, k.parent_id, k.name, tree.path,
                        (SELECT COUNT(*) FROM image_keywords ik WHERE ik.keyword_id = k.id)
                 FROM keywords k JOIN tree ON tree.id = k.id ORDER BY tree.sort",
            )?
            .query_map([], |r| {
                Ok(KeywordRow {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    name: r.get(2)?,
                    path: r.get(3)?,
                    image_count: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Id of the keyword at `path` ("Animals > Birds"), creating missing levels.
    pub fn keyword_for_path(&mut self, path: &str) -> Result<i64> {
        let parts: Vec<&str> = path
            .split(KEYWORD_SEPARATOR)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            return Err(invalid("empty keyword"));
        }
        let mut parent: Option<i64> = None;
        for name in parts {
            let existing: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM keywords WHERE parent_id IS ?1 AND name = ?2 COLLATE NOCASE",
                    params![parent, name],
                    |r| r.get(0),
                )
                .optional()?;
            parent = Some(match existing {
                Some(id) => id,
                None => {
                    self.conn.execute(
                        "INSERT INTO keywords (uuid, parent_id, name) VALUES (?1, ?2, ?3)",
                        params![uuid::Uuid::new_v4().to_string(), parent, name],
                    )?;
                    self.conn.last_insert_rowid()
                }
            });
        }
        Ok(parent.expect("at least one part"))
    }

    /// Add keywords (paths; missing ones are created) to photos.
    pub fn add_keywords(&mut self, image_ids: &[i64], paths: &[String]) -> Result<()> {
        let ids: Vec<i64> = paths
            .iter()
            .map(|p| self.keyword_for_path(p))
            .collect::<Result<_>>()?;
        let tx = self.conn.transaction()?;
        for img in image_ids {
            for k in &ids {
                tx.execute(
                    "INSERT OR IGNORE INTO image_keywords (image_id, keyword_id) VALUES (?1, ?2)",
                    params![img, k],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_keyword(&mut self, image_ids: &[i64], keyword_id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        for img in image_ids {
            tx.execute(
                "DELETE FROM image_keywords WHERE image_id = ?1 AND keyword_id = ?2",
                params![img, keyword_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Keywords on any of the given photos, with how many of them have each (`image_count`).
    pub fn keywords_of(&self, image_ids: &[i64]) -> Result<Vec<KeywordRow>> {
        let all = self.keywords()?;
        let mut counts = std::collections::HashMap::<i64, i64>::new();
        let mut stmt = self
            .conn
            .prepare("SELECT keyword_id FROM image_keywords WHERE image_id = ?1")?;
        for img in image_ids {
            for k in stmt.query_map([img], |r| r.get::<_, i64>(0))? {
                *counts.entry(k?).or_default() += 1;
            }
        }
        Ok(all
            .into_iter()
            .filter_map(|mut k| {
                counts.get(&k.id).map(|n| {
                    k.image_count = *n;
                    k
                })
            })
            .collect())
    }

    pub fn rename_keyword(&mut self, id: i64, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() || name.contains(KEYWORD_SEPARATOR) {
            return Err(invalid("invalid keyword name"));
        }
        self.conn.execute(
            "UPDATE keywords SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Delete a keyword and those below it (removing them from photos).
    pub fn delete_keyword(&mut self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM keywords WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- virtual copies ---------------------------------------------------------------------------

    /// A new virtual copy of a photo: another entry for the same file, starting from the current
    /// develop settings, with its own history from then on. Returns its id.
    pub fn create_virtual_copy(&mut self, id: i64) -> Result<i64> {
        let master: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(master_id, id) FROM images WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| invalid("no such photo"))?;
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM images WHERE master_id = ?1",
            [master],
            |r| r.get(0),
        )?;
        let settings = self.develop_settings(id)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO images (uuid, file_id, imported_at, capture_time, capture_offset, rating, flag, label,
                 orientation, camera_make, camera_model, lens, iso, exposure_time, f_number, focal_length,
                 master_id, copy_name)
             SELECT ?1, file_id, ?2, capture_time, capture_offset, rating, flag, label, orientation, camera_make,
                 camera_model, lens, iso, exposure_time, f_number, focal_length, ?3, ?4
             FROM images WHERE id = ?5",
            params![uuid::Uuid::new_v4().to_string(), now_ms(), master, format!("Copy {}", n + 1), id],
        )?;
        let copy = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO image_keywords (image_id, keyword_id) SELECT ?1, keyword_id FROM image_keywords WHERE image_id = ?2",
            params![copy, id],
        )?;
        tx.execute(
            "INSERT INTO collection_images (collection_id, image_id, added_at)
             SELECT collection_id, ?1, ?2 FROM collection_images WHERE image_id = ?3",
            params![copy, now_ms(), id],
        )?;
        tx.commit()?;
        if settings != Settings::default() {
            self.develop_execute(
                &[copy],
                &DevelopCommand::Set {
                    settings,
                    label: Some("Virtual copy".into()),
                },
            )?;
        }
        // Keep the copy next to its original when it is stacked.
        let stack: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT stack_id, stack_position FROM images WHERE id = ?1 AND stack_id IS NOT NULL",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((sid, _)) = stack {
            let pos: i64 = self.conn.query_row(
                "SELECT COALESCE(MAX(stack_position), -1) + 1 FROM images WHERE stack_id = ?1",
                [sid],
                |r| r.get(0),
            )?;
            self.conn.execute(
                "UPDATE images SET stack_id = ?2, stack_position = ?3 WHERE id = ?1",
                params![copy, sid, pos],
            )?;
        }
        Ok(copy)
    }

    /// Remove photos from the library (never from disk). Removing an original also removes its
    /// virtual copies; files left with no photos are forgotten so they can be imported again.
    pub fn remove_images(&mut self, image_ids: &[i64]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        for id in image_ids {
            n += tx.execute("DELETE FROM images WHERE id = ?1", [id])?;
        }
        tx.execute(
            "DELETE FROM files WHERE id NOT IN (SELECT file_id FROM images)",
            [],
        )?;
        tx.execute("DELETE FROM stacks WHERE id NOT IN (SELECT stack_id FROM images WHERE stack_id IS NOT NULL)", [])?;
        tx.commit()?;
        for id in image_ids {
            let _ = self.thumbs().remove(*id);
            let _ = self.previews.remove(*id);
        }
        self.tidy_stacks()?;
        Ok(n)
    }

    // ---- stacks -----------------------------------------------------------------------------------

    /// Group photos into one stack, in the given order (the first is the top). Photos leave any stack
    /// they were in. Returns the stack id.
    pub fn stack(&mut self, image_ids: &[i64]) -> Result<i64> {
        if image_ids.len() < 2 {
            return Err(invalid("a stack needs at least two photos"));
        }
        let tx = self.conn.transaction()?;
        tx.execute("INSERT INTO stacks (collapsed) VALUES (1)", [])?;
        let sid = tx.last_insert_rowid();
        for (pos, id) in image_ids.iter().enumerate() {
            tx.execute(
                "UPDATE images SET stack_id = ?2, stack_position = ?3 WHERE id = ?1",
                params![id, sid, pos as i64],
            )?;
        }
        tx.commit()?;
        self.tidy_stacks()?;
        Ok(sid)
    }

    /// Take photos out of their stacks.
    pub fn unstack(&mut self, image_ids: &[i64]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for id in image_ids {
            tx.execute(
                "UPDATE images SET stack_id = NULL, stack_position = NULL WHERE id = ?1",
                [id],
            )?;
        }
        tx.commit()?;
        self.tidy_stacks()
    }

    /// Make a photo the top of its stack.
    pub fn set_stack_top(&mut self, image_id: i64) -> Result<()> {
        let sid: Option<i64> = self.conn.query_row(
            "SELECT stack_id FROM images WHERE id = ?1",
            [image_id],
            |r| r.get(0),
        )?;
        let Some(sid) = sid else {
            return Err(invalid("not in a stack"));
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE images SET stack_position = stack_position + 1 WHERE stack_id = ?1",
            [sid],
        )?;
        tx.execute(
            "UPDATE images SET stack_position = 0 WHERE id = ?1",
            [image_id],
        )?;
        tx.commit()?;
        self.tidy_stacks()
    }

    pub fn set_stack_collapsed(&mut self, stack_id: i64, collapsed: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE stacks SET collapsed = ?2 WHERE id = ?1",
            params![stack_id, collapsed as i64],
        )?;
        Ok(())
    }

    /// Dissolve stacks with fewer than two photos and renumber positions 0, 1, 2, ...
    fn tidy_stacks(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE images SET stack_id = NULL, stack_position = NULL WHERE stack_id IN
               (SELECT stack_id FROM images WHERE stack_id IS NOT NULL GROUP BY stack_id HAVING COUNT(*) < 2)",
            [],
        )?;
        tx.execute(
            "DELETE FROM stacks WHERE id NOT IN (SELECT stack_id FROM images WHERE stack_id IS NOT NULL)",
            [],
        )?;
        // Renumber from the current order (computed first: an SQL UPDATE would see its own changes).
        let members: Vec<(i64, i64)> = tx
            .prepare(
                "SELECT id, stack_id FROM images WHERE stack_id IS NOT NULL
                 ORDER BY stack_id, stack_position, id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut last = None;
        let mut pos = 0i64;
        for (id, sid) in members {
            pos = if last == Some(sid) { pos + 1 } else { 0 };
            last = Some(sid);
            tx.execute(
                "UPDATE images SET stack_position = ?2 WHERE id = ?1",
                params![id, pos],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// A library operation as JSON, for the app and the command line, e.g.
/// `{"command": "addKeywords", "imageIds": [1, 2], "paths": ["Animals > Birds"]}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "command",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LibraryCommand {
    Collections,
    CreateCollection {
        name: String,
        kind: CollectionKind,
        #[serde(default)]
        parent_id: Option<i64>,
        #[serde(default)]
        rules: Option<RuleSet>,
    },
    RenameCollection {
        id: i64,
        name: String,
    },
    MoveCollection {
        id: i64,
        #[serde(default)]
        parent_id: Option<i64>,
    },
    SetSmartRules {
        id: i64,
        rules: RuleSet,
    },
    DeleteCollection {
        id: i64,
    },
    AddToCollection {
        id: i64,
        image_ids: Vec<i64>,
    },
    RemoveFromCollection {
        id: i64,
        image_ids: Vec<i64>,
    },
    Keywords,
    KeywordsOf {
        image_ids: Vec<i64>,
    },
    AddKeywords {
        image_ids: Vec<i64>,
        paths: Vec<String>,
    },
    RemoveKeyword {
        image_ids: Vec<i64>,
        keyword_id: i64,
    },
    RenameKeyword {
        id: i64,
        name: String,
    },
    DeleteKeyword {
        id: i64,
    },
    CreateVirtualCopy {
        image_id: i64,
    },
    RemoveImages {
        image_ids: Vec<i64>,
    },
    Stack {
        image_ids: Vec<i64>,
    },
    Unstack {
        image_ids: Vec<i64>,
    },
    SetStackTop {
        image_id: i64,
    },
    SetStackCollapsed {
        stack_id: i64,
        collapsed: bool,
    },
}

impl Catalog {
    /// Run a [`LibraryCommand`]; the result is JSON (lists, a new id, or `{"ok": true}`).
    pub fn library_execute(&mut self, cmd: &LibraryCommand) -> Result<serde_json::Value> {
        use serde_json::json;
        let ok = json!({ "ok": true });
        let to = |v: &dyn erased::Ser| v.to_json();
        Ok(match cmd {
            LibraryCommand::Collections => to(&self.collections()?),
            LibraryCommand::CreateCollection {
                name,
                kind,
                parent_id,
                rules,
            } => {
                json!({ "id": self.create_collection(name, *kind, *parent_id, rules.as_ref())? })
            }
            LibraryCommand::RenameCollection { id, name } => {
                self.rename_collection(*id, name)?;
                ok
            }
            LibraryCommand::MoveCollection { id, parent_id } => {
                self.move_collection(*id, *parent_id)?;
                ok
            }
            LibraryCommand::SetSmartRules { id, rules } => {
                self.set_smart_rules(*id, rules)?;
                ok
            }
            LibraryCommand::DeleteCollection { id } => {
                self.delete_collection(*id)?;
                ok
            }
            LibraryCommand::AddToCollection { id, image_ids } => {
                self.add_to_collection(*id, image_ids)?;
                ok
            }
            LibraryCommand::RemoveFromCollection { id, image_ids } => {
                self.remove_from_collection(*id, image_ids)?;
                ok
            }
            LibraryCommand::Keywords => to(&self.keywords()?),
            LibraryCommand::KeywordsOf { image_ids } => to(&self.keywords_of(image_ids)?),
            LibraryCommand::AddKeywords { image_ids, paths } => {
                self.add_keywords(image_ids, paths)?;
                ok
            }
            LibraryCommand::RemoveKeyword {
                image_ids,
                keyword_id,
            } => {
                self.remove_keyword(image_ids, *keyword_id)?;
                ok
            }
            LibraryCommand::RenameKeyword { id, name } => {
                self.rename_keyword(*id, name)?;
                ok
            }
            LibraryCommand::DeleteKeyword { id } => {
                self.delete_keyword(*id)?;
                ok
            }
            LibraryCommand::CreateVirtualCopy { image_id } => {
                json!({ "id": self.create_virtual_copy(*image_id)? })
            }
            LibraryCommand::RemoveImages { image_ids } => {
                json!({ "removed": self.remove_images(image_ids)? })
            }
            LibraryCommand::Stack { image_ids } => json!({ "id": self.stack(image_ids)? }),
            LibraryCommand::Unstack { image_ids } => {
                self.unstack(image_ids)?;
                ok
            }
            LibraryCommand::SetStackTop { image_id } => {
                self.set_stack_top(*image_id)?;
                ok
            }
            LibraryCommand::SetStackCollapsed {
                stack_id,
                collapsed,
            } => {
                self.set_stack_collapsed(*stack_id, *collapsed)?;
                ok
            }
        })
    }
}

mod erased {
    pub trait Ser {
        fn to_json(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Ser for T {
        fn to_json(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
        }
    }
}

impl Catalog {
    // ---- mask rasters -----------------------------------------------------------------------------

    /// Store a mask raster (8-bit grey, `width` x `height`) for a photo; returns its id.
    pub fn save_raster(
        &mut self,
        image_id: i64,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<String> {
        if data.len() != (width as usize) * (height as usize) || width == 0 || height == 0 {
            return Err(invalid("raster size does not match its data"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO mask_rasters (id, image_id, width, height, data, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, image_id, width, height, data, now_ms()],
        )?;
        Ok(id)
    }

    /// Rasters by id (missing ids are skipped).
    pub fn rasters(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, pe_pipeline::masks::Raster>> {
        let mut out = std::collections::HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT width, height, data FROM mask_rasters WHERE id = ?1")?;
        for id in ids {
            let row: Option<(i64, i64, Vec<u8>)> = stmt
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            if let Some((w, h, d)) = row {
                out.insert(
                    id.clone(),
                    pe_pipeline::masks::Raster {
                        width: w as usize,
                        height: h as usize,
                        data: d.iter().map(|v| *v as f32 / 255.0).collect(),
                    },
                );
            }
        }
        Ok(out)
    }

    /// Develop settings with the rasters their masks refer to attached (for rendering).
    pub fn render_settings(&self, image_id: i64) -> Result<Settings> {
        let mut s = self.develop_settings(image_id)?;
        self.attach_rasters(&mut s)?;
        Ok(s)
    }

    /// Load the rasters `s`'s masks refer to into `s.rasters`.
    pub fn attach_rasters(&self, s: &mut Settings) -> Result<()> {
        let ids: Vec<String> = s
            .masks
            .iter()
            .flat_map(|m| m.components.iter())
            .filter_map(|c| match &c.shape {
                pe_pipeline::masks::Shape::Raster { raster_id, .. } => Some(raster_id.clone()),
                _ => None,
            })
            .collect();
        if !ids.is_empty() {
            s.rasters = std::sync::Arc::new(self.rasters(&ids)?);
        }
        Ok(())
    }
}
