//! Masked noise reduction, sharpness and dehaze change only the masked part, in the right direction.
use pe_pipeline::{render_display, Settings};
use pe_raw::{Layout, RawFrame};

fn noise(i: usize) -> f32 {
    let mut x = (i as u32).wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 13;
    x = x.wrapping_mul(0xC2B2_AE35);
    x ^= x >> 16;
    (x as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Linear-RGB test frame: vertical stripes (edges), with noise, seen through mild haze.
fn frame(w: usize, h: usize) -> RawFrame {
    let data = (0..w * h)
        .flat_map(|i| {
            let x = i % w;
            let base = if (x / 12).is_multiple_of(2) {
                0.08
            } else {
                0.3
            };
            [0, 1, 2]
                .map(|c| ((base * (1.0 + 0.25 * noise(i * 3 + c))) * 0.6 + 0.35 * 0.4).max(0.0))
        })
        .collect();
    RawFrame {
        make: "Test".into(),
        model: "Synthetic".into(),
        width: w,
        height: h,
        layout: Layout::LinearRgb,
        data,
        wb: [1.0, 1.0, 1.0],
        xyz_to_cam: [
            [0.4124, 0.3576, 0.1805],
            [0.2126, 0.7152, 0.0722],
            [0.0193, 0.1192, 0.9505],
        ],
        orientation: 1,
        scale: 1.0,
        lens: None,
    }
}

fn settings(adjust: serde_json::Value) -> Settings {
    // Mask: the left half (linear gradient with a sharp step).
    serde_json::from_value(serde_json::json!({
        "masks": [{"id": "left", "components": [{"shape": {"type": "linear", "x0": 0.499, "y0": 0.5, "x1": 0.5, "y1": 0.5}}],
                   "adjust": adjust}]
    }))
    .unwrap()
}

/// Mean absolute difference between neighbouring pixels within flat stripe interiors (noise), and the
/// contrast between stripes, for x in [x0, x1).
fn stats(img: &[f32], w: usize, h: usize, x0: usize, x1: usize) -> (f32, f32) {
    let g = |x: usize, y: usize| img[(y * w + x) * 3 + 1];
    let (mut n, mut ns, mut lo, mut hi, mut nl, mut nh) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for y in 2..h - 2 {
        for x in x0..x1 - 1 {
            let k = x % 12;
            if (3..9).contains(&k) {
                ns += (g(x + 1, y) - g(x, y)).abs();
                n += 1.0;
                if (x / 12).is_multiple_of(2) {
                    lo += g(x, y);
                    nl += 1.0;
                } else {
                    hi += g(x, y);
                    nh += 1.0;
                }
            }
        }
    }
    (ns / n, hi / nh - lo / nl)
}

#[test]
fn masked_detail_adjustments_act_only_inside_the_mask() {
    let (w, h) = (240, 96);
    let f = frame(w, h);
    let base = render_display(&f, &settings(serde_json::json!({})))
        .unwrap()
        .data;
    let (bn_l, bc_l) = stats(&base, w, h, 12, 108);
    let (bn_r, bc_r) = stats(&base, w, h, 132, 228);

    let nr = render_display(&f, &settings(serde_json::json!({"noise": 100})))
        .unwrap()
        .data;
    let (n_l, _) = stats(&nr, w, h, 12, 108);
    let (n_r, _) = stats(&nr, w, h, 132, 228);
    assert!(
        n_l < bn_l * 0.6,
        "noise reduced in the mask: {n_l} vs {bn_l}"
    );
    assert!(
        (n_r - bn_r).abs() < bn_r * 0.02,
        "unchanged outside: {n_r} vs {bn_r}"
    );

    let dz = render_display(&f, &settings(serde_json::json!({"dehaze": 100})))
        .unwrap()
        .data;
    let (_, c_l) = stats(&dz, w, h, 12, 108);
    let (_, c_r) = stats(&dz, w, h, 132, 228);
    assert!(
        c_l > bc_l * 1.2,
        "contrast restored in the mask: {c_l} vs {bc_l}"
    );
    assert!((c_r - bc_r).abs() < bc_r * 0.02, "unchanged outside");
    let hz = render_display(&f, &settings(serde_json::json!({"dehaze": -100})))
        .unwrap()
        .data;
    assert!(
        stats(&hz, w, h, 12, 108).1 < bc_l * 0.8,
        "negative adds haze"
    );

    // Sharpness: edge overshoot next to stripe boundaries.
    // Mean dark-to-bright step across boundaries inside the left half (x = 12, 36, 60, 84), all rows.
    let edge = |img: &[f32]| {
        let g = |x: usize, y: usize| img[(y * w + x) * 3 + 1];
        let mut s = 0.0;
        let mut n = 0.0;
        for y in 2..h - 2 {
            for b in [12, 36, 60, 84] {
                s += g(b, y) - g(b - 1, y);
                n += 1.0;
            }
        }
        s / n
    };
    let sh = render_display(&f, &settings(serde_json::json!({"sharpness": 100})))
        .unwrap()
        .data;
    let so = render_display(&f, &settings(serde_json::json!({"sharpness": -100})))
        .unwrap()
        .data;
    assert!(
        edge(&sh) > edge(&base) * 1.05,
        "sharper: {} vs {}",
        edge(&sh),
        edge(&base)
    );
    assert!(
        edge(&so) < edge(&base) * 0.95,
        "softer: {} vs {}",
        edge(&so),
        edge(&base)
    );
}
