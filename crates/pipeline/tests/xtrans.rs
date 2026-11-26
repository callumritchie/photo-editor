//! X-Trans demosaic accuracy on a synthetic picture with known RGB everywhere.
use pe_pipeline::demosaic::{basic_xtrans, high_quality};
use pe_raw::Cfa;

/// The X-Trans layout (0 R, 1 G, 2 B).
fn xtrans() -> Cfa {
    let rows = ["GGRGGB", "GGBGGR", "BRGRBG", "GGBGGR", "GGRGGB", "RBGBRG"];
    Cfa {
        width: 6,
        height: 6,
        colors: rows
            .iter()
            .flat_map(|r| {
                r.chars().map(|c| match c {
                    'R' => 0,
                    'G' => 1,
                    _ => 2,
                })
            })
            .collect(),
    }
}

/// Detail like a photo's: luminance with edges, lines and fine texture at many scales and
/// orientations, times a colour that changes in blocks (colour differences vary slowly, as in
/// real scenes, but have sharp edges too).
fn reference(w: usize, h: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32, y as f32);
            let r2 =
                ((fx - w as f32 * 0.5).powi(2) + (fy - h as f32 * 0.5).powi(2)) / (w * w) as f32;
            // Zone plate: all orientations, frequencies up to ~0.25 cycles/pixel at the region's far
            // corner (finer detail than that aliases whatever the demosaic does).
            let zone = 0.5 + 0.5 * (r2 * 360.0).cos();
            let stripes = if ((fx * 0.8 + fy * 0.3) as i32 / 3) % 2 == 0 {
                0.35
            } else {
                0.9
            };
            let disc = if (fx - 70.0).hypot(fy - 150.0) < 40.0 {
                1.0
            } else {
                0.0
            };
            let tex = 0.5 + 0.5 * (fx * 1.3).sin() * (fy * 0.9).cos();
            let lum = if x < w / 3 {
                0.05 + 0.5 * zone
            } else if x < 2 * w / 3 {
                0.08 + 0.4 * stripes * (0.6 + 0.4 * disc)
            } else {
                0.05 + 0.45 * tex
            };
            let block = (x / 37 + 2 * (y / 29)) % 5;
            let tint = [
                [1.0, 0.9, 0.6],
                [0.6, 0.9, 1.0],
                [0.9, 1.0, 0.7],
                [1.0, 0.75, 0.8],
                [0.85, 0.85, 0.85],
            ][block];
            out.extend(tint.map(|t| lum * t));
        }
    }
    out
}

fn mosaic(rgb: &[f32], w: usize, h: usize, cfa: &Cfa) -> Vec<f32> {
    (0..w * h)
        .map(|i| rgb[i * 3 + cfa.color_at(i / w, i % w) as usize])
        .collect()
}

/// (PSNR in dB over RGB, mean colour-difference error) away from the border.
fn score(out: &[f32], truth: &[f32], w: usize, h: usize) -> (f64, f64) {
    score_in(out, truth, w, h, 8, w - 8)
}

fn score_in(out: &[f32], truth: &[f32], w: usize, h: usize, x0: usize, x1: usize) -> (f64, f64) {
    let (mut se, mut ce, mut n) = (0.0f64, 0.0f64, 0.0f64);
    for y in 8..h - 8 {
        for x in x0..x1 {
            let i = (y * w + x) * 3;
            for c in 0..3 {
                se += ((out[i + c] - truth[i + c]) as f64).powi(2);
            }
            let cd = |p: &[f32], c: usize| (p[i + c] - p[i + 1]) as f64;
            ce += (cd(out, 0) - cd(truth, 0)).abs() + (cd(out, 2) - cd(truth, 2)).abs();
            n += 1.0;
        }
    }
    (10.0 * (1.0 / (se / (n * 3.0))).log10(), ce / n)
}

#[test]
fn directional_xtrans_beats_plain_averaging() {
    let (w, h) = (300, 240);
    let cfa = xtrans();
    let truth = reference(w, h);
    let raw = mosaic(&truth, w, h, &cfa);
    let (p_old, c_old) = score(&basic_xtrans(&raw, w, h, &cfa), &truth, w, h);
    let (p_new, c_new) = score(&high_quality(&raw, w, h, &cfa), &truth, w, h);
    eprintln!("PSNR {p_old:.2} -> {p_new:.2} dB; colour error {c_old:.5} -> {c_new:.5}");
    // Overall: better, with much less false colour.
    assert!(p_new >= p_old + 1.0, "PSNR {p_old:.2} -> {p_new:.2}");
    assert!(c_new <= c_old * 0.7, "colour error {c_old} -> {c_new}");
    // Per region (zone plate, edges and lines, fine texture): never worse in PSNR, and much
    // better on edges, where the old averaging made zippers and colour fringes.
    let basic = basic_xtrans(&raw, w, h, &cfa);
    let new = high_quality(&raw, w, h, &cfa);
    for (k, (a, b)) in [(8, w / 3), (w / 3, 2 * w / 3), (2 * w / 3, w - 8)]
        .into_iter()
        .enumerate()
    {
        let o = score_in(&basic, &truth, w, h, a, b);
        let n = score_in(&new, &truth, w, h, a, b);
        eprintln!(
            "  x {a}..{b}: PSNR {:.2} -> {:.2}, colour {:.5} -> {:.5}",
            o.0, n.0, o.1, n.1
        );
        assert!(n.0 >= o.0, "region {k} PSNR worse: {} -> {}", o.0, n.0);
        if k == 1 {
            assert!(
                n.0 >= o.0 + 4.0 && n.1 <= o.1 * 0.5,
                "edges: {o:?} -> {n:?}"
            );
        }
    }
    // Flat areas stay exact.
    let flat = vec![0.3f32; 36 * 36];
    let out = high_quality(&flat, 36, 36, &cfa);
    assert!(out.iter().all(|v| (v - 0.3).abs() < 1e-6));
}
