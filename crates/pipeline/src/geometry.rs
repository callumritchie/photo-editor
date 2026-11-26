//! Geometry: crop, straighten, perspective, and manual lens corrections.
//!
//! Design (this project's own). Every output pixel is mapped back to a position in the sensor-space
//! image through one inverse transform, then sampled bilinearly (black outside the frame):
//! 1. crop: the output covers the rectangle `crop` (fractions 0..1) of the corrected frame;
//! 2. straighten: rotate by -angle about the centre;
//! 3. perspective: a homography in coordinates where the frame spans -1..1, source = p / w / scale
//!    with w = 1 + kx x + ky y (Vertical / Horizontal tilt the plane; Scale zooms);
//! 4. orientation: the camera's EXIF rotation/flip, inverted;
//! 5. lens distortion: radial, source = centre + d (1 + k1 r^2), r relative to the half-diagonal;
//! 6. lateral chromatic aberration: red and blue sampled at slightly different radial scales.
//!
//! Resampling happens after all per-pixel processing (in sensor space), on both CPU and GPU, so both
//! apply the same nonlinear steps to the same values. Lens vignetting correction is a radial gain in
//! scene-linear light, applied with white balance.

use rayon::prelude::*;

use crate::Settings;

/// Perspective tilt at +/-100 (homography coefficient), distortion k1 at +/-100, chromatic
/// aberration radial scale at +/-100, lens vignetting at +/-100 (stops at the corners).
pub const PERSPECTIVE_MAX: f32 = 0.5;
pub const DISTORTION_MAX: f32 = 0.15;
pub const CA_MAX: f32 = 0.003;
pub const LENS_VIGNETTE_STOPS: f32 = 1.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeometryParams {
    /// Whether resampling is needed (otherwise only orientation applies).
    pub active: bool,
    /// Sensor-space frame size.
    pub src_w: usize,
    pub src_h: usize,
    /// EXIF orientation applied (1 = none).
    pub orientation: u8,
    /// Oriented (displayed) frame size.
    pub frame_w: usize,
    pub frame_h: usize,
    pub out_w: usize,
    pub out_h: usize,
    pub crop: [f32; 4],
    pub cos: f32,
    pub sin: f32,
    pub kx: f32,
    pub ky: f32,
    pub inv_scale: f32,
    pub k1: f32,
    /// Radial scale per channel (R, G, B) for chromatic aberration.
    pub ca: [f32; 3],
    /// Lens vignetting correction, stops at the corners.
    pub lens_vignette: f32,
    /// Lens profile (identity when off): distortion factor coefficients with the amount applied,
    /// red / blue TCA factors, and the normalised radius per sensor pixel.
    pub pdist: [f32; 5],
    pub ptca: [[f32; 3]; 2],
    pub pr_scale: f32,
    /// Zoom applied with the profile so the corrected picture fills the frame.
    pub pfit: f32,
    /// Profile vignetting `1 + k1 r² + k2 r⁴ + k3 r⁶`, radius per sensor pixel, and amount (0 = off).
    pub pvig: [f32; 3],
    pub pv_scale: f32,
    pub pv_amount: f32,
    /// The file's own lens correction (DNG WarpRectilinear), used instead of a profile.
    pub dwarp: Option<pe_raw::dng::WarpModel>,
}

const PDIST_IDENTITY: [f32; 5] = [1.0, 0.0, 0.0, 0.0, 0.0];
const PTCA_IDENTITY: [[f32; 3]; 2] = [[1.0, 0.0, 0.0]; 2];

impl GeometryParams {
    pub fn new(s: &Settings, src_w: usize, src_h: usize, orientation: u8) -> Self {
        let orientation = if s.apply_orientation && (2..=8).contains(&orientation) {
            orientation
        } else {
            1
        };
        let (frame_w, frame_h) = if orientation >= 5 {
            (src_h, src_w)
        } else {
            (src_w, src_h)
        };
        let c = &s.crop;
        let crop = [
            c.left.clamp(0.0, 1.0),
            c.top.clamp(0.0, 1.0),
            c.right.clamp(0.0, 1.0),
            c.bottom.clamp(0.0, 1.0),
        ];
        let (cw, ch) = ((crop[2] - crop[0]).max(1e-4), (crop[3] - crop[1]).max(1e-4));
        let t = &s.transform;
        let l = &s.lens;
        let angle = c.angle.to_radians();
        let ca = [
            1.0 + l.ca_red_cyan / 100.0 * CA_MAX,
            1.0,
            1.0 + l.ca_blue_yellow / 100.0 * CA_MAX,
        ];
        let active = crop != [0.0, 0.0, 1.0, 1.0]
            || angle != 0.0
            || t.vertical != 0.0
            || t.horizontal != 0.0
            || t.scale != 100.0
            || l.distortion != 0.0
            || ca != [1.0; 3];
        Self {
            active,
            src_w,
            src_h,
            orientation,
            frame_w,
            frame_h,
            out_w: ((frame_w as f32 * cw).round() as usize).max(1),
            out_h: ((frame_h as f32 * ch).round() as usize).max(1),
            crop,
            cos: angle.cos(),
            sin: angle.sin(),
            kx: -t.horizontal / 100.0 * PERSPECTIVE_MAX,
            ky: -t.vertical / 100.0 * PERSPECTIVE_MAX,
            inv_scale: 100.0 / t.scale.max(1.0),
            k1: -l.distortion / 100.0 * DISTORTION_MAX,
            ca,
            lens_vignette: l.vignetting / 100.0 * LENS_VIGNETTE_STOPS,
            pdist: PDIST_IDENTITY,
            ptca: PTCA_IDENTITY,
            pr_scale: 0.0,
            pfit: 1.0,
            pvig: [0.0; 3],
            pv_scale: 0.0,
            pv_amount: 0.0,
            dwarp: None,
        }
    }

    /// [`new`](Self::new) for a photo, with its lens profile when the settings turn it on and the
    /// lens is in the database. `src_w` x `src_h` is the whole frame's size.
    pub fn for_frame(s: &Settings, frame: &pe_raw::RawFrame, src_w: usize, src_h: usize) -> Self {
        let mut g = Self::new(s, src_w, src_h, frame.orientation);
        if let Some(w) = frame.dng_warp {
            g.dwarp = Some(w);
            g.active = true;
            g.pfit = g.fill_scale();
            return g;
        }
        match profile_for(s, frame) {
            Some(p) => g.with_profile(
                &p,
                s.lens.profile_distortion / 100.0,
                s.lens.profile_vignetting / 100.0,
            ),
            None => g,
        }
    }

    /// Apply a lens profile with the given amounts (1 = as calibrated).
    pub fn with_profile(mut self, p: &pe_lens::Profile, dist_amount: f32, vig_amount: f32) -> Self {
        let half_diag = 0.5 * (self.src_w as f32).hypot(self.src_h as f32);
        if p.has_distortion && dist_amount > 0.0 {
            for (k, d) in self.pdist.iter_mut().enumerate() {
                *d = PDIST_IDENTITY[k] + dist_amount * (p.distortion[k] - PDIST_IDENTITY[k]);
            }
        }
        if p.has_tca {
            self.ptca = [p.tca_red, p.tca_blue];
        }
        self.pr_scale = p.dist_scale / half_diag;
        if p.has_vignetting && vig_amount > 0.0 {
            self.pvig = p.vignetting;
            self.pv_scale = p.vig_scale / half_diag;
            self.pv_amount = vig_amount;
        }
        if self.pdist != PDIST_IDENTITY || self.ptca != PTCA_IDENTITY {
            self.active = true;
            self.pfit = self.fill_scale();
        }
        self
    }

    /// Profile distortion and TCA factor for a sensor offset (dx, dy) from the centre, channel c.
    #[inline]
    fn profile_factor(&self, dx: f32, dy: f32, c: usize) -> f32 {
        let r = (dx * dx + dy * dy).sqrt() * self.pr_scale;
        let d = &self.pdist;
        let f = d[0] + r * (d[1] + r * (d[2] + r * (d[3] + r * d[4])));
        if c == 1 {
            return f;
        }
        let t = &self.ptca[if c == 0 { 0 } else { 1 }];
        let rd = r * f;
        f * (t[0] + rd * (t[1] + rd * t[2]))
    }

    /// The lens correction (DNG warp, or the profile's distortion and TCA) for a sensor offset from
    /// the centre: the offset sampled instead.
    #[inline]
    fn lens_map(&self, ex: f32, ey: f32, c: usize) -> (f32, f32) {
        if let Some(w) = &self.dwarp {
            let (sw, sh) = (self.src_w as f32, self.src_h as f32);
            let (x, y) = w.source(sw * 0.5 + ex, sh * 0.5 + ey, c, sw, sh);
            return (x - sw * 0.5, y - sh * 0.5);
        }
        let g = self.profile_factor(ex, ey, c);
        (ex * g, ey * g)
    }

    /// Whether a lens correction (DNG warp or profile geometry) is applied.
    #[inline]
    fn has_lens_geometry(&self) -> bool {
        self.dwarp.is_some() || self.pdist != PDIST_IDENTITY || self.ptca != PTCA_IDENTITY
    }

    /// Largest zoom-out (between 0.5 and 1.5) for which the corrected frame's border still samples
    /// inside the photo.
    fn fill_scale(&self) -> f32 {
        let (hw, hh) = (self.src_w as f32 * 0.5, self.src_h as f32 * 0.5);
        let border: Vec<(f32, f32)> = (0..=32)
            .flat_map(|i| {
                let t = i as f32 / 32.0 * 2.0 - 1.0;
                [(t * hw, -hh), (t * hw, hh), (-hw, t * hh), (hw, t * hh)]
            })
            .collect();
        let fits = |s: f32| {
            border.iter().all(|&(x, y)| {
                let (mx, my) = self.lens_map(x * s, y * s, 1);
                mx.abs() <= hw * 1.0005 && my.abs() <= hh * 1.0005
            })
        };
        let (mut lo, mut hi) = (0.5f32, 1.5f32);
        if !fits(lo) {
            return 1.0;
        }
        for _ in 0..30 {
            let m = 0.5 * (lo + hi);
            if fits(m) {
                lo = m;
            } else {
                hi = m;
            }
        }
        lo
    }

    /// Output size when [`active`](Self::active); otherwise the oriented frame size.
    pub fn output_size(&self) -> (usize, usize) {
        if self.active {
            (self.out_w, self.out_h)
        } else {
            (self.frame_w, self.frame_h)
        }
    }

    /// Continuous sensor-space position (pixel edges at integers) sampled for output pixel (x, y),
    /// channel `c`.
    #[inline]
    pub fn source(&self, x: usize, y: usize, c: usize) -> (f32, f32) {
        self.source_at(x as f32, y as f32, c)
    }

    /// [`source`](Self::source) for a continuous output position (pixel indices; centres at +0.5).
    #[inline]
    pub fn source_at(&self, x: f32, y: f32, c: usize) -> (f32, f32) {
        let (fw, fh) = (self.frame_w as f32, self.frame_h as f32);
        // 1. crop -> corrected frame.
        let xt =
            (self.crop[0] + (x + 0.5) / self.out_w as f32 * (self.crop[2] - self.crop[0])) * fw;
        let yt =
            (self.crop[1] + (y + 0.5) / self.out_h as f32 * (self.crop[3] - self.crop[1])) * fh;
        // 2. straighten.
        let (cx, cy) = (xt - fw * 0.5, yt - fh * 0.5);
        let rx = cx * self.cos + cy * self.sin;
        let ry = -cx * self.sin + cy * self.cos;
        // 3. perspective.
        let (nx, ny) = (rx / (fw * 0.5), ry / (fh * 0.5));
        let w = 1.0 + self.kx * nx + self.ky * ny;
        let (px, py) = (
            nx / w * self.inv_scale * fw * 0.5 + fw * 0.5,
            ny / w * self.inv_scale * fh * 0.5 + fh * 0.5,
        );
        // 4. orientation (continuous form of orient::apply).
        let (sw, sh) = (self.src_w as f32, self.src_h as f32);
        let (sx, sy) = match self.orientation {
            2 => (sw - px, py),
            3 => (sw - px, sh - py),
            4 => (px, sh - py),
            5 => (py, px),
            6 => (py, sh - px),
            7 => (sw - py, sh - px),
            8 => (sw - py, px),
            _ => (px, py),
        };
        // 5-6. lens distortion and chromatic aberration.
        let (dx, dy) = (sx - sw * 0.5, sy - sh * 0.5);
        let r2 = (dx * dx + dy * dy) / (sw * sw * 0.25 + sh * sh * 0.25);
        let f = (1.0 + self.k1 * r2) * self.ca[c];
        let (mut ex, mut ey) = (dx * f, dy * f);
        // 7. lens correction (the file's own warp, or the profile): zoom to fill, then map.
        if self.has_lens_geometry() {
            (ex, ey) = self.lens_map(ex * self.pfit, ey * self.pfit, c);
        }
        (sw * 0.5 + ex, sh * 0.5 + ey)
    }

    /// Lens vignetting gain for sensor pixel (x, y).
    #[inline]
    pub fn lens_gain(&self, x: usize, y: usize) -> f32 {
        let (sw, sh) = (self.src_w as f32, self.src_h as f32);
        let (dx, dy) = (x as f32 + 0.5 - sw * 0.5, y as f32 + 0.5 - sh * 0.5);
        let r2 = (dx * dx + dy * dy) / (sw * sw * 0.25 + sh * sh * 0.25);
        let mut g = (self.lens_vignette * r2).exp2();
        if self.pv_amount > 0.0 {
            let rv2 = (dx * dx + dy * dy) * self.pv_scale * self.pv_scale;
            let k = &self.pvig;
            let fall = (1.0 + rv2 * (k[0] + rv2 * (k[1] + rv2 * k[2]))).max(0.05);
            g *= fall.powf(-self.pv_amount);
        }
        g
    }

    /// Whether [`lens_gain`](Self::lens_gain) is not 1 everywhere.
    pub fn has_lens_gain(&self) -> bool {
        self.lens_vignette != 0.0 || self.pv_amount > 0.0
    }
}

/// Bilinear sample of channel `c` of interleaved RGB at a continuous position (pixel centres at
/// integer + 0.5). Outside the frame: 0.
#[inline]
pub fn bilinear(img: &[f32], w: usize, h: usize, fx: f32, fy: f32, c: usize) -> f32 {
    if !(0.0..=w as f32).contains(&fx) || !(0.0..=h as f32).contains(&fy) {
        return 0.0;
    }
    let x = (fx - 0.5).clamp(0.0, (w - 1) as f32);
    let y = (fy - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (tx, ty) = (x - x0 as f32, y - y0 as f32);
    let at = |xx: usize, yy: usize| img[(yy * w + xx) * 3 + c];
    let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
    let bot = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
    top * (1.0 - ty) + bot * ty
}

/// Resample a sensor-space RGB image through the geometry; returns (width, height, data).
pub fn resample(img: &[f32], p: &GeometryParams) -> (usize, usize, Vec<f32>) {
    let (w, h) = (p.out_w, p.out_h);
    let mut out = vec![0.0; w * h * 3];
    out.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            for c in 0..3 {
                let (fx, fy) = p.source(x, y, c);
                row[x * 3 + c] = bilinear(img, p.src_w, p.src_h, fx, fy, c);
            }
        }
    });
    (w, h, out)
}

/// Resample the output rectangle `out` (x, y, w, h in whole-picture output coordinates) from a
/// sensor-space region whose pixels start at (`ctx.x0`, `ctx.y0`) of the whole frame. `p` describes
/// the whole picture; even with no geometry set, this applies the orientation.
pub fn resample_region(
    img: &[f32],
    rw: usize,
    rh: usize,
    ctx: &crate::region::Ctx,
    p: &GeometryParams,
    out: [usize; 4],
) -> (usize, usize, Vec<f32>) {
    let [ox, oy, w, h] = out;
    // With no geometry, the mapping uses the oriented frame as the output.
    let mut q = *p;
    if !q.active {
        q.out_w = q.frame_w;
        q.out_h = q.frame_h;
        q.crop = [0.0, 0.0, 1.0, 1.0];
    }
    let mut data = vec![0.0; w * h * 3];
    data.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            for c in 0..3 {
                let (fx, fy) = q.source(x + ox, y + oy, c);
                row[x * 3 + c] = bilinear(img, rw, rh, fx - ctx.x0 as f32, fy - ctx.y0 as f32, c);
            }
        }
    });
    (w, h, data)
}

impl GeometryParams {
    /// The mapping used for output coordinates: with no geometry set, the output is the oriented frame.
    pub fn output_mapping(&self) -> GeometryParams {
        let mut q = *self;
        if !q.active {
            q.out_w = q.frame_w;
            q.out_h = q.frame_h;
            q.crop = [0.0, 0.0, 1.0, 1.0];
        }
        q
    }

    /// Mask (oriented-frame) position, in fractions, of an output position given in fractions.
    pub fn output_to_frame(&self, u: f32, v: f32) -> (f32, f32) {
        let q = self.output_mapping();
        // Continuous output pixel position (source() takes pixel indices; subtract the centre offset).
        let (ox, oy) = (u * q.out_w as f32 - 0.5, v * q.out_h as f32 - 0.5);
        let (sx, sy) = q.source_at(ox, oy, 1);
        let (fx, fy) = crate::masks::frame_position_at(sx, sy, q.src_w, q.src_h, q.orientation);
        (fx / q.frame_w as f32, fy / q.frame_h as f32)
    }

    /// Output position (fractions) of a mask position (fractions): the inverse of
    /// [`output_to_frame`](Self::output_to_frame), solved with Newton's method. Positions outside the
    /// output come back outside 0..1.
    pub fn frame_to_output(&self, fx: f32, fy: f32) -> (f32, f32) {
        let q = self.output_mapping();
        // Start from the crop-relative position (exact when only cropping).
        let c = q.crop;
        let mut u = ((fx - c[0]) / (c[2] - c[0]).max(1e-6)) as f64;
        let mut v = ((fy - c[1]) / (c[3] - c[1]).max(1e-6)) as f64;
        if q.orientation != 1 || !q.active {
            u = fx as f64;
            v = fy as f64;
        }
        let f = |u: f64, v: f64| {
            let (a, b) = self.output_to_frame(u as f32, v as f32);
            (a as f64, b as f64)
        };
        for _ in 0..20 {
            let (a, b) = f(u, v);
            let (ex, ey) = (a - fx as f64, b - fy as f64);
            if ex.abs() < 1e-6 && ey.abs() < 1e-6 {
                break;
            }
            let h = 1e-3;
            let (a1, b1) = f(u + h, v);
            let (a2, b2) = f(u, v + h);
            let (j11, j21, j12, j22) = ((a1 - a) / h, (b1 - b) / h, (a2 - a) / h, (b2 - b) / h);
            let det = j11 * j22 - j12 * j21;
            if det.abs() < 1e-12 {
                break;
            }
            u -= (j22 * ex - j12 * ey) / det;
            v -= (-j21 * ex + j11 * ey) / det;
        }
        (u as f32, v as f32)
    }
}

/// The lens profile for a photo when the settings turn profiles on and its lens is in the database.
pub fn profile_for(s: &Settings, frame: &pe_raw::RawFrame) -> Option<pe_lens::Profile> {
    lens_match(s, frame).map(|m| m.profile)
}

/// The database match for a photo's lens (when profiles are on).
pub fn lens_match(s: &Settings, frame: &pe_raw::RawFrame) -> Option<pe_lens::Match> {
    if s.lens.profile != crate::settings::ProfileMode::On || frame.own_lens_corrections {
        return None;
    }
    let l = frame.lens.as_ref()?;
    pe_lens::lookup(&pe_lens::Shot {
        camera_make: frame.make.clone(),
        camera_model: frame.model.clone(),
        lens: l.name.clone(),
        focal_length: l.focal_length,
        aperture: l.aperture,
    })
}

/// Scale of the largest centred rectangle with the frame's aspect ratio that fits inside the frame
/// rotated by `angle_deg` (for auto-cropping when straightening).
pub fn straighten_fit(w: f32, h: f32, angle_deg: f32) -> f32 {
    let (s, c) = (
        angle_deg.to_radians().sin().abs(),
        angle_deg.to_radians().cos(),
    );
    (w / (w * c + h * s)).min(h / (w * s + h * c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Crop, LensCorrection};

    fn grid(w: usize, h: usize) -> Vec<f32> {
        (0..w * h)
            .flat_map(|i| [(i % w) as f32, (i / w) as f32, 1.0])
            .collect()
    }

    #[test]
    fn identity_maps_pixel_centres_and_orientation_matches_orient_apply() {
        let (w, h) = (7, 5);
        let img = grid(w, h);
        for code in 1..=8u8 {
            let mut p = GeometryParams::new(&Settings::default(), w, h, code);
            p.active = true;
            let (ow, oh, out) = resample(&img, &p);
            let (ew, eh, expect) = crate::orient::apply(code, w, h, &img);
            assert_eq!((ow, oh), (ew, eh));
            for (a, b) in out.iter().zip(&expect) {
                assert!((a - b).abs() < 1e-3, "orientation {code}");
            }
        }
    }

    #[test]
    fn crop_selects_the_region_and_rotation_fits() {
        let (w, h) = (100, 50);
        let img = grid(w, h);
        let s = Settings {
            crop: Crop {
                left: 0.5,
                top: 0.0,
                right: 1.0,
                bottom: 0.5,
                angle: 0.0,
            },
            ..Default::default()
        };
        let p = GeometryParams::new(&s, w, h, 1);
        let (ow, oh, out) = resample(&img, &p);
        assert_eq!((ow, oh), (50, 25));
        assert!(
            (out[0] - 50.0).abs() < 1e-3 && (out[1] - 0.0).abs() < 1e-3,
            "top-left is source (50, 0)"
        );
        // A rotated, auto-fitted crop never samples outside the frame.
        let k = straighten_fit(w as f32, h as f32, 10.0);
        let m = (1.0 - k) / 2.0;
        let s = Settings {
            crop: Crop {
                left: m,
                top: m,
                right: 1.0 - m,
                bottom: 1.0 - m,
                angle: 10.0,
            },
            ..Default::default()
        };
        let (_, _, out) = resample(&img, &GeometryParams::new(&s, w, h, 1));
        assert!(out.chunks(3).all(|p| p[2] > 0.99), "no black corners");
    }

    #[test]
    fn output_and_frame_positions_invert_each_other() {
        for s in [
            Settings::default(),
            serde_json::from_value(serde_json::json!({
                "crop": {"left": 0.1, "top": 0.2, "right": 0.8, "bottom": 0.9, "angle": 6},
                "transform": {"vertical": 30, "horizontal": -15, "scale": 90},
                "lens": {"distortion": 40}
            }))
            .unwrap(),
        ] {
            for code in [1u8, 6] {
                let p = GeometryParams::new(&s, 600, 400, code);
                for (u, v) in [(0.5, 0.5), (0.1, 0.2), (0.9, 0.75)] {
                    let (fx, fy) = p.output_to_frame(u, v);
                    let (u2, v2) = p.frame_to_output(fx, fy);
                    assert!(
                        (u - u2).abs() < 1e-4 && (v - v2).abs() < 1e-4,
                        "{code} {u},{v} -> {fx},{fy} -> {u2},{v2}"
                    );
                }
            }
        }
        // Only cropping: a plain rescale.
        let s: Settings = serde_json::from_value(
            serde_json::json!({"crop": {"left": 0.5, "top": 0.0, "right": 1.0, "bottom": 1.0}}),
        )
        .unwrap();
        let (fx, fy) = GeometryParams::new(&s, 600, 400, 1).output_to_frame(0.5, 0.5);
        assert!((fx - 0.75).abs() < 1e-4 && (fy - 0.5).abs() < 1e-4);
    }

    fn barrel() -> pe_lens::Profile {
        // A barrel lens: the photo's radius is 8 % short of the true one at r = 1.
        pe_lens::Profile {
            distortion: [1.0, 0.0, -0.08, 0.0, 0.0],
            tca_red: [1.001, 0.0, 0.0],
            tca_blue: [0.999, 0.0, 0.0],
            vignetting: [-0.5, 0.0, 0.0],
            has_distortion: true,
            has_tca: true,
            has_vignetting: true,
            dist_scale: 1.8,
            vig_scale: 1.0,
        }
    }

    #[test]
    fn lens_profile_maps_radially_and_fills_the_frame() {
        let (w, h) = (600, 400);
        let s = Settings::default();
        assert!(!GeometryParams::new(&s, w, h, 1).active);
        // Amount 0: no change at all.
        let none = GeometryParams::new(&s, w, h, 1).with_profile(&barrel(), 0.0, 0.0);
        assert_eq!(none.pdist, PDIST_IDENTITY);
        assert!(!none.has_lens_gain());
        let p = GeometryParams::new(&s, w, h, 1).with_profile(&barrel(), 1.0, 1.0);
        assert!(p.active);
        // Barrel correction samples nearer the centre, so the fill zoom is > 1 (more of the field).
        assert!(p.pfit > 1.0 && p.pfit < 1.2, "{}", p.pfit);
        // Every border pixel of the output samples inside the photo (no empty corners).
        for (x, y) in [
            (0, 0),
            (w - 1, 0),
            (0, h - 1),
            (w - 1, h - 1),
            (w / 2, 0),
            (0, h / 2),
        ] {
            for c in 0..3 {
                let (sx, sy) = p.source(x, y, c);
                assert!(
                    (-0.6..=w as f32 + 0.6).contains(&sx) && (-0.6..=h as f32 + 0.6).contains(&sy),
                    "{x},{y},{c}: {sx},{sy}"
                );
            }
        }
        // ... and some border reaches the edge (the zoom is as wide as possible).
        let (sx, _) = p.source(0, h / 2, 1);
        let (_, sy) = p.source(w / 2, 0, 1);
        assert!(sx < 2.0 || sy < 2.0, "{sx} {sy}");
        // The centre stays put; red and blue separate slightly off-centre.
        let (cx, cy) = p.source(w / 2, h / 2, 1);
        assert!((cx - w as f32 / 2.0).abs() < 1.0 && (cy - h as f32 / 2.0).abs() < 1.0);
        let (r, _) = p.source(w - 20, h / 2, 0);
        let (b, _) = p.source(w - 20, h / 2, 2);
        assert!(r > b);
        // Vignetting: corners brightened by 1 / (1 - 0.5 r²) with r = 1 at the corner.
        let g = p.lens_gain(0, 0);
        assert!((g - 2.0).abs() < 0.02, "{g}");
        assert!((p.lens_gain(w / 2, h / 2) - 1.0).abs() < 1e-3);
        // The amount scales distortion linearly.
        let half = GeometryParams::new(&s, w, h, 1).with_profile(&barrel(), 0.5, 0.0);
        assert!((half.pdist[2] + 0.04).abs() < 1e-6);
        // Masks follow: output and frame positions still invert each other.
        let (fx, fy) = p.output_to_frame(0.2, 0.3);
        let (u, v) = p.frame_to_output(fx, fy);
        assert!((u - 0.2).abs() < 1e-4 && (v - 0.3).abs() < 1e-4);
    }

    #[test]
    fn distortion_and_ca_move_samples_radially() {
        let s = Settings {
            lens: LensCorrection {
                distortion: 100.0,
                ca_red_cyan: 100.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let p = GeometryParams::new(&s, 200, 100, 1);
        // Barrel correction (k1 < 0) samples corners closer to the centre; red further out than green.
        let (gx, _) = p.source(0, 0, 1);
        let (rx, _) = p.source(0, 0, 0);
        assert!(gx > 0.5 && rx < gx, "{gx} {rx}");
        let (cx, cy) = p.source(100, 50, 1);
        assert!(
            (cx - 100.5).abs() < 0.01 && (cy - 50.5).abs() < 0.01,
            "centre fixed"
        );
    }
}
