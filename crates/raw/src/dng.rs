//! DNG extras that the decoder reads but does not apply, implemented from the published DNG
//! specification (version 1.4+): the opcodes cameras and converters use for mandatory lens
//! corrections: GainMap (9) and FixVignetteRadial (3), which scale sample values, and
//! WarpRectilinear (1), a distortion and chromatic-aberration model applied as geometry.
//! BaselineExposure is read (and reported) but not applied, so DNGs match native raws.
//!
//! Opcode coordinates are relative to the DNG "stage" image (the active area); [`Stage`] maps our
//! frame (cropped to the default crop) into it.

use std::path::Path;

/// TIFF tags used here.
const SUB_IFDS: u16 = 330;
const NEW_SUBFILE_TYPE: u16 = 254;
const EXIF_IFD: u16 = 34665;
const PHOTOMETRIC: u16 = 262;
const BASELINE_EXPOSURE: u16 = 50730;
const BASELINE_EXPOSURE_OFFSET: u16 = 51109;
const OPCODE_LIST: [u16; 3] = [51008, 51009, 51022];

/// What a DNG file says beyond the image itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DngExtras {
    /// Stops to add to the rendering (BaselineExposure + BaselineExposureOffset).
    pub baseline_exposure: f32,
    /// Opcode lists 1, 2 and 3 (supported opcodes only).
    pub opcodes: [Vec<Opcode>; 3],
    /// Ids of opcodes present but not supported (skipped).
    pub skipped: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Opcode {
    WarpRectilinear(Warp),
    FixVignetteRadial {
        k: [f64; 5],
        /// Optical centre, relative to the image area (0..1).
        cx: f64,
        cy: f64,
    },
    GainMap(GainMap),
}

/// WarpRectilinear: per plane `[kr0, kr1, kr2, kr3, kt0, kt1]`; one plane applies to all.
#[derive(Debug, Clone, PartialEq)]
pub struct Warp {
    pub planes: Vec<[f64; 6]>,
    pub cx: f64,
    pub cy: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GainMap {
    pub top: u32,
    pub left: u32,
    pub bottom: u32,
    pub right: u32,
    pub plane: u32,
    pub planes: u32,
    pub row_pitch: u32,
    pub col_pitch: u32,
    pub points_v: u32,
    pub points_h: u32,
    pub spacing_v: f64,
    pub spacing_h: f64,
    pub origin_v: f64,
    pub origin_h: f64,
    pub map_planes: u32,
    /// `points_v * points_h * map_planes` gains, row-major, planes innermost.
    pub gains: Vec<f32>,
}

/// A geometric warp ready for rendering, in units of the frame it belongs to: centre as fractions
/// of the frame's width and height, the normalising distance as a fraction of its width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarpModel {
    /// Per channel R, G, B: `[kr0, kr1, kr2, kr3, kt0, kt1]`.
    pub coeffs: [[f32; 6]; 3],
    pub centre: [f32; 2],
    pub m_over_width: f32,
}

impl WarpModel {
    /// Source position (frame pixels, continuous) sampled for corrected position (x, y), channel c,
    /// in a frame of `w` x `h` pixels.
    #[inline]
    pub fn source(&self, x: f32, y: f32, c: usize, w: f32, h: f32) -> (f32, f32) {
        let (cx, cy, m) = (
            self.centre[0] * w,
            self.centre[1] * h,
            self.m_over_width * w,
        );
        let (dx, dy) = ((x - cx) / m, (y - cy) / m);
        let r2 = dx * dx + dy * dy;
        let k = &self.coeffs[c];
        let f = k[0] + r2 * (k[1] + r2 * (k[2] + r2 * k[3]));
        let tx = k[4] * 2.0 * dx * dy + k[5] * (r2 + 2.0 * dx * dx);
        let ty = k[5] * 2.0 * dx * dy + k[4] * (r2 + 2.0 * dy * dy);
        (cx + m * (dx * f + tx), cy + m * (dy * f + ty))
    }
}

/// Where our frame sits in the DNG stage image (active area): offset and the stage size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stage {
    pub x0: usize,
    pub y0: usize,
    pub width: usize,
    pub height: usize,
}

// --- Reading -------------------------------------------------------------------------------------

struct Tiff<'a> {
    b: &'a [u8],
    le: bool,
}

#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    /// Offset of the value bytes (inline or pointed to).
    at: usize,
}

impl Tiff<'_> {
    fn u16(&self, o: usize) -> Option<u16> {
        let s: [u8; 2] = self.b.get(o..o + 2)?.try_into().ok()?;
        Some(if self.le {
            u16::from_le_bytes(s)
        } else {
            u16::from_be_bytes(s)
        })
    }
    fn u32(&self, o: usize) -> Option<u32> {
        let s: [u8; 4] = self.b.get(o..o + 4)?.try_into().ok()?;
        Some(if self.le {
            u32::from_le_bytes(s)
        } else {
            u32::from_be_bytes(s)
        })
    }
    fn i32(&self, o: usize) -> Option<i32> {
        self.u32(o).map(|v| v as i32)
    }
    fn type_size(t: u16) -> usize {
        match t {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 => 8,
            _ => 1,
        }
    }
    fn ifd(&self, off: usize) -> Option<(Vec<Entry>, usize)> {
        let n = self.u16(off)? as usize;
        if n > 4096 {
            return None;
        }
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            let e = off + 2 + i * 12;
            let (tag, typ, count) = (self.u16(e)?, self.u16(e + 2)?, self.u32(e + 4)?);
            let size = Self::type_size(typ) * count as usize;
            let at = if size <= 4 {
                e + 8
            } else {
                self.u32(e + 8)? as usize
            };
            v.push(Entry {
                tag,
                typ,
                count,
                at,
            });
        }
        let next = self.u32(off + 2 + n * 12).unwrap_or(0) as usize;
        Some((v, next))
    }
    fn first_u32(&self, e: &Entry) -> Option<u32> {
        match e.typ {
            3 => self.u16(e.at).map(u32::from),
            4 | 13 => self.u32(e.at),
            _ => None,
        }
    }
    fn first_real(&self, e: &Entry) -> Option<f64> {
        match e.typ {
            5 => Some(self.u32(e.at)? as f64 / self.u32(e.at + 4)?.max(1) as f64),
            10 => {
                let d = self.i32(e.at + 4)?;
                (d != 0).then(|| self.i32(e.at).unwrap_or(0) as f64 / d as f64)
            }
            _ => self.first_u32(e).map(f64::from),
        }
    }
}

/// All IFDs reachable from the header (chain, SubIFDs and the EXIF IFD), the first being IFD0.
fn ifds(t: &Tiff) -> Vec<Vec<Entry>> {
    let mut out = Vec::new();
    let mut todo = vec![t.u32(4).unwrap_or(0) as usize];
    let mut seen = std::collections::HashSet::new();
    while let Some(off) = todo.pop() {
        if off == 0 || !seen.insert(off) || out.len() > 64 {
            continue;
        }
        let Some((entries, next)) = t.ifd(off) else {
            continue;
        };
        for e in &entries {
            if e.tag == SUB_IFDS || e.tag == EXIF_IFD {
                for i in 0..e.count.min(16) as usize {
                    if let Some(o) = t.u32(e.at + 4 * i) {
                        todo.push(o as usize);
                    }
                }
            }
        }
        todo.push(next);
        out.push(entries);
    }
    // Keep IFD0 first (it was the first visited).
    out
}

/// Read [`DngExtras`] from a DNG file's bytes (`None` if it is not a TIFF).
pub fn read_extras(bytes: &[u8]) -> Option<DngExtras> {
    let le = match bytes.get(0..4)? {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return None,
    };
    let t = Tiff { b: bytes, le };
    let all = ifds(&t);
    let mut x = DngExtras::default();
    let find = |tag: u16| all.iter().flatten().find(|e| e.tag == tag).copied();
    for tag in [BASELINE_EXPOSURE, BASELINE_EXPOSURE_OFFSET] {
        if let Some(v) = find(tag).and_then(|e| t.first_real(&e)) {
            x.baseline_exposure += v as f32;
        }
    }
    // The raw image IFD: full resolution (NewSubFileType 0) with a CFA or linear-raw photometric.
    let raw = all.iter().find(|ifd| {
        let get = |tag| {
            ifd.iter()
                .find(|e| e.tag == tag)
                .and_then(|e| t.first_u32(e))
        };
        get(NEW_SUBFILE_TYPE).unwrap_or(0) == 0 && matches!(get(PHOTOMETRIC), Some(32803 | 34892))
    });
    for (i, tag) in OPCODE_LIST.iter().enumerate() {
        let e = raw
            .and_then(|ifd| ifd.iter().find(|e| e.tag == *tag))
            .or_else(|| all.iter().flatten().find(|e| e.tag == *tag));
        if let Some(e) = e {
            if let Some(data) = bytes.get(e.at..e.at + e.count as usize) {
                let (ops, skipped) = parse_opcodes(data);
                x.opcodes[i] = ops;
                x.skipped.extend(skipped);
            }
        }
    }
    Some(x)
}

/// Read [`DngExtras`] from a file.
pub fn read_extras_file(path: &Path) -> Option<DngExtras> {
    read_extras(&std::fs::read(path).ok()?)
}

/// Parse an opcode list (always big-endian): count, then per opcode id, version, flags, size, params.
pub fn parse_opcodes(d: &[u8]) -> (Vec<Opcode>, Vec<u32>) {
    let t = Tiff { b: d, le: false };
    let f64_at =
        |o: usize| -> Option<f64> { Some(f64::from_be_bytes(d.get(o..o + 8)?.try_into().ok()?)) };
    let mut ops = Vec::new();
    let mut skipped = Vec::new();
    let Some(n) = t.u32(0) else {
        return (ops, skipped);
    };
    let mut o = 4;
    for _ in 0..n.min(256) {
        let (Some(id), Some(size)) = (t.u32(o), t.u32(o + 12)) else {
            break;
        };
        let p = o + 16;
        let parsed = match id {
            1 => (|| {
                let planes = t.u32(p)? as usize;
                let mut v = Vec::new();
                for k in 0..planes.min(4) {
                    let b = p + 4 + k * 48;
                    let mut c = [0.0; 6];
                    for (j, cj) in c.iter_mut().enumerate() {
                        *cj = f64_at(b + j * 8)?;
                    }
                    v.push(c);
                }
                let b = p + 4 + planes * 48;
                Some(Opcode::WarpRectilinear(Warp {
                    planes: v,
                    cx: f64_at(b)?,
                    cy: f64_at(b + 8)?,
                }))
            })(),
            3 => (|| {
                let mut k = [0.0; 5];
                for (j, kj) in k.iter_mut().enumerate() {
                    *kj = f64_at(p + j * 8)?;
                }
                Some(Opcode::FixVignetteRadial {
                    k,
                    cx: f64_at(p + 40)?,
                    cy: f64_at(p + 48)?,
                })
            })(),
            9 => (|| {
                let u = |i: usize| t.u32(p + i * 4);
                let (points_v, points_h) = (u(8)?, u(9)?);
                let b = p + 40;
                let map_planes = t.u32(b + 32)?;
                let count = (points_v * points_h * map_planes) as usize;
                if count > 4_000_000 {
                    return None;
                }
                let g0 = b + 36;
                let gains = (0..count)
                    .map(|i| {
                        d.get(g0 + i * 4..g0 + i * 4 + 4)
                            .map(|s| f32::from_be_bytes(s.try_into().unwrap()))
                    })
                    .collect::<Option<Vec<f32>>>()?;
                Some(Opcode::GainMap(GainMap {
                    top: u(0)?,
                    left: u(1)?,
                    bottom: u(2)?,
                    right: u(3)?,
                    plane: u(4)?,
                    planes: u(5)?,
                    row_pitch: u(6)?.max(1),
                    col_pitch: u(7)?.max(1),
                    points_v,
                    points_h,
                    spacing_v: f64_at(b)?,
                    spacing_h: f64_at(b + 8)?,
                    origin_v: f64_at(b + 16)?,
                    origin_h: f64_at(b + 24)?,
                    map_planes,
                    gains,
                }))
            })(),
            _ => None,
        };
        match parsed {
            Some(op) => ops.push(op),
            None => skipped.push(id),
        }
        o = p + size as usize;
    }
    (ops, skipped)
}

// --- Applying --------------------------------------------------------------------------------------

impl GainMap {
    /// Gain at a stage-image position (relative 0..1), map plane `mp`, bilinear between map points.
    fn gain(&self, rv: f64, rh: f64, mp: usize) -> f32 {
        let fv = if self.spacing_v > 0.0 {
            (rv - self.origin_v) / self.spacing_v
        } else {
            0.0
        };
        let fh = if self.spacing_h > 0.0 {
            (rh - self.origin_h) / self.spacing_h
        } else {
            0.0
        };
        let (nv, nh) = (self.points_v.max(1) as usize, self.points_h.max(1) as usize);
        let fv = fv.clamp(0.0, (nv - 1) as f64);
        let fh = fh.clamp(0.0, (nh - 1) as f64);
        let (v0, h0) = (fv.floor() as usize, fh.floor() as usize);
        let (v1, h1) = ((v0 + 1).min(nv - 1), (h0 + 1).min(nh - 1));
        let (tv, th) = ((fv - v0 as f64) as f32, (fh - h0 as f64) as f32);
        let mpn = self.map_planes.max(1) as usize;
        let mp = mp.min(mpn - 1);
        let at = |v: usize, h: usize| self.gains[(v * nh + h) * mpn + mp];
        let top = at(v0, h0) * (1.0 - th) + at(v0, h1) * th;
        let bot = at(v1, h0) * (1.0 - th) + at(v1, h1) * th;
        top * (1.0 - tv) + bot * tv
    }
}

/// Apply the sample-scaling opcodes (GainMap, FixVignetteRadial) from all lists to frame data of
/// `width` x `height` x `cpp`, placed at `stage` within the DNG stage image.
pub fn apply_gains(
    extras: &DngExtras,
    data: &mut [f32],
    width: usize,
    height: usize,
    cpp: usize,
    stage: Stage,
) {
    let (sw, sh) = (stage.width as f64, stage.height as f64);
    for op in extras.opcodes.iter().flatten() {
        match op {
            Opcode::GainMap(g) => {
                for y in 0..height {
                    let sy = (y + stage.y0) as u32;
                    if sy < g.top || sy >= g.bottom || !(sy - g.top).is_multiple_of(g.row_pitch) {
                        continue;
                    }
                    let rv = sy as f64 / sh;
                    for x in 0..width {
                        let sx = (x + stage.x0) as u32;
                        if sx < g.left
                            || sx >= g.right
                            || !(sx - g.left).is_multiple_of(g.col_pitch)
                        {
                            continue;
                        }
                        let rh = sx as f64 / sw;
                        for p in 0..g.planes.max(1) as usize {
                            let plane = g.plane as usize + p;
                            if plane >= cpp {
                                break;
                            }
                            let i = (y * width + x) * cpp + plane;
                            data[i] *= g.gain(rv, rh, p);
                        }
                    }
                }
            }
            Opcode::FixVignetteRadial { k, cx, cy } => {
                let (cxp, cyp) = (cx * sw, cy * sh);
                // Normalised so the farthest corner of the image area is at r = 1.
                let m2 = [(0.0, 0.0), (sw, 0.0), (0.0, sh), (sw, sh)]
                    .iter()
                    .map(|(x, y): &(f64, f64)| (x - cxp).powi(2) + (y - cyp).powi(2))
                    .fold(0.0f64, f64::max)
                    .max(1e-9);
                for y in 0..height {
                    let dy = (y + stage.y0) as f64 + 0.5 - cyp;
                    for x in 0..width {
                        let dx = (x + stage.x0) as f64 + 0.5 - cxp;
                        let r2 = (dx * dx + dy * dy) / m2;
                        let g =
                            1.0 + r2 * (k[0] + r2 * (k[1] + r2 * (k[2] + r2 * (k[3] + r2 * k[4]))));
                        for c in 0..cpp {
                            data[(y * width + x) * cpp + c] *= g as f32;
                        }
                    }
                }
            }
            Opcode::WarpRectilinear(_) => {}
        }
    }
}

/// The WarpRectilinear in the lists (the last one wins), as a model for a frame of
/// `width` x `height` placed at `stage`.
pub fn warp_model(
    extras: &DngExtras,
    width: usize,
    height: usize,
    stage: Stage,
) -> Option<WarpModel> {
    let w = extras
        .opcodes
        .iter()
        .flatten()
        .rev()
        .find_map(|op| match op {
            Opcode::WarpRectilinear(w) if !w.planes.is_empty() => Some(w),
            _ => None,
        })?;
    let (sw, sh) = (stage.width as f64, stage.height as f64);
    let (cxp, cyp) = (w.cx * sw, w.cy * sh);
    let m = [(0.0, 0.0), (sw, 0.0), (0.0, sh), (sw, sh)]
        .iter()
        .map(|(x, y): &(f64, f64)| ((x - cxp).powi(2) + (y - cyp).powi(2)).sqrt())
        .fold(0.0f64, f64::max)
        .max(1e-9);
    let plane = |c: usize| {
        let p = w.planes[c.min(w.planes.len() - 1)];
        p.map(|v| v as f32)
    };
    Some(WarpModel {
        coeffs: [plane(0), plane(1), plane(2)],
        centre: [
            ((cxp - stage.x0 as f64) / width as f64) as f32,
            ((cyp - stage.y0 as f64) / height as f64) as f32,
        ],
        m_over_width: (m / width as f64) as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be32(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }
    fn bef(v: f64) -> [u8; 8] {
        v.to_be_bytes()
    }
    fn opcode(id: u32, params: Vec<u8>) -> Vec<u8> {
        [
            be32(id).to_vec(),
            be32(1).to_vec(),
            be32(0).to_vec(),
            be32(params.len() as u32).to_vec(),
            params,
        ]
        .concat()
    }
    fn list(ops: Vec<Vec<u8>>) -> Vec<u8> {
        let mut v = be32(ops.len() as u32).to_vec();
        for o in ops {
            v.extend(o);
        }
        v
    }
    fn warp_op(k: [f64; 6], cx: f64, cy: f64) -> Vec<u8> {
        let mut p = be32(1).to_vec();
        for v in k {
            p.extend(bef(v));
        }
        p.extend(bef(cx));
        p.extend(bef(cy));
        opcode(1, p)
    }
    fn vignette_op(k: [f64; 5]) -> Vec<u8> {
        let mut p = Vec::new();
        for v in k {
            p.extend(bef(v));
        }
        p.extend(bef(0.5));
        p.extend(bef(0.5));
        opcode(3, p)
    }
    fn gain_map_op(gains: [f32; 4]) -> Vec<u8> {
        // Whole 100x80 image, one plane, every pixel; 2x2 map at the corners.
        let mut p = Vec::new();
        for v in [0u32, 0, 80, 100, 0, 1, 1, 1, 2, 2] {
            p.extend(be32(v));
        }
        for v in [1.0f64, 1.0, 0.0, 0.0] {
            p.extend(bef(v));
        }
        p.extend(be32(1));
        for g in gains {
            p.extend(g.to_be_bytes());
        }
        opcode(9, p)
    }

    #[test]
    fn parses_supported_opcodes_and_skips_others() {
        let d = list(vec![
            warp_op([1.0, 0.02, 0.0, 0.0, 0.001, 0.0], 0.5, 0.5),
            opcode(4, vec![0; 12]), // FixBadPixelsConstant: not supported
            vignette_op([0.5, 0.0, 0.0, 0.0, 0.0]),
            gain_map_op([1.0, 2.0, 3.0, 4.0]),
        ]);
        let (ops, skipped) = parse_opcodes(&d);
        assert_eq!(skipped, vec![4]);
        assert_eq!(ops.len(), 3);
        assert!(
            matches!(&ops[0], Opcode::WarpRectilinear(w) if w.planes[0][1] == 0.02 && w.planes[0][4] == 0.001)
        );
        assert!(matches!(&ops[1], Opcode::FixVignetteRadial { k, .. } if k[0] == 0.5));
        assert!(
            matches!(&ops[2], Opcode::GainMap(g) if g.gains == vec![1.0, 2.0, 3.0, 4.0] && g.points_h == 2)
        );
    }

    #[test]
    fn gains_scale_samples_by_position() {
        let stage = Stage {
            x0: 0,
            y0: 0,
            width: 100,
            height: 80,
        };
        let extras = |ops: Vec<Vec<u8>>| DngExtras {
            opcodes: [vec![], parse_opcodes(&list(ops)).0, vec![]],
            ..Default::default()
        };
        // Vignette: gain 1 + 1 r^2, r = 1 at the corners.
        let mut d = vec![1.0f32; 100 * 80];
        apply_gains(
            &extras(vec![vignette_op([1.0, 0.0, 0.0, 0.0, 0.0])]),
            &mut d,
            100,
            80,
            1,
            stage,
        );
        assert!((d[0] - 2.0).abs() < 0.05, "corner {}", d[0]);
        assert!(
            (d[40 * 100 + 50] - 1.0).abs() < 0.01,
            "centre {}",
            d[40 * 100 + 50]
        );
        // Gain map: 1 top-left, 2 top-right, 3 bottom-left, 4 bottom-right; bilinear between.
        let mut d = vec![1.0f32; 100 * 80];
        apply_gains(
            &extras(vec![gain_map_op([1.0, 2.0, 3.0, 4.0])]),
            &mut d,
            100,
            80,
            1,
            stage,
        );
        assert!((d[0] - 1.0).abs() < 1e-4);
        assert!((d[99] - 1.99).abs() < 0.02, "{}", d[99]);
        assert!((d[79 * 100] - 2.975).abs() < 0.05, "{}", d[79 * 100]);
        // A frame cropped inside the stage picks up the stage position.
        let mut d = vec![1.0f32; 50 * 40];
        let inner = Stage {
            x0: 50,
            y0: 40,
            width: 100,
            height: 80,
        };
        apply_gains(
            &extras(vec![gain_map_op([1.0, 2.0, 3.0, 4.0])]),
            &mut d,
            50,
            40,
            1,
            inner,
        );
        assert!((d[0] - 2.5).abs() < 0.01, "{}", d[0]);
    }

    #[test]
    fn warp_maps_about_its_centre() {
        let stage = Stage {
            x0: 0,
            y0: 0,
            width: 200,
            height: 100,
        };
        let model = |k: [f64; 6], cx: f64| {
            let x = DngExtras {
                opcodes: [
                    vec![],
                    vec![],
                    parse_opcodes(&list(vec![warp_op(k, cx, 0.5)])).0,
                ],
                ..Default::default()
            };
            warp_model(&x, 200, 100, stage).unwrap()
        };
        let id = model([1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 0.5);
        let (x, y) = id.source(170.0, 20.0, 1, 200.0, 100.0);
        assert!((x - 170.0).abs() < 1e-3 && (y - 20.0).abs() < 1e-3);
        // kr1 > 0: corrected points sample farther out (the photo was barrel-distorted).
        let barrel = model([1.0, 0.1, 0.0, 0.0, 0.0, 0.0], 0.5);
        let (x, _) = barrel.source(190.0, 50.0, 1, 200.0, 100.0);
        assert!(x > 190.0);
        let (x, y) = barrel.source(100.0, 50.0, 1, 200.0, 100.0);
        assert!(
            (x - 100.0).abs() < 1e-3 && (y - 50.0).abs() < 1e-3,
            "centre stays"
        );
        // An off-centre optical centre stays fixed instead.
        let off = model([1.0, 0.1, 0.0, 0.0, 0.0, 0.0], 0.25);
        let (x, _) = off.source(50.0, 50.0, 1, 200.0, 100.0);
        assert!((x - 50.0).abs() < 1e-3);
        // Same model for a half-size preview of the frame: positions scale.
        let (x1, _) = barrel.source(190.0, 50.0, 1, 200.0, 100.0);
        let (x2, _) = barrel.source(95.0, 25.0, 1, 100.0, 50.0);
        assert!((x1 / 2.0 - x2).abs() < 1e-3);
    }

    #[test]
    fn reads_baseline_exposure_and_opcodes_from_a_tiff() {
        // Little-endian TIFF: IFD0 with BaselineExposure 1/2 and a SubIFD (raw: NewSubFileType 0,
        // photometric CFA) carrying OpcodeList3.
        let ops = list(vec![warp_op([1.0, 0.05, 0.0, 0.0, 0.0, 0.0], 0.5, 0.5)]);
        let mut b = b"II*\0".to_vec();
        b.extend(8u32.to_le_bytes());
        // IFD0 at 8: 2 entries.
        let ifd0_len = 2 + 2 * 12 + 4;
        let rational_at = 8 + ifd0_len;
        let sub_at = rational_at + 8;
        let entry = |tag: u16, typ: u16, count: u32, val: u32| {
            [
                tag.to_le_bytes().to_vec(),
                typ.to_le_bytes().to_vec(),
                count.to_le_bytes().to_vec(),
                val.to_le_bytes().to_vec(),
            ]
            .concat()
        };
        b.extend(2u16.to_le_bytes());
        b.extend(entry(SUB_IFDS, 4, 1, sub_at as u32));
        b.extend(entry(BASELINE_EXPOSURE, 10, 1, rational_at as u32));
        b.extend(0u32.to_le_bytes());
        b.extend(1i32.to_le_bytes());
        b.extend(2i32.to_le_bytes());
        // Sub IFD: 3 entries, then the opcode bytes.
        let sub_len = 2 + 3 * 12 + 4;
        let ops_at = sub_at + sub_len;
        b.extend(3u16.to_le_bytes());
        b.extend(entry(NEW_SUBFILE_TYPE, 4, 1, 0));
        b.extend(entry(PHOTOMETRIC, 3, 1, 32803));
        b.extend(entry(OPCODE_LIST[2], 7, ops.len() as u32, ops_at as u32));
        b.extend(0u32.to_le_bytes());
        b.extend(&ops);
        let x = read_extras(&b).unwrap();
        assert!((x.baseline_exposure - 0.5).abs() < 1e-6);
        assert!(x.opcodes[0].is_empty() && x.opcodes[1].is_empty());
        assert!(
            matches!(&x.opcodes[2][..], [Opcode::WarpRectilinear(w)] if w.planes[0][1] == 0.05)
        );
        assert!(read_extras(b"not a tiff").is_none());
    }
}
