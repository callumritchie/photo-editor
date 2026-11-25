//! Demosaicing.
//!
//! M0 provides a generic neighbourhood-average demosaic that works for any colour filter array
//! (2x2 Bayer and 6x6 X-Trans alike): each missing colour is the mean of the same-colour samples in
//! the surrounding 3x3 window, widening to 5x5 where the 3x3 window lacks that colour. It is soft but
//! correct, and serves as the reference baseline. M2 adds RCD (Bayer) and Markesteijn (X-Trans).

use pe_raw::Cfa;
use rayon::prelude::*;

/// Demosaic a single-channel mosaic into interleaved RGB (`width * height * 3`).
pub fn neighbourhood_average(data: &[f32], width: usize, height: usize, cfa: &Cfa) -> Vec<f32> {
    assert_eq!(data.len(), width * height);
    let mut out = vec![0.0f32; width * height * 3];
    out.par_chunks_mut(width * 3)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..width {
                let own = cfa.color_at(y, x) as usize;
                let mut px = [0.0f32; 3];
                px[own] = data[y * width + x];
                for (c, slot) in px.iter_mut().enumerate() {
                    if c == own {
                        continue;
                    }
                    *slot = average(data, width, height, cfa, x, y, c as u8, 1)
                        .or_else(|| average(data, width, height, cfa, x, y, c as u8, 2))
                        .unwrap_or(0.0);
                }
                row[x * 3..x * 3 + 3].copy_from_slice(&px);
            }
        });
    out
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn average(
    data: &[f32],
    width: usize,
    height: usize,
    cfa: &Cfa,
    x: usize,
    y: usize,
    color: u8,
    radius: usize,
) -> Option<f32> {
    let (x0, x1) = (x.saturating_sub(radius), (x + radius).min(width - 1));
    let (y0, y1) = (y.saturating_sub(radius), (y + radius).min(height - 1));
    let (mut sum, mut n) = (0.0f32, 0u32);
    for yy in y0..=y1 {
        for xx in x0..=x1 {
            if cfa.color_at(yy, xx) == color {
                sum += data[yy * width + xx];
                n += 1;
            }
        }
    }
    (n > 0).then(|| sum / n as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mosaic(width: usize, height: usize, cfa: &Cfa, rgb: [f32; 3]) -> Vec<f32> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (y, x)))
            .map(|(y, x)| rgb[cfa.color_at(y, x) as usize])
            .collect()
    }

    #[test]
    fn flat_field_is_reconstructed_exactly_bayer() {
        for name in ["RGGB", "BGGR", "GRBG", "GBRG"] {
            let cfa = Cfa::bayer(name).unwrap();
            let (w, h) = (16, 12);
            let out = neighbourhood_average(&mosaic(w, h, &cfa, [0.2, 0.5, 0.8]), w, h, &cfa);
            for px in out.chunks(3) {
                assert_eq!(px, [0.2, 0.5, 0.8], "pattern {name}");
            }
        }
    }

    #[test]
    fn flat_field_is_reconstructed_exactly_xtrans() {
        // A 6x6 X-Trans-style layout (20 green, 8 red, 8 blue).
        let rows = ["GGRGGB", "GGBGGR", "BRGRBG", "GGBGGR", "GGRGGB", "RBGBRG"];
        let colors = rows
            .iter()
            .flat_map(|r| {
                r.chars().map(|c| match c {
                    'R' => 0,
                    'G' => 1,
                    _ => 2,
                })
            })
            .collect();
        let cfa = Cfa {
            width: 6,
            height: 6,
            colors,
        };
        let (w, h) = (18, 12);
        let out = neighbourhood_average(&mosaic(w, h, &cfa, [0.3, 0.6, 0.1]), w, h, &cfa);
        for px in out.chunks(3) {
            for (a, b) in px.iter().zip([0.3, 0.6, 0.1]) {
                assert!((a - b).abs() < 1e-6);
            }
        }
    }
}
