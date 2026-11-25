//! Minimal ICC v4.3 display profile writer (RGB matrix/TRC), per the public ICC.1:2022 specification.
//!
//! Tags: `desc`, `cprt` (multiLocalizedUnicode), `wtpt` (D50), `chad` (Bradford adaptation from the
//! space's native white to D50), `rXYZ`/`gXYZ`/`bXYZ` (D50-adapted colorants) and `rTRC`/`gTRC`/`bTRC`
//! (one shared parametric curve).

use crate::space::{bradford, ColorSpace, Transfer, D50_XY};

const D50_PCS: [f64; 3] = [0.9642, 1.0, 0.8249];

fn s15f16(v: f64) -> [u8; 4] {
    ((v * 65536.0).round() as i32).to_be_bytes()
}

fn xyz_tag(v: [f64; 3]) -> Vec<u8> {
    let mut t = b"XYZ \0\0\0\0".to_vec();
    for c in v {
        t.extend(s15f16(c));
    }
    t
}

fn mluc(text: &str) -> Vec<u8> {
    let utf16: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
    let mut t = b"mluc\0\0\0\0".to_vec();
    t.extend(1u32.to_be_bytes()); // record count
    t.extend(12u32.to_be_bytes()); // record size
    t.extend(b"enUS");
    t.extend((utf16.len() as u32).to_be_bytes());
    t.extend(28u32.to_be_bytes()); // offset of string from tag start
    t.extend(utf16);
    t
}

fn para(tf: Transfer) -> Vec<u8> {
    let mut t = b"para\0\0\0\0".to_vec();
    match tf {
        Transfer::Gamma(g) => {
            t.extend(0u16.to_be_bytes());
            t.extend([0, 0]);
            t.extend(s15f16(g));
        }
        Transfer::Srgb => {
            // Y = (aX + b)^g for X >= d, else cX.
            t.extend(3u16.to_be_bytes());
            t.extend([0, 0]);
            for v in [2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045] {
                t.extend(s15f16(v));
            }
        }
    }
    t
}

fn sf32(m: [[f64; 3]; 3]) -> Vec<u8> {
    let mut t = b"sf32\0\0\0\0".to_vec();
    for row in m {
        for v in row {
            t.extend(s15f16(v));
        }
    }
    t
}

/// Serialised ICC profile describing `space` for embedding in exported images.
pub fn profile(space: ColorSpace) -> Vec<u8> {
    let m = space.to_xyz_d50();
    let cols = [0, 1, 2].map(|c| [m[0][c], m[1][c], m[2][c]]);
    let curve = para(space.transfer());
    let tags: Vec<([u8; 4], Vec<u8>)> = vec![
        (*b"desc", mluc(space.name())),
        (*b"cprt", mluc("No copyright, use freely")),
        (*b"wtpt", xyz_tag(D50_PCS)),
        (*b"chad", sf32(bradford(space.white(), D50_XY))),
        (*b"rXYZ", xyz_tag(cols[0])),
        (*b"gXYZ", xyz_tag(cols[1])),
        (*b"bXYZ", xyz_tag(cols[2])),
        (*b"rTRC", curve.clone()),
        (*b"gTRC", curve.clone()),
        (*b"bTRC", curve),
    ];

    // Layout: header (128) + tag count (4) + table (12 per tag) + 4-byte-aligned data.
    let table_end = 128 + 4 + 12 * tags.len();
    let mut data = Vec::new();
    let mut table = Vec::new();
    let mut trc_offset = None;
    for (sig, body) in &tags {
        // Share one copy of the identical TRC between r/g/b.
        let shared = if sig.ends_with(b"TRC") {
            trc_offset
        } else {
            None
        };
        let offset = if let Some(o) = shared {
            o
        } else {
            while data.len() % 4 != 0 {
                data.push(0);
            }
            let o = table_end + data.len();
            data.extend(body);
            if sig.ends_with(b"TRC") {
                trc_offset = Some(o);
            }
            o
        };
        table.extend(sig);
        table.extend((offset as u32).to_be_bytes());
        table.extend((body.len() as u32).to_be_bytes());
    }
    while data.len() % 4 != 0 {
        data.push(0);
    }
    let size = table_end + data.len();

    let mut h = vec![0u8; 128];
    h[0..4].copy_from_slice(&(size as u32).to_be_bytes());
    h[8..12].copy_from_slice(&[0x04, 0x30, 0, 0]); // version 4.3
    h[12..16].copy_from_slice(b"mntr");
    h[16..20].copy_from_slice(b"RGB ");
    h[20..24].copy_from_slice(b"XYZ ");
    // Creation date 2025-01-01 00:00:00 (fixed, so profiles are byte-identical across runs).
    for (i, v) in [2025u16, 1, 1, 0, 0, 0].iter().enumerate() {
        h[24 + i * 2..26 + i * 2].copy_from_slice(&v.to_be_bytes());
    }
    h[36..40].copy_from_slice(b"acsp");
    h[64..68].copy_from_slice(&0u32.to_be_bytes()); // perceptual intent
    for (i, v) in D50_PCS.iter().enumerate() {
        h[68 + i * 4..72 + i * 4].copy_from_slice(&s15f16(*v));
    }

    let mut out = h;
    out.extend((tags.len() as u32).to_be_bytes());
    out.extend(table);
    out.extend(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn structure_is_valid() {
        for cs in ColorSpace::ALL {
            let p = profile(cs);
            assert_eq!(be32(&p, 0) as usize, p.len(), "{cs:?} size");
            assert_eq!(&p[36..40], b"acsp");
            let n = be32(&p, 128) as usize;
            assert_eq!(n, 10);
            for i in 0..n {
                let entry = 132 + i * 12;
                let (off, len) = (be32(&p, entry + 4) as usize, be32(&p, entry + 8) as usize);
                assert_eq!(off % 4, 0, "tag alignment");
                assert!(off + len <= p.len(), "tag inside profile");
            }
        }
    }

    #[test]
    fn colorants_sum_to_d50() {
        let p = profile(ColorSpace::DisplayP3);
        let read = |sig: &[u8; 4]| -> [f64; 3] {
            let n = be32(&p, 128) as usize;
            let e = (0..n)
                .map(|i| 132 + i * 12)
                .find(|&e| &p[e..e + 4] == sig)
                .unwrap();
            let off = be32(&p, e + 4) as usize;
            [0, 1, 2].map(|k| {
                i32::from_be_bytes(p[off + 8 + k * 4..off + 12 + k * 4].try_into().unwrap()) as f64
                    / 65536.0
            })
        };
        let sum = [0, 1, 2].map(|k| read(b"rXYZ")[k] + read(b"gXYZ")[k] + read(b"bXYZ")[k]);
        for (a, b) in sum.iter().zip(D50_PCS) {
            assert!((a - b).abs() < 1e-3, "{sum:?}");
        }
    }
}
