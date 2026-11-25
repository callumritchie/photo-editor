//! Output file names and collision handling.
//!
//! Template tokens (this project's own syntax):
//! `{name}` original file name without extension, `{seq}` / `{seq:4}` sequence number (zero-padded),
//! `{date}` capture date YYYY-MM-DD, `{year}` `{month}` `{day}`, `{time}` HHMMSS, `{camera}` model.
//! Any other text is kept literally. Characters not allowed in file names become '-'.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::exif::PhotoMeta;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Collision {
    /// Add "-1", "-2", … to the name.
    #[default]
    Rename,
    Overwrite,
    Skip,
}

pub fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '\0') || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let t = cleaned.trim().trim_start_matches('.').to_string();
    if t.is_empty() {
        "untitled".into()
    } else {
        t
    }
}

pub fn expand(template: &str, source: &Path, meta: &PhotoMeta, seq: usize) -> String {
    let name = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let ct = meta.capture_time.as_deref().unwrap_or("");
    let part = |a: usize, b: usize, fallback: &str| {
        ct.get(a..b)
            .map(str::to_owned)
            .unwrap_or_else(|| fallback.into())
    };
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let token = &rest[start + 1..start + end];
        let (key, arg) = token.split_once(':').unwrap_or((token, ""));
        let value = match key {
            "name" => name.to_string(),
            "seq" => format!("{:0width$}", seq, width = arg.parse().unwrap_or(1)),
            "date" => {
                if ct.len() >= 10 {
                    ct[..10].to_string()
                } else {
                    "undated".into()
                }
            }
            "year" => part(0, 4, "0000"),
            "month" => part(5, 7, "00"),
            "day" => part(8, 10, "00"),
            "time" => format!(
                "{}{}{}",
                part(11, 13, "00"),
                part(14, 16, "00"),
                part(17, 19, "00")
            ),
            "camera" => meta.model.clone().unwrap_or_else(|| "camera".into()),
            _ => format!("{{{token}}}"),
        };
        out.push_str(&value);
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    sanitize(&out)
}

/// Final path for `stem.ext` in `dir` under the collision policy; `None` means skip.
pub fn resolve(dir: &Path, stem: &str, ext: &str, collision: Collision) -> Option<PathBuf> {
    let file = |s: &str| {
        if ext.is_empty() {
            dir.join(s)
        } else {
            dir.join(format!("{s}.{ext}"))
        }
    };
    let first = file(stem);
    if !first.exists() {
        return Some(first);
    }
    match collision {
        Collision::Overwrite => Some(first),
        Collision::Skip => None,
        Collision::Rename => (1..100_000)
            .map(|n| file(&format!("{stem}-{n}")))
            .find(|p| !p.exists()),
    }
}

/// Write via a hidden temporary file and rename, so a crash never leaves a half-written export.
pub fn write_atomic(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("export");
    let tmp = target.with_file_name(format!(".{name}.pe-partial"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_expand() {
        let meta = PhotoMeta {
            capture_time: Some("2021-06-26T22:17:11".into()),
            model: Some("D750".into()),
            ..Default::default()
        };
        let src = Path::new("/x/DSC_0042.NEF");
        assert_eq!(expand("{name}", src, &meta, 1), "DSC_0042");
        assert_eq!(expand("{date}_{seq:4}", src, &meta, 7), "2021-06-26_0007");
        assert_eq!(
            expand("{year}/{camera}-{time}", src, &meta, 1),
            "2021-D750-221711"
        );
        assert_eq!(expand("trip {unknown}", src, &meta, 1), "trip {unknown}");
        assert_eq!(expand("{date}", src, &PhotoMeta::default(), 1), "undated");
        assert_eq!(sanitize("../.."), "-..");
        assert_eq!(sanitize("  "), "untitled");
    }

    #[test]
    fn collisions() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("a.jpg"), b"x").unwrap();
        assert_eq!(
            resolve(t.path(), "a", "jpg", Collision::Rename).unwrap(),
            t.path().join("a-1.jpg")
        );
        assert_eq!(
            resolve(t.path(), "a", "jpg", Collision::Overwrite).unwrap(),
            t.path().join("a.jpg")
        );
        assert!(resolve(t.path(), "a", "jpg", Collision::Skip).is_none());
        assert_eq!(
            resolve(t.path(), "b", "jpg", Collision::Skip).unwrap(),
            t.path().join("b.jpg")
        );
    }
}
