//! Demosaicing.
//!
//! - [`neighbourhood_average`]: generic for any colour filter array (2x2 Bayer, 6x6 X-Trans). Each
//!   missing colour is the mean of same-colour samples in the 3x3 window (5x5 fallback). Soft but
//!   artefact-free; the reference baseline.
//! - [`colour_difference`]: two passes. (1) Estimate green everywhere: on 2x2 Bayer with the
//!   edge-directed, Laplacian-corrected interpolation of Hamilton & Adams (US 5,629,734, 1997; expired),
//!   otherwise by averaging neighbouring greens. (2) Interpolate red and blue as *colour differences*
//!   (R-G, B-G), which vary slowly across edges, then add green back. Much sharper with far less
//!   colour fringing than plain averaging.
//! - Other patterns (Fujifilm X-Trans, 6x6) use this project's directional interpolation, see
//!   [`green_directional`]: along four directions, the nearest greens on each side give a linear
//!   estimate, corrected by half the curvature of the centre's own colour along the same line (the
//!   Hamilton-Adams idea, generalised to uneven spacing); the estimates are blended with weights
//!   falling with each direction's gradient. Red and blue colour differences are averaged with
//!   weights favouring neighbours of similar green, so they do not bleed across edges.

use pe_raw::Cfa;
use rayon::prelude::*;

/// Demosaic with [`colour_difference`], then suppress false colour with [`median_refine`].
pub fn high_quality(data: &[f32], width: usize, height: usize, cfa: &Cfa) -> Vec<f32> {
    let mut rgb = colour_difference(data, width, height, cfa);
    median_refine(&mut rgb, width, height, 1);
    rgb
}

/// False-colour suppression after Freeman (1988): replace the colour differences R-G and B-G with
/// their 3x3 median, then rebuild R and B from G. Isolated colour errors (demosaic "confetti",
/// zipper fringes) are removed while colour edges, which are coherent over the window, survive.
pub fn median_refine(rgb: &mut [f32], width: usize, height: usize, passes: usize) {
    for _ in 0..passes {
        let diff = |c: usize| -> Vec<f32> { rgb.chunks(3).map(|p| p[c] - p[1]).collect() };
        let (rg, bg) = (
            median3x3(&diff(0), width, height),
            median3x3(&diff(2), width, height),
        );
        rgb.par_chunks_mut(3).enumerate().for_each(|(i, p)| {
            p[0] = p[1] + rg[i];
            p[2] = p[1] + bg[i];
        });
    }
}

fn median3x3(src: &[f32], width: usize, height: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; src.len()];
    out.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
        let ys = [y.saturating_sub(1), y, (y + 1).min(height - 1)];
        for (x, v) in row.iter_mut().enumerate() {
            let xs = [x.saturating_sub(1), x, (x + 1).min(width - 1)];
            let mut w = [0.0f32; 9];
            let mut k = 0;
            for &yy in &ys {
                for &xx in &xs {
                    w[k] = src[yy * width + xx];
                    k += 1;
                }
            }
            let (_, m, _) = w.select_nth_unstable_by(4, |a, b| a.total_cmp(b));
            *v = *m;
        }
    });
    out
}

/// Demosaic into interleaved RGB using green estimation followed by colour-difference interpolation.
pub fn colour_difference(data: &[f32], width: usize, height: usize, cfa: &Cfa) -> Vec<f32> {
    colour_difference_with(data, width, height, cfa, false)
}

/// The previous treatment of non-Bayer patterns (plain averages of greens and colour differences)
/// followed by [`median_refine`]; kept to measure the directional method against.
pub fn basic_xtrans(data: &[f32], width: usize, height: usize, cfa: &Cfa) -> Vec<f32> {
    let mut rgb = colour_difference_with(data, width, height, cfa, true);
    median_refine(&mut rgb, width, height, 1);
    rgb
}

fn colour_difference_with(
    data: &[f32],
    width: usize,
    height: usize,
    cfa: &Cfa,
    basic: bool,
) -> Vec<f32> {
    assert_eq!(data.len(), width * height);
    let bayer = cfa.width == 2 && cfa.height == 2;
    let green = if bayer {
        green_hamilton_adams(data, width, height, cfa)
    } else if basic {
        green_average(data, width, height, cfa)
    } else {
        green_directional(data, width, height, cfa)
    };
    let mut out = vec![0.0f32; width * height * 3];
    out.par_chunks_mut(width * 3)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..width {
                let own = cfa.color_at(y, x) as usize;
                let g = green[y * width + x];
                let mut px = [0.0f32; 3];
                px[1] = g;
                for c in [0usize, 2] {
                    px[c] = if c == own {
                        data[y * width + x]
                    } else {
                        let avg = if bayer || basic {
                            diff_average
                        } else {
                            diff_weighted
                        };
                        let diff = avg(data, &green, width, height, cfa, x, y, c as u8, 1)
                            .or_else(|| avg(data, &green, width, height, cfa, x, y, c as u8, 2))
                            .unwrap_or(0.0);
                        g + diff
                    };
                }
                row[x * 3..x * 3 + 3].copy_from_slice(&px);
            }
        });
    out
}

/// Mean of (sample - green) over same-colour sites in a (2r+1)^2 window, excluding the centre.
#[allow(clippy::too_many_arguments)]
#[inline]
fn diff_average(
    data: &[f32],
    green: &[f32],
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
                let i = yy * width + xx;
                sum += data[i] - green[i];
                n += 1;
            }
        }
    }
    (n > 0).then(|| sum / n as f32)
}

/// Like [`diff_average`], weighting each site by how close its green is to the centre's
/// (1 / (|Δg| + [`DIFF_EPS`])), so colour does not bleed across edges.
#[allow(clippy::too_many_arguments)]
#[inline]
fn diff_weighted(
    data: &[f32],
    green: &[f32],
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
    let g0 = green[y * width + x];
    let (mut sum, mut wsum) = (0.0f32, 0.0f32);
    for yy in y0..=y1 {
        for xx in x0..=x1 {
            if cfa.color_at(yy, xx) == color {
                let i = yy * width + xx;
                let w = 1.0 / ((green[i] - g0).abs() + DIFF_EPS);
                sum += w * (data[i] - green[i]);
                wsum += w;
            }
        }
    }
    (wsum > 0.0).then(|| sum / wsum)
}

/// Regularisation of the colour-difference weights.
pub const DIFF_EPS: f32 = 2e-3;
/// Regularisation of the directional green weights (on 0..1 sensor values).
pub const DIR_EPS: f32 = 1e-2;
/// Directions of [`green_directional`]: horizontal, vertical and the two diagonals.
pub const DIRECTIONS: [(isize, isize); 4] = [(1, 0), (0, 1), (1, 1), (1, -1)];

/// Per CFA position (row-major over the pattern) and direction: steps to the nearest green on the
/// positive and negative side, then to the nearest sample of the position's own colour on each side
/// (0 = none within 6 steps). Green positions are all zeros.
pub fn direction_table(cfa: &Cfa) -> Vec<[[u32; 4]; 4]> {
    let (cw, ch) = (cfa.width, cfa.height);
    let colour = |x: isize, y: isize| {
        cfa.colors[(y.rem_euclid(ch as isize) as usize) * cw + x.rem_euclid(cw as isize) as usize]
    };
    (0..cw * ch)
        .map(|k| {
            let (x, y) = ((k % cw) as isize, (k / cw) as isize);
            let own = colour(x, y);
            let mut t = [[0u32; 4]; 4];
            if own == 1 {
                return t;
            }
            for (d, &(dx, dy)) in DIRECTIONS.iter().enumerate() {
                let find = |sign: isize, want: u8, max: isize| {
                    (1..=max)
                        .find(|&s| colour(x + sign * s * dx, y + sign * s * dy) == want)
                        .map_or(0, |s| s as u32)
                };
                t[d] = [
                    find(1, 1, 3),
                    find(-1, 1, 3),
                    find(1, own, 6),
                    find(-1, own, 6),
                ];
            }
            t
        })
        .collect()
}

/// Green at one non-green site by directional interpolation (see the module notes); `None` when no
/// direction has greens on both sides inside the image.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn green_at(
    data: &[f32],
    width: usize,
    height: usize,
    table: &[[u32; 4]; 4],
    x: usize,
    y: usize,
) -> Option<f32> {
    let c = data[y * width + x];
    let at = |s: u32, sign: isize, (dx, dy): (isize, isize)| -> Option<f32> {
        let (xx, yy) = (
            x as isize + sign * s as isize * dx,
            y as isize + sign * s as isize * dy,
        );
        (xx >= 0 && yy >= 0 && (xx as usize) < width && (yy as usize) < height)
            .then(|| data[yy as usize * width + xx as usize])
    };
    let (mut sum, mut wsum) = (0.0f32, 0.0f32);
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for (d, &dir) in DIRECTIONS.iter().enumerate() {
        let [a, b, ap, am] = table[d];
        if a == 0 || b == 0 {
            continue;
        }
        let (Some(gp), Some(gm)) = (at(a, 1, dir), at(b, -1, dir)) else {
            continue;
        };
        let (af, bf) = (a as f32, b as f32);
        let lin_g = (gp * bf + gm * af) / (af + bf);
        let mut corr = 0.0;
        if ap > 0 && am > 0 {
            if let (Some(cp), Some(cm)) = (at(ap, 1, dir), at(am, -1, dir)) {
                let (apf, amf) = (ap as f32, am as f32);
                corr = c - (cp * amf + cm * apf) / (apf + amf);
            }
        }
        let est = lin_g + 0.5 * corr;
        let grad = (gp - gm).abs() + corr.abs();
        let w = 1.0 / (grad + DIR_EPS);
        sum += w * est;
        wsum += w;
        lo = lo.min(gp.min(gm));
        hi = hi.max(gp.max(gm));
    }
    (wsum > 0.0).then(|| (sum / wsum).clamp(lo, hi))
}

/// Green everywhere for non-Bayer patterns (X-Trans) by [`green_at`], falling back to the mean of
/// nearby greens at the image border.
pub fn green_directional(data: &[f32], width: usize, height: usize, cfa: &Cfa) -> Vec<f32> {
    let table = direction_table(cfa);
    let mut g = vec![0.0f32; width * height];
    g.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
        for (x, v) in row.iter_mut().enumerate() {
            *v = if cfa.color_at(y, x) == 1 {
                data[y * width + x]
            } else {
                let k = (y % cfa.height) * cfa.width + x % cfa.width;
                green_at(data, width, height, &table[k], x, y)
                    .or_else(|| average(data, width, height, cfa, x, y, 1, 1))
                    .or_else(|| average(data, width, height, cfa, x, y, 1, 2))
                    .unwrap_or(0.0)
            };
        }
    });
    g
}

/// The previous X-Trans green (mean of nearby greens), kept for comparison in tests.
pub fn green_average(data: &[f32], width: usize, height: usize, cfa: &Cfa) -> Vec<f32> {
    let mut g = vec![0.0f32; width * height];
    g.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
        for (x, v) in row.iter_mut().enumerate() {
            *v = if cfa.color_at(y, x) == 1 {
                data[y * width + x]
            } else {
                average(data, width, height, cfa, x, y, 1, 1)
                    .or_else(|| average(data, width, height, cfa, x, y, 1, 2))
                    .unwrap_or(0.0)
            };
        }
    });
    g
}

/// Gradients closer than this (absolute plus relative) are a tie in [`green_hamilton_adams`].
pub const HA_TIE_ABS: f32 = 1e-6;
pub const HA_TIE_REL: f32 = 1e-4;

/// Edge-directed green interpolation for 2x2 Bayer patterns.
fn green_hamilton_adams(data: &[f32], width: usize, height: usize, cfa: &Cfa) -> Vec<f32> {
    // Mirror at the borders (x = -1 -> 1, x = -2 -> 2), which preserves the 2x2 CFA parity so
    // "green neighbour" and "same colour two away" stay true at the edges.
    let reflect = |v: isize, n: usize| -> usize {
        let n = n as isize;
        let r = if v < 0 {
            -v
        } else if v >= n {
            2 * (n - 1) - v
        } else {
            v
        };
        r.clamp(0, n - 1) as usize
    };
    let at = |x: isize, y: isize| -> f32 { data[reflect(y, height) * width + reflect(x, width)] };
    let mut g = vec![0.0f32; width * height];
    g.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
        let yi = y as isize;
        for (x, v) in row.iter_mut().enumerate() {
            let xi = x as isize;
            let c = data[y * width + x];
            if cfa.color_at(y, x) == 1 {
                *v = c;
                continue;
            }
            // Neighbouring greens (left/right, up/down) and same-colour samples two pixels away.
            let (gl, gr, gu, gd) = (
                at(xi - 1, yi),
                at(xi + 1, yi),
                at(xi, yi - 1),
                at(xi, yi + 1),
            );
            let lap_h = 2.0 * c - at(xi - 2, yi) - at(xi + 2, yi);
            let lap_v = 2.0 * c - at(xi, yi - 2) - at(xi, yi + 2);
            let grad_h = (gl - gr).abs() + lap_h.abs();
            let grad_v = (gu - gd).abs() + lap_v.abs();
            let est_h = (gl + gr) / 2.0 + lap_h / 4.0;
            let est_v = (gu + gd) / 2.0 + lap_v / 4.0;
            // Near-equal gradients count as a tie (the tolerance makes the decision independent of
            // floating-point evaluation order, so the GPU makes the same choice).
            let tie = (grad_h - grad_v).abs() <= HA_TIE_ABS + HA_TIE_REL * (grad_h + grad_v);
            let est = if tie {
                (est_h + est_v) / 2.0
            } else if grad_h < grad_v {
                est_h
            } else {
                est_v
            };
            // Limit Laplacian overshoot to the range of the four neighbouring greens.
            let lo = gl.min(gr).min(gu).min(gd);
            let hi = gl.max(gr).max(gu).max(gd);
            *v = est.clamp(lo, hi);
        }
    });
    g
}

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
    fn colour_difference_reconstructs_flat_fields() {
        for name in ["RGGB", "BGGR", "GRBG", "GBRG"] {
            let cfa = Cfa::bayer(name).unwrap();
            let (w, h) = (16, 12);
            let out = colour_difference(&mosaic(w, h, &cfa, [0.2, 0.5, 0.8]), w, h, &cfa);
            for px in out.chunks(3) {
                for (a, b) in px.iter().zip([0.2, 0.5, 0.8]) {
                    assert!((a - b).abs() < 1e-6, "{name}: {px:?}");
                }
            }
        }
    }

    #[test]
    fn colour_difference_keeps_a_vertical_edge_sharp() {
        // Grey step edge at x = 8: bilinear averaging smears it; edge-directed green should not.
        let cfa = Cfa::bayer("RGGB").unwrap();
        let (w, h) = (16, 12);
        let data: Vec<f32> = (0..w * h)
            .map(|i| if i % w < 8 { 0.1 } else { 0.9 })
            .collect();
        let sharp = colour_difference(&data, w, h, &cfa);
        let soft = neighbourhood_average(&data, w, h, &cfa);
        let err = |img: &[f32]| -> f32 {
            (0..w * h)
                .map(|i| {
                    let truth = if i % w < 8 { 0.1 } else { 0.9 };
                    (img[i * 3 + 1] - truth).abs()
                })
                .sum()
        };
        assert!(
            err(&sharp) < err(&soft) * 0.5,
            "sharp {} soft {}",
            err(&sharp),
            err(&soft)
        );
    }

    #[test]
    fn median_refine_removes_isolated_false_colour_but_keeps_colour_edges() {
        let (w, h) = (9, 9);
        // Left half red-ish, right half blue-ish, plus one magenta speck in the left half.
        let mut rgb: Vec<f32> = (0..w * h)
            .flat_map(|i| {
                if i % w < 5 {
                    [0.6, 0.3, 0.2]
                } else {
                    [0.2, 0.3, 0.6]
                }
            })
            .collect();
        let speck = (4 * w + 2) * 3;
        rgb[speck] = 0.9;
        rgb[speck + 2] = 0.9;
        median_refine(&mut rgb, w, h, 1);
        assert!((rgb[speck] - 0.6).abs() < 1e-6 && (rgb[speck + 2] - 0.2).abs() < 1e-6);
        // Pixels well inside each half keep their colour.
        assert_eq!(&rgb[(4 * w + 1) * 3..(4 * w + 1) * 3 + 3], &[0.6, 0.3, 0.2]);
        assert_eq!(&rgb[(4 * w + 7) * 3..(4 * w + 7) * 3 + 3], &[0.2, 0.3, 0.6]);
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
