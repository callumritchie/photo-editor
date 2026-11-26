//! Reading the database XML and interpolating calibrations.

use std::collections::HashMap;
use std::path::Path;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use crate::Profile;

#[derive(Debug, Clone, Default)]
pub struct Camera {
    pub maker: Vec<String>,
    pub model: Vec<String>,
    pub mount: String,
    pub crop: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DistEntry {
    pub focal: f32,
    /// Polynomial coefficients of the factor, see [`Profile::distortion`].
    pub d: [f32; 5],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TcaEntry {
    pub focal: f32,
    pub red: [f32; 3],
    pub blue: [f32; 3],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VigEntry {
    pub focal: f32,
    pub aperture: f32,
    pub distance: f32,
    pub k: [f32; 3],
}

#[derive(Debug, Clone, Default)]
pub struct Lens {
    pub maker: String,
    /// Primary model name (first `<model>`).
    pub model: String,
    /// All names (including translations).
    pub names: Vec<String>,
    pub mounts: Vec<String>,
    pub crop: f32,
    /// Long edge / short edge of the calibration frame.
    pub aspect: f32,
    pub(crate) dist: Vec<DistEntry>,
    pub(crate) tca: Vec<TcaEntry>,
    pub(crate) vig: Vec<VigEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct Database {
    pub cameras: Vec<Camera>,
    pub lenses: Vec<Lens>,
    /// Mount name -> mounts whose lenses also fit it.
    pub compat: HashMap<String, Vec<String>>,
}

fn attr(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name.as_bytes())
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

fn num(e: &BytesStart, name: &str, default: f32) -> f32 {
    attr(e, name)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn parse_aspect(s: &str) -> Option<f32> {
    let s = s.trim();
    let v = if let Some((a, b)) = s.split_once(':') {
        a.trim().parse::<f32>().ok()? / b.trim().parse::<f32>().ok()?
    } else {
        s.parse().ok()?
    };
    (v > 0.0).then_some(if v < 1.0 { 1.0 / v } else { v })
}

impl Database {
    /// Load every `*.xml` file in `dir`.
    pub fn load_dir(dir: &Path) -> std::io::Result<Database> {
        let mut db = Database::default();
        let mut files: Vec<_> = std::fs::read_dir(dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "xml"))
            .collect();
        files.sort();
        for f in files {
            let text = std::fs::read_to_string(&f)?;
            db.add_xml(&text);
        }
        Ok(db)
    }

    /// Add the contents of one database file (malformed parts are skipped).
    pub fn add_xml(&mut self, text: &str) {
        #[derive(PartialEq)]
        enum In {
            None,
            Mount,
            Camera,
            Lens,
        }
        let mut r = Reader::from_str(text);
        let mut in_ = In::None;
        let mut path: Vec<String> = Vec::new();
        let mut cam = Camera::default();
        let mut lens = Lens::default();
        let mut mount_name = String::new();
        let mut mount_compat: Vec<String> = Vec::new();
        let mut lang = false;
        loop {
            let ev = match r.read_event() {
                Ok(Event::Eof) | Err(_) => break,
                Ok(e) => e,
            };
            match ev {
                Event::Start(e) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                    lang = attr(&e, "lang").is_some();
                    match (path.len(), name.as_str()) {
                        (1, "mount") => {
                            in_ = In::Mount;
                            mount_name.clear();
                            mount_compat.clear();
                        }
                        (1, "camera") => {
                            in_ = In::Camera;
                            cam = Camera::default();
                        }
                        (1, "lens") => {
                            in_ = In::Lens;
                            lens = Lens {
                                crop: 1.0,
                                aspect: 1.5,
                                ..Default::default()
                            };
                        }
                        _ => {}
                    }
                    path.push(name);
                }
                Event::Empty(e) => {
                    if in_ == In::Lens && path.last().map(String::as_str) == Some("calibration") {
                        lens.add_calibration(&e);
                    }
                }
                Event::Text(t) => {
                    let Ok(v) = t.unescape() else { continue };
                    let v = v.trim().to_string();
                    if v.is_empty() {
                        continue;
                    }
                    let tag = path.last().map(String::as_str).unwrap_or("");
                    match (&in_, tag) {
                        (In::Mount, "name") => mount_name = v,
                        (In::Mount, "compat") => mount_compat.push(v),
                        (In::Camera, "maker") => cam.maker.push(v),
                        (In::Camera, "model") => cam.model.push(v),
                        (In::Camera, "mount") => cam.mount = v,
                        (In::Camera, "cropfactor") => cam.crop = v.parse().unwrap_or(0.0),
                        (In::Lens, "maker") => {
                            if lens.maker.is_empty() && !lang {
                                lens.maker = v
                            }
                        }
                        (In::Lens, "model") => {
                            if lens.model.is_empty() && !lang {
                                lens.model = v.clone();
                            }
                            lens.names.push(v);
                        }
                        (In::Lens, "mount") => lens.mounts.push(v),
                        (In::Lens, "cropfactor") => lens.crop = v.parse().unwrap_or(1.0),
                        (In::Lens, "aspect-ratio") => lens.aspect = parse_aspect(&v).unwrap_or(1.5),
                        _ => {}
                    }
                }
                Event::End(_) => {
                    if path.len() == 2 {
                        match in_ {
                            In::Mount => {
                                if !mount_name.is_empty() {
                                    self.compat
                                        .entry(mount_name.clone())
                                        .or_default()
                                        .append(&mut mount_compat);
                                }
                            }
                            In::Camera => {
                                if !cam.model.is_empty() {
                                    self.cameras.push(std::mem::take(&mut cam));
                                }
                            }
                            In::Lens => {
                                if lens.model.is_empty() {
                                    lens.model = lens.names.first().cloned().unwrap_or_default();
                                }
                                if !lens.model.is_empty() && lens.crop > 0.0 {
                                    self.lenses.push(std::mem::take(&mut lens));
                                }
                            }
                            In::None => {}
                        }
                        in_ = In::None;
                    }
                    path.pop();
                }
                _ => {}
            }
        }
    }
}

impl Lens {
    fn add_calibration(&mut self, e: &BytesStart) {
        let tag = e.name();
        let model = attr(e, "model").unwrap_or_default();
        let focal = num(e, "focal", 0.0);
        if focal <= 0.0 {
            return;
        }
        match tag.as_ref() {
            b"distortion" => {
                let d = match model.as_str() {
                    "ptlens" => {
                        let (a, b, c) = (num(e, "a", 0.0), num(e, "b", 0.0), num(e, "c", 0.0));
                        [1.0 - a - b - c, c, b, a, 0.0]
                    }
                    "poly3" => {
                        let k1 = num(e, "k1", 0.0);
                        [1.0 - k1, 0.0, k1, 0.0, 0.0]
                    }
                    "poly5" => [1.0, 0.0, num(e, "k1", 0.0), 0.0, num(e, "k2", 0.0)],
                    _ => return,
                };
                self.dist.push(DistEntry { focal, d });
            }
            b"tca" => {
                let (red, blue) = match model.as_str() {
                    "poly3" => (
                        [num(e, "vr", 1.0), num(e, "cr", 0.0), num(e, "br", 0.0)],
                        [num(e, "vb", 1.0), num(e, "cb", 0.0), num(e, "bb", 0.0)],
                    ),
                    "linear" => ([num(e, "kr", 1.0), 0.0, 0.0], [num(e, "kb", 1.0), 0.0, 0.0]),
                    _ => return,
                };
                self.tca.push(TcaEntry { focal, red, blue });
            }
            b"vignetting" if model == "pa" => {
                let aperture = num(e, "aperture", 0.0);
                if aperture > 0.0 {
                    self.vig.push(VigEntry {
                        focal,
                        aperture,
                        distance: num(e, "distance", 1000.0),
                        k: [num(e, "k1", 0.0), num(e, "k2", 0.0), num(e, "k3", 0.0)],
                    });
                }
            }
            _ => {}
        }
    }

    /// The focal length of a lens calibrated at only one focal length (a prime).
    pub fn single_focal(&self) -> Option<f32> {
        let mut f: Vec<f32> = self
            .dist
            .iter()
            .map(|d| d.focal)
            .chain(self.tca.iter().map(|t| t.focal))
            .chain(self.vig.iter().map(|v| v.focal))
            .collect();
        f.sort_by(f32::total_cmp);
        f.dedup();
        (f.len() == 1).then(|| f[0])
    }

    /// Calibrated focal lengths (sorted, distinct) of the entries.
    pub fn focal_range(&self) -> Option<(f32, f32)> {
        let all = self
            .dist
            .iter()
            .map(|d| d.focal)
            .chain(self.vig.iter().map(|v| v.focal));
        let (mut lo, mut hi) = (f32::MAX, 0.0f32);
        for f in all {
            lo = lo.min(f);
            hi = hi.max(f);
        }
        (hi > 0.0).then_some((lo, hi))
    }

    /// The calibration interpolated at `focal` mm and f/`aperture` (0 = unknown: no vignetting).
    pub(crate) fn profile_at(&self, focal: f32, aperture: f32) -> Profile {
        let mut p = Profile::IDENTITY;
        if let Some(d) = interp_focal(&self.dist, focal, |e| e.focal, |e| e.d.to_vec()) {
            p.distortion.copy_from_slice(&d);
            p.has_distortion = true;
        }
        if let Some(t) = interp_focal(&self.tca, focal, |e| e.focal, |e| [e.red, e.blue].concat()) {
            p.tca_red.copy_from_slice(&t[..3]);
            p.tca_blue.copy_from_slice(&t[3..]);
            p.has_tca = true;
        }
        if aperture > 0.0 && !self.vig.is_empty() {
            // Farthest focus distance per (focal, aperture).
            let mut best: Vec<VigEntry> = Vec::new();
            for v in &self.vig {
                match best
                    .iter_mut()
                    .find(|b| b.focal == v.focal && b.aperture == v.aperture)
                {
                    Some(b) if v.distance > b.distance => *b = *v,
                    Some(_) => {}
                    None => best.push(*v),
                }
            }
            let mut focals: Vec<f32> = best.iter().map(|v| v.focal).collect();
            focals.sort_by(f32::total_cmp);
            focals.dedup();
            // At each calibrated focal length, interpolate over aperture (in stops).
            let at_focal: Vec<(f32, Vec<f32>)> = focals
                .iter()
                .filter_map(|&f| {
                    let entries: Vec<&VigEntry> = best.iter().filter(|v| v.focal == f).collect();
                    let k = interp_by(
                        &entries,
                        aperture.log2(),
                        |e| e.aperture.log2(),
                        |e| e.k.to_vec(),
                    )?;
                    Some((f, k))
                })
                .collect();
            if let Some(k) = interp_by(&at_focal, focal, |e| e.0, |e| e.1.clone()) {
                p.vignetting.copy_from_slice(&k);
                p.has_vignetting = true;
            }
        }
        p
    }
}

fn interp_focal<T>(
    entries: &[T],
    focal: f32,
    key: impl Fn(&T) -> f32,
    val: impl Fn(&T) -> Vec<f32>,
) -> Option<Vec<f32>> {
    let refs: Vec<&T> = entries.iter().collect();
    interp_by(&refs, focal, |e| key(e), |e| val(e))
}

/// Linear interpolation of `val` at `x` between the nearest entries below and above (by `key`),
/// clamped to the end entries.
fn interp_by<T>(
    entries: &[T],
    x: f32,
    key: impl Fn(&T) -> f32,
    val: impl Fn(&T) -> Vec<f32>,
) -> Option<Vec<f32>> {
    let mut lo: Option<&T> = None;
    let mut hi: Option<&T> = None;
    for e in entries {
        let k = key(e);
        if k <= x && lo.is_none_or(|l| k > key(l)) {
            lo = Some(e);
        }
        if k >= x && hi.is_none_or(|h| k < key(h)) {
            hi = Some(e);
        }
    }
    match (lo, hi) {
        (Some(a), Some(b)) => {
            let (ka, kb) = (key(a), key(b));
            let t = if kb > ka { (x - ka) / (kb - ka) } else { 0.0 };
            Some(
                val(a)
                    .iter()
                    .zip(val(b))
                    .map(|(u, v)| u + t * (v - u))
                    .collect(),
            )
        }
        (Some(a), None) => Some(val(a)),
        (None, Some(b)) => Some(val(b)),
        (None, None) => None,
    }
}
