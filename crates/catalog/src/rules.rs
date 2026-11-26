//! Rules for smart collections and library filters, compiled to SQL.
//!
//! A rule set matches `all`, `any` or `none` of its rules; a rule is either a condition on one field or
//! a nested rule set. Smart collections store a rule set as JSON and are evaluated when queried, so they
//! are always up to date. Example:
//!
//! ```json
//! {"match": "all", "rules": [
//!   {"field": "rating", "op": "ge", "value": 3},
//!   {"match": "any", "rules": [
//!     {"field": "keyword", "op": "is", "value": "Birds"},
//!     {"field": "camera", "op": "contains", "value": "X-T3"}]}]}
//! ```
//!
//! The SQL refers to the aliases of the image query: `i` (images), `f` (files), `fo` (folders) and
//! `r` (roots). Every value is passed as a bound parameter.

use rusqlite::types::Value;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Match {
    #[default]
    All,
    Any,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RuleSet {
    #[serde(rename = "match", default)]
    pub matching: Match,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// Order matters for deserialising: a condition requires `field` and `op`, while every field of a
/// rule set has a default (so any object would parse as an empty group).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Rule {
    Condition(Condition),
    Group(RuleSet),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Field {
    Rating,
    /// "picked", "rejected" or "unflagged".
    Flag,
    /// A colour name or "none".
    Label,
    Keyword,
    /// File name, camera, lens, keywords or copy name.
    Text,
    FileName,
    /// "raw" or "jpeg" (by extension).
    FileType,
    Folder,
    Camera,
    Lens,
    Iso,
    FocalLength,
    Aperture,
    /// Exposure time in seconds.
    ShutterSpeed,
    /// "YYYY-MM-DD" values; `inLast` takes a number of days.
    CaptureDate,
    /// Has develop settings other than the defaults.
    Edited,
    VirtualCopy,
    /// Member of a collection (by id; smart collections are expanded).
    Collection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Op {
    Is,
    IsNot,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Lt,
    Le,
    Gt,
    Ge,
    Between,
    InLast,
    Empty,
    NotEmpty,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    pub field: Field,
    pub op: Op,
    #[serde(default)]
    pub value: serde_json::Value,
    /// Upper bound for `between`.
    #[serde(default)]
    pub value2: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("operator {op:?} does not apply to {field:?}")]
    BadOp { field: Field, op: Op },
    #[error("{0:?} needs a value")]
    MissingValue(Field),
    #[error("rules nest too deeply (or smart collections refer to each other)")]
    TooDeep,
}

/// Context the compiler needs from the catalog.
pub trait RuleContext {
    /// Digest of default develop settings (for `edited`).
    fn default_digest(&self) -> String;
    /// Rules of a smart collection, or `None` for a static one (or unknown id).
    fn smart_rules(&self, collection_id: i64) -> Option<RuleSet>;
}

const MAX_DEPTH: usize = 8;

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

struct Compiler<'a> {
    args: &'a mut Vec<Value>,
    ctx: &'a dyn RuleContext,
}

impl Compiler<'_> {
    fn bind(&mut self, v: impl Into<Value>) -> String {
        self.args.push(v.into());
        format!("?{}", self.args.len())
    }

    fn set(&mut self, set: &RuleSet, depth: usize) -> Result<String, RuleError> {
        if depth > MAX_DEPTH {
            return Err(RuleError::TooDeep);
        }
        let parts: Vec<String> = set
            .rules
            .iter()
            .map(|r| match r {
                Rule::Group(g) => self.set(g, depth + 1),
                Rule::Condition(c) => self.condition(c, depth),
            })
            .collect::<Result<_, _>>()?;
        Ok(match (set.matching, parts.is_empty()) {
            (Match::All, true) | (Match::None, true) => "1".into(),
            (Match::Any, true) => "0".into(),
            (Match::All, false) => format!("({})", parts.join(" AND ")),
            (Match::Any, false) => format!("({})", parts.join(" OR ")),
            (Match::None, false) => format!("NOT ({})", parts.join(" OR ")),
        })
    }

    fn text_value(c: &Condition) -> Result<String, RuleError> {
        match &c.value {
            serde_json::Value::String(s) => Ok(s.clone()),
            serde_json::Value::Number(n) => Ok(n.to_string()),
            _ => Err(RuleError::MissingValue(c.field)),
        }
    }

    fn num_value(v: &serde_json::Value, f: Field) -> Result<f64, RuleError> {
        match v {
            serde_json::Value::Number(n) => n.as_f64().ok_or(RuleError::MissingValue(f)),
            serde_json::Value::String(s) => {
                s.trim().parse().map_err(|_| RuleError::MissingValue(f))
            }
            _ => Err(RuleError::MissingValue(f)),
        }
    }

    /// Text comparison of a (nullable) SQL expression.
    fn text(&mut self, expr: &str, c: &Condition) -> Result<String, RuleError> {
        let e = format!("COALESCE({expr}, '')");
        Ok(match c.op {
            Op::Empty => format!("{e} = ''"),
            Op::NotEmpty => format!("{e} <> ''"),
            Op::Is => format!("{e} = {} COLLATE NOCASE", self.bind(Self::text_value(c)?)),
            Op::IsNot => format!("{e} <> {} COLLATE NOCASE", self.bind(Self::text_value(c)?)),
            Op::Contains | Op::NotContains | Op::StartsWith | Op::EndsWith => {
                let v = like_escape(&Self::text_value(c)?);
                let pat = match c.op {
                    Op::StartsWith => format!("{v}%"),
                    Op::EndsWith => format!("%{v}"),
                    _ => format!("%{v}%"),
                };
                let not = if c.op == Op::NotContains { "NOT " } else { "" };
                format!("{e} {not}LIKE {} ESCAPE '\\'", self.bind(pat))
            }
            op => return Err(RuleError::BadOp { field: c.field, op }),
        })
    }

    /// Numeric comparison; images without a value never match (except `empty`).
    fn number(&mut self, expr: &str, c: &Condition) -> Result<String, RuleError> {
        let f = c.field;
        let sym = match c.op {
            Op::Is => "=",
            Op::IsNot => "<>",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Between => {
                let a = self.bind(Self::num_value(&c.value, f)?);
                let b = self.bind(Self::num_value(&c.value2, f)?);
                return Ok(format!("({expr} BETWEEN {a} AND {b})"));
            }
            Op::Empty => return Ok(format!("{expr} IS NULL")),
            Op::NotEmpty => return Ok(format!("{expr} IS NOT NULL")),
            op => return Err(RuleError::BadOp { field: f, op }),
        };
        let v = self.bind(Self::num_value(&c.value, f)?);
        Ok(format!("({expr} {sym} {v})"))
    }

    fn truthy(c: &Condition) -> Result<bool, RuleError> {
        let v = match &c.value {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Null => true,
            serde_json::Value::String(s) => s != "false",
            _ => true,
        };
        match c.op {
            Op::Is => Ok(v),
            Op::IsNot => Ok(!v),
            op => Err(RuleError::BadOp { field: c.field, op }),
        }
    }

    fn condition(&mut self, c: &Condition, depth: usize) -> Result<String, RuleError> {
        let bad = |op| RuleError::BadOp { field: c.field, op };
        Ok(match c.field {
            Field::Rating => self.number("i.rating", c)?,
            Field::Iso => self.number("i.iso", c)?,
            Field::FocalLength => self.number("i.focal_length", c)?,
            Field::Aperture => self.number("i.f_number", c)?,
            Field::ShutterSpeed => self.number("i.exposure_time", c)?,
            Field::FileName => self.text("f.name", c)?,
            Field::Lens => self.text("i.lens", c)?,
            Field::Camera => self.text(
                "TRIM(COALESCE(i.camera_make, '') || ' ' || COALESCE(i.camera_model, ''))",
                c,
            )?,
            Field::Folder => self.text("r.path || '/' || fo.rel_path", c)?,
            Field::Flag => {
                let v = match Self::text_value(c)?.as_str() {
                    "picked" => 1,
                    "rejected" => -1,
                    _ => 0,
                };
                match c.op {
                    Op::Is => format!("i.flag = {}", self.bind(v as i64)),
                    Op::IsNot => format!("i.flag <> {}", self.bind(v as i64)),
                    op => return Err(bad(op)),
                }
            }
            Field::Label => {
                let v = if matches!(c.op, Op::Is | Op::IsNot) {
                    Self::text_value(c)?
                } else {
                    String::new()
                };
                match (c.op, v.as_str()) {
                    (Op::Is, "none") | (Op::Empty, _) => "i.label IS NULL".into(),
                    (Op::IsNot, "none") | (Op::NotEmpty, _) => "i.label IS NOT NULL".into(),
                    (Op::Is, _) => format!("i.label = {}", self.bind(v)),
                    (Op::IsNot, _) => format!("COALESCE(i.label, '') <> {}", self.bind(v)),
                    (op, _) => return Err(bad(op)),
                }
            }
            Field::FileType => {
                let jpeg = "(LOWER(f.name) LIKE '%.jpg' OR LOWER(f.name) LIKE '%.jpeg')";
                let want_jpeg = Self::text_value(c)?.eq_ignore_ascii_case("jpeg");
                match (c.op, want_jpeg) {
                    (Op::Is, true) | (Op::IsNot, false) => jpeg.into(),
                    (Op::Is, false) | (Op::IsNot, true) => format!("NOT {jpeg}"),
                    (op, _) => return Err(bad(op)),
                }
            }
            Field::CaptureDate => {
                let d = "substr(i.capture_time, 1, 10)";
                match c.op {
                    Op::InLast => {
                        let days = Self::num_value(&c.value, c.field)?;
                        let off = self.bind(format!("-{} days", days.round() as i64));
                        format!("{d} >= date('now', {off})")
                    }
                    Op::Is | Op::IsNot | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                        let sym = match c.op {
                            Op::Is => "=",
                            Op::IsNot => "<>",
                            Op::Lt => "<",
                            Op::Le => "<=",
                            Op::Gt => ">",
                            _ => ">=",
                        };
                        format!("{d} {sym} {}", self.bind(Self::text_value(c)?))
                    }
                    Op::Between => {
                        let a = self.bind(Self::text_value(c)?);
                        let b = match &c.value2 {
                            serde_json::Value::String(s) => self.bind(s.clone()),
                            _ => return Err(RuleError::MissingValue(c.field)),
                        };
                        format!("({d} BETWEEN {a} AND {b})")
                    }
                    Op::Empty => "i.capture_time IS NULL".into(),
                    Op::NotEmpty => "i.capture_time IS NOT NULL".into(),
                    op => return Err(bad(op)),
                }
            }
            Field::Edited => {
                let digest = self.ctx.default_digest();
                let dd = self.bind(digest);
                let e = format!(
                    "EXISTS (SELECT 1 FROM develop d WHERE d.image_id = i.id AND d.digest <> {dd})"
                );
                if Self::truthy(c)? {
                    e
                } else {
                    format!("NOT {e}")
                }
            }
            Field::VirtualCopy => {
                if Self::truthy(c)? {
                    "i.master_id IS NOT NULL".into()
                } else {
                    "i.master_id IS NULL".into()
                }
            }
            Field::Keyword => {
                let has = "EXISTS (SELECT 1 FROM image_keywords ik WHERE ik.image_id = i.id)";
                match c.op {
                    Op::Empty => format!("NOT {has}"),
                    Op::NotEmpty => has.into(),
                    Op::Is | Op::IsNot => {
                        // The named keyword or any keyword below it.
                        let v = self.bind(Self::text_value(c)?);
                        let e = format!(
                            "EXISTS (SELECT 1 FROM image_keywords ik WHERE ik.image_id = i.id AND ik.keyword_id IN (
                               WITH RECURSIVE d(id) AS (
                                 SELECT id FROM keywords WHERE name = {v} COLLATE NOCASE
                                 UNION SELECT k.id FROM keywords k JOIN d ON k.parent_id = d.id)
                               SELECT id FROM d))"
                        );
                        if c.op == Op::Is {
                            e
                        } else {
                            format!("NOT {e}")
                        }
                    }
                    Op::Contains | Op::NotContains | Op::StartsWith | Op::EndsWith => {
                        let inner = self.text(
                            "k.name",
                            &Condition {
                                op: if c.op == Op::NotContains {
                                    Op::Contains
                                } else {
                                    c.op
                                },
                                ..c.clone()
                            },
                        )?;
                        let e = format!(
                            "EXISTS (SELECT 1 FROM image_keywords ik JOIN keywords k ON k.id = ik.keyword_id
                               WHERE ik.image_id = i.id AND {inner})"
                        );
                        if c.op == Op::NotContains {
                            format!("NOT {e}")
                        } else {
                            e
                        }
                    }
                    op => return Err(bad(op)),
                }
            }
            Field::Text => {
                if !matches!(c.op, Op::Contains | Op::NotContains) {
                    return Err(bad(c.op));
                }
                let positive = Condition {
                    op: Op::Contains,
                    ..c.clone()
                };
                let mut alts = vec![
                    self.text("f.name", &positive)?,
                    self.text("i.camera_model", &positive)?,
                    self.text("i.camera_make", &positive)?,
                    self.text("i.lens", &positive)?,
                    self.text("i.copy_name", &positive)?,
                ];
                let kw = self.text("k.name", &positive)?;
                alts.push(format!(
                    "EXISTS (SELECT 1 FROM image_keywords ik JOIN keywords k ON k.id = ik.keyword_id
                       WHERE ik.image_id = i.id AND {kw})"
                ));
                let e = format!("({})", alts.join(" OR "));
                if c.op == Op::NotContains {
                    format!("NOT {e}")
                } else {
                    e
                }
            }
            Field::Collection => {
                let id = Self::num_value(&c.value, c.field)? as i64;
                let e = match self.ctx.smart_rules(id) {
                    Some(rules) => self.set(&rules, depth + 1)?,
                    None => format!(
                        "EXISTS (SELECT 1 FROM collection_images ci WHERE ci.image_id = i.id AND ci.collection_id = {})",
                        self.bind(id)
                    ),
                };
                match c.op {
                    Op::Is => e,
                    Op::IsNot => format!("NOT {e}"),
                    op => return Err(bad(op)),
                }
            }
        })
    }
}

/// Compile `set` to an SQL boolean expression, appending its parameters to `args`.
pub fn compile(
    set: &RuleSet,
    args: &mut Vec<Value>,
    ctx: &dyn RuleContext,
) -> Result<String, RuleError> {
    Compiler { args, ctx }.set(set, 0)
}
