//! EXIF orientation (TIFF/EXIF tag 0x0112): rotate/flip an interleaved RGB image for display.

/// Returns (width, height, data) after applying orientation `code` (1..=8; others treated as 1).
pub fn apply(code: u8, width: usize, height: usize, data: &[f32]) -> (usize, usize, Vec<f32>) {
    if !(2..=8).contains(&code) {
        return (width, height, data.to_vec());
    }
    let (w, h) = (width, height);
    let swap = code >= 5;
    let (dw, dh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0.0; data.len()];
    for dy in 0..dh {
        for dx in 0..dw {
            let (sx, sy) = match code {
                2 => (w - 1 - dx, dy),
                3 => (w - 1 - dx, h - 1 - dy),
                4 => (dx, h - 1 - dy),
                5 => (dy, dx),
                6 => (dy, h - 1 - dx),
                7 => (w - 1 - dy, h - 1 - dx),
                _ => (w - 1 - dy, dx), // 8
            };
            let (s, d) = ((sy * w + sx) * 3, (dy * dw + dx) * 3);
            out[d..d + 3].copy_from_slice(&data[s..s + 3]);
        }
    }
    (dw, dh, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3x2 image whose red channel holds the pixel index: row 0 = [0,1,2], row 1 = [3,4,5].
    fn img() -> Vec<f32> {
        (0..6).flat_map(|i| [i as f32, 0.0, 0.0]).collect()
    }
    fn reds(d: &[f32]) -> Vec<f32> {
        d.chunks(3).map(|p| p[0]).collect()
    }

    #[test]
    fn all_orientations() {
        let cases: [(u8, (usize, usize), [f32; 6]); 8] = [
            (1, (3, 2), [0., 1., 2., 3., 4., 5.]),
            (2, (3, 2), [2., 1., 0., 5., 4., 3.]),
            (3, (3, 2), [5., 4., 3., 2., 1., 0.]),
            (4, (3, 2), [3., 4., 5., 0., 1., 2.]),
            (5, (2, 3), [0., 3., 1., 4., 2., 5.]),
            (6, (2, 3), [3., 0., 4., 1., 5., 2.]), // rotate 90 clockwise
            (7, (2, 3), [5., 2., 4., 1., 3., 0.]),
            (8, (2, 3), [2., 5., 1., 4., 0., 3.]), // rotate 90 counter-clockwise
        ];
        for (code, dims, expect) in cases {
            let (w, h, d) = apply(code, 3, 2, &img());
            assert_eq!((w, h), dims, "code {code}");
            assert_eq!(reds(&d), expect, "code {code}");
        }
    }
}
