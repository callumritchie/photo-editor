//! The GPU X-Trans demosaic matches the CPU one on a synthetic mosaic (no sample files needed).
use pe_raw::{Cfa, Layout, RawFrame};

#[test]
fn gpu_xtrans_demosaic_matches_cpu() {
    let Some(gpu) = pe_gpu::Gpu::new() else {
        eprintln!("no GPU; skipping");
        return;
    };
    let rows = ["GGRGGB", "GGBGGR", "BRGRBG", "GGBGGR", "GGRGGB", "RBGBRG"];
    let cfa = Cfa {
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
    };
    let (w, h) = (203, 157); // not multiples of 6: exercises the borders
    let data: Vec<f32> = (0..w * h)
        .map(|i| {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let edge = if x * 0.7 + y * 0.4 > 90.0 { 0.6 } else { 0.15 };
            (edge + 0.2 * (x * 0.9).sin() * (y * 0.5).cos()).max(0.0)
        })
        .collect();
    let frame = RawFrame {
        make: "Test".into(),
        model: "X-Trans".into(),
        width: w,
        height: h,
        layout: Layout::Mosaic(cfa.clone()),
        data: data.clone(),
        wb: [1.0; 3],
        xyz_to_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        orientation: 1,
        scale: 1.0,
        lens: None,
        rendered: false,
        exposure_bias: 0.0,
        dng_warp: None,
        own_lens_corrections: false,
    };
    let cpu = pe_pipeline::demosaic::high_quality(&data, w, h, &cfa);
    let g = gpu.demosaic(&frame).unwrap();
    let worst = g
        .data
        .iter()
        .zip(&cpu)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        worst < 1e-5,
        "GPU and CPU X-Trans demosaic differ by {worst}"
    );
}
