//! Matching a photo's camera and lens names against the database.
//!
//! Names are compared as tokens: runs of letters, and numbers (`f/2.8`, `F2.8` and `2.8` all give the
//! number 2.8; `35mm` gives 35 and "mm"). A database lens qualifies when every number in its name
//! appears in the photo's lens name (focal lengths and apertures must agree), its mount fits the
//! camera (when the camera is known), and the photo's focal length is in its range; the best is the
//! one sharing the most words, both ways.

use crate::{Camera, Database, Lens};

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Num(f32),
}

fn tokens(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let b: Vec<char> = s.to_lowercase().chars().collect();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            let st = i;
            while i < b.len()
                && (b[i].is_ascii_digit()
                    || (b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()))
            {
                i += 1;
            }
            let t: String = b[st..i].iter().collect();
            if let Ok(v) = t.parse() {
                out.push(Tok::Num(v));
            }
        } else if c.is_alphabetic() {
            let st = i;
            while i < b.len() && b[i].is_alphabetic() {
                i += 1;
            }
            let mut w: String = b[st..i].iter().collect();
            // "mm" straight after a number is a unit ("35mmF2"); "f" before a number is the aperture
            // prefix.
            if st > 0 && b[st - 1].is_ascii_digit() && w.starts_with("mm") {
                w.drain(..2);
            }
            if !w.is_empty() && w != "f" {
                out.push(Tok::Word(w));
            }
        } else {
            i += 1;
        }
    }
    out
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(crate) fn find_camera<'a>(db: &'a Database, make: &str, model: &str) -> Option<&'a Camera> {
    let (mk, md) = (norm(make), norm(model));
    if md.is_empty() {
        return None;
    }
    let maker_ok = |c: &Camera| {
        mk.is_empty()
            || c.maker.iter().any(|m| {
                let m = norm(m);
                !m.is_empty() && (m.contains(&mk) || mk.contains(&m))
            })
    };
    // Exact model name, else a database name that ends with the photo's model ("Pentax K-1").
    db.cameras
        .iter()
        .filter(|c| maker_ok(c))
        .find(|c| c.model.iter().any(|m| norm(m) == md))
        .or_else(|| {
            db.cameras
                .iter()
                .filter(|c| maker_ok(c))
                .find(|c| c.model.iter().any(|m| norm(m).ends_with(&md)))
        })
}

fn mount_fits(db: &Database, camera: &Camera, lens: &Lens) -> bool {
    let cm = &camera.mount;
    let compat = db.compat.get(cm);
    lens.mounts
        .iter()
        .any(|m| m == cm || compat.is_some_and(|c| c.contains(m)))
}

/// Score of database name `name` against the photo's lens tokens; `None` if a number disagrees.
fn score(photo: &[Tok], name: &str) -> Option<f32> {
    let t = tokens(name);
    let nums: Vec<f32> = t
        .iter()
        .filter_map(|x| if let Tok::Num(v) = x { Some(*v) } else { None })
        .collect();
    if nums.is_empty() {
        return None;
    }
    let has_num = |v: f32| {
        photo
            .iter()
            .any(|p| matches!(p, Tok::Num(u) if (u - v).abs() < 1e-3))
    };
    if !nums.iter().all(|&v| has_num(v)) {
        return None;
    }
    let words: Vec<&String> = t
        .iter()
        .filter_map(|x| if let Tok::Word(w) = x { Some(w) } else { None })
        .collect();
    let pwords: Vec<&String> = photo
        .iter()
        .filter_map(|x| if let Tok::Word(w) = x { Some(w) } else { None })
        .collect();
    let a = words.iter().filter(|w| pwords.contains(w)).count() as f32 / words.len().max(1) as f32;
    let b = pwords.iter().filter(|w| words.contains(w)).count() as f32 / pwords.len().max(1) as f32;
    // Database words mostly present in the photo's name (allowing for brand prefixes like "smc").
    if !words.is_empty() && a < 0.5 {
        return None;
    }
    // Version marks must agree (an "II" lens is a different design from the "III").
    const VERSIONS: [&str; 5] = ["ii", "iii", "iv", "v", "vi"];
    let ver = |ws: &[&String]| -> Vec<String> {
        ws.iter()
            .filter(|w| VERSIONS.contains(&w.as_str()))
            .map(|w| w.to_string())
            .collect()
    };
    if ver(&words) != ver(&pwords) {
        return None;
    }
    Some(a + 0.5 * b + 0.01 * nums.len() as f32)
}

pub(crate) fn find_lens<'a>(
    db: &'a Database,
    camera: Option<&Camera>,
    lens_name: &str,
    focal: f32,
) -> Option<&'a Lens> {
    let photo = tokens(lens_name);
    let mut best: Option<(f32, &Lens)> = None;
    for l in &db.lenses {
        if l.dist.is_empty() && l.tca.is_empty() && l.vig.is_empty() {
            continue;
        }
        if let Some(c) = camera {
            if !mount_fits(db, c, l) {
                continue;
            }
        }
        if focal > 0.0 {
            if let Some((lo, hi)) = l.focal_range() {
                if focal < lo * 0.95 - 0.5 || focal > hi * 1.05 + 0.5 {
                    continue;
                }
            }
        }
        // Names may lack the maker ("XF35mmF2 R WR"); also try "maker model".
        let s = l
            .names
            .iter()
            .flat_map(|n| [score(&photo, n), score(&photo, &format!("{} {n}", l.maker))])
            .flatten()
            .fold(None::<f32>, |a, b| Some(a.map_or(b, |a| a.max(b))));
        if let Some(s) = s {
            if best.is_none_or(|(bs, _)| s > bs) {
                best = Some((s, l));
            }
        }
    }
    best.map(|(_, l)| l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenises_apertures_and_focal_lengths() {
        assert_eq!(
            tokens("XF35mmF2 R WR"),
            vec![
                Tok::Word("xf".into()),
                Tok::Num(35.0),
                Tok::Num(2.0),
                Tok::Word("r".into()),
                Tok::Word("wr".into())
            ]
        );
        assert_eq!(tokens("f/2.8L"), vec![Tok::Num(2.8), Tok::Word("l".into())]);
        assert_eq!(tokens("70-200mm"), vec![Tok::Num(70.0), Tok::Num(200.0)]);
    }
}
