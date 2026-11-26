//! Lens profiles: distortion, lateral chromatic aberration (TCA) and vignetting calibrations for a
//! lens at a focal length and aperture.
//!
//! The calibrations come from the Lensfun lens database, which is distributed with this program as
//! data under its own licence (CC BY-SA 3.0, see `data/lensfun/README.md`); this crate only reads
//! it. The models and conventions used (our reading of the database's documentation):
//!
//! - **Distortion** maps an undistorted (corrected) radius to the distorted radius found in the
//!   photo, `r_d = r_u * f(r_u)`: `ptlens` `f = a r³ + b r² + c r + 1 - a - b - c`, `poly3`
//!   `f = 1 - k1 + k1 r²`, `poly5` `f = 1 + k1 r² + k2 r⁴`. Radii are normalised so that 1 is half
//!   the shorter side of the calibration camera's frame (whose size follows from the lens's crop
//!   factor and aspect ratio, 3:2 if not given).
//! - **TCA** maps the (distorted) green radius to the red / blue radius, `r_c = r (v + c r + b r²)`
//!   (`poly3`) or `r_c = r k` (`linear`), in the same units.
//! - **Vignetting** (`pa`): the photo is darker by `1 + k1 r² + k2 r⁴ + k3 r⁶`, with r normalised so
//!   that 1 is the corner of the calibration frame.
//!
//! Coefficients are interpolated linearly between the nearest calibrated focal lengths, and for
//! vignetting also between apertures (in stops), using the farthest calibrated focus distance.

mod db;
mod matching;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub use db::{Camera, Database, Lens};

/// A lens profile resolved for one photo. Radii are measured in units of the photo's half-diagonal
/// and converted with `dist_scale` / `vig_scale`, so the profile is independent of image size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Profile {
    /// Distortion factor `f(r) = d[0] + d[1] r + d[2] r² + d[3] r³ + d[4] r⁴` (identity: 1, 0, ...).
    pub distortion: [f32; 5],
    /// Red and blue radius factors `v + c r + b r²` as `[v, c, b]` (identity: 1, 0, 0).
    pub tca_red: [f32; 3],
    pub tca_blue: [f32; 3],
    /// Vignetting `1 + k1 r² + k2 r⁴ + k3 r⁶` (identity: zeros).
    pub vignetting: [f32; 3],
    pub has_distortion: bool,
    pub has_tca: bool,
    pub has_vignetting: bool,
    /// Normalised radius per unit of the photo's half-diagonal, for distortion / TCA and vignetting.
    pub dist_scale: f32,
    pub vig_scale: f32,
}

impl Profile {
    pub const IDENTITY: Profile = Profile {
        distortion: [1.0, 0.0, 0.0, 0.0, 0.0],
        tca_red: [1.0, 0.0, 0.0],
        tca_blue: [1.0, 0.0, 0.0],
        vignetting: [0.0; 3],
        has_distortion: false,
        has_tca: false,
        has_vignetting: false,
        dist_scale: 1.0,
        vig_scale: 1.0,
    };

    /// Distortion factor at normalised radius `r`.
    #[inline]
    pub fn distortion_factor(&self, r: f32) -> f32 {
        let d = &self.distortion;
        d[0] + r * (d[1] + r * (d[2] + r * (d[3] + r * d[4])))
    }

    /// Fall-off (≤ 1 usually) at normalised vignetting radius `r`; the correction divides by it.
    #[inline]
    pub fn falloff(&self, r: f32) -> f32 {
        let r2 = r * r;
        let k = &self.vignetting;
        1.0 + r2 * (k[0] + r2 * (k[1] + r2 * k[2]))
    }
}

/// What is known about the photo.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shot {
    pub camera_make: String,
    pub camera_model: String,
    pub lens: String,
    /// Millimetres; 0 when unknown.
    pub focal_length: f32,
    /// f-number; 0 when unknown.
    pub aperture: f32,
}

/// A matched lens and its profile for a shot.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// Lens maker and model as named in the database.
    pub name: String,
    pub profile: Profile,
}

impl Database {
    /// Find the lens used for `shot` and interpolate its calibration. `None` when the lens is not
    /// in the database (or has no calibration).
    pub fn lookup(&self, shot: &Shot) -> Option<Match> {
        let camera = matching::find_camera(self, &shot.camera_make, &shot.camera_model);
        let lens = matching::find_lens(self, camera, &shot.lens, shot.focal_length)?;
        let focal = if shot.focal_length > 0.0 {
            shot.focal_length
        } else {
            lens.single_focal()?
        };
        let mut p = lens.profile_at(focal, shot.aperture);
        if !(p.has_distortion || p.has_tca || p.has_vignetting) {
            return None;
        }
        let cam_crop = camera
            .map(|c| c.crop)
            .filter(|c| *c > 0.0)
            .unwrap_or(lens.crop);
        let ratio = lens.crop / cam_crop;
        // Image half-diagonal -> calibration half-shorter-side / half-diagonal units.
        p.dist_scale = ratio * (1.0 + lens.aspect * lens.aspect).sqrt();
        p.vig_scale = ratio;
        Some(Match {
            name: if lens
                .model
                .to_lowercase()
                .starts_with(&lens.maker.to_lowercase())
            {
                lens.model.clone()
            } else {
                format!("{} {}", lens.maker, lens.model).trim().to_string()
            },
            profile: p,
        })
    }
}

static DB_DIR: OnceLock<PathBuf> = OnceLock::new();
static DB: OnceLock<Option<Database>> = OnceLock::new();
type CacheKey = (String, String, String, u32, u32);
static CACHE: Mutex<Vec<(CacheKey, Option<Match>)>> = Mutex::new(Vec::new());

/// Use the database in `dir` (call before the first lookup; later calls are ignored). Without it,
/// `PE_LENS_DB` or the repository's `data/lensfun` are used.
pub fn set_database_dir(dir: impl AsRef<Path>) {
    let _ = DB_DIR.set(dir.as_ref().to_path_buf());
}

fn database_dir() -> PathBuf {
    if let Some(d) = DB_DIR.get() {
        return d.clone();
    }
    if let Ok(d) = std::env::var("PE_LENS_DB") {
        return PathBuf::from(d);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/lensfun")
}

/// The shared database (loaded on first use); `None` if it cannot be found.
pub fn database() -> Option<&'static Database> {
    DB.get_or_init(|| {
        Database::load_dir(&database_dir())
            .ok()
            .filter(|d| !d.lenses.is_empty())
    })
    .as_ref()
}

/// Look `shot` up in the shared database, with a small cache.
pub fn lookup(shot: &Shot) -> Option<Match> {
    if shot.lens.trim().is_empty() {
        return None;
    }
    let key = (
        shot.camera_make.clone(),
        shot.camera_model.clone(),
        shot.lens.clone(),
        shot.focal_length.to_bits(),
        shot.aperture.to_bits(),
    );
    if let Some((_, m)) = CACHE.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return m.clone();
    }
    let m = database().and_then(|db| db.lookup(shot));
    let mut c = CACHE.lock().unwrap();
    if c.len() > 64 {
        c.remove(0);
    }
    c.push((key, m.clone()));
    m
}
