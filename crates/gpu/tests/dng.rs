//! DNG corrections: the GPU applies a WarpRectilinear and BaselineExposure as the CPU does, and the
//! baseline exposure scales the scene by 2^BE.
use pe_pipeline::Settings;
use pe_raw::{dng::WarpModel, Layout, RawFrame};

fn frame(bias: f32, warp: bool) -> RawFrame {
    let (w, h) = (240usize, 160usize);
    let data = (0..w * h)
        .flat_map(|i| {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let grid = if (x as i32 / 12 + y as i32 / 12) % 2 == 0 {
                0.25
            } else {
                0.06
            };
            [
                grid * (0.8 + 0.2 * x / w as f32),
                grid,
                grid * (1.0 - 0.3 * y / h as f32),
            ]
        })
        .collect();
    RawFrame {
        make: "Test".into(),
        model: "DNG".into(),
        width: w,
        height: h,
        layout: Layout::LinearRgb,
        data,
        wb: [1.0; 3],
        xyz_to_cam: [[3.24, -1.54, -0.5], [-0.97, 1.88, 0.04], [0.06, -0.2, 1.06]],
        orientation: 1,
        scale: 1.0,
        lens: None,
        rendered: false,
        exposure_bias: bias,
        dng_warp: warp.then_some(WarpModel {
            coeffs: [
                [1.0, 0.06, 0.01, 0.0, 0.002, -0.001],
                [1.0, 0.05, 0.01, 0.0, 0.002, -0.001],
                [1.0, 0.045, 0.01, 0.0, 0.002, -0.001],
            ],
            centre: [0.52, 0.47],
            m_over_width: (120.0f32).hypot(80.0) / 240.0,
        }),
        own_lens_corrections: warp,
    }
}

#[test]
fn baseline_exposure_scales_the_scene() {
    let s = Settings::default();
    let a = pe_pipeline::render_scene(&frame(0.0, false), &s).unwrap();
    let b = pe_pipeline::render_scene(&frame(0.5, false), &s).unwrap();
    for (x, y) in a.data.iter().zip(&b.data) {
        if *x > 1e-3 {
            assert!((y / x - 0.5f32.exp2()).abs() < 1e-3, "{x} -> {y}");
        }
    }
}

#[test]
fn gpu_applies_dng_warp_and_baseline_like_cpu() {
    let Some(gpu) = pe_gpu::Gpu::new() else {
        eprintln!("no GPU; skipping");
        return;
    };
    let f = frame(0.4, true);
    let gf = gpu.upload(&f).unwrap();
    for s in [
        Settings::default(),
        serde_json::from_value(
            serde_json::json!({"crop": {"left": 0.1, "right": 0.9, "angle": 4}, "exposure": -0.3}),
        )
        .unwrap(),
    ] {
        let cpu = pe_pipeline::render(&f, &s).unwrap();
        let (w, h, px) = gpu.render(&gf, &s).unwrap();
        assert_eq!((w as usize, h as usize), (cpu.width, cpu.height));
        let worst = px
            .chunks(4)
            .zip(cpu.data.chunks(3))
            .flat_map(|(g, c)| {
                (0..3).map(move |k| (g[k] as i32 - (c[k] * 255.0 + 0.5) as i32).abs())
            })
            .max()
            .unwrap();
        assert!(worst <= 1, "GPU and CPU differ by {worst} levels");
    }
    // The warp changes the picture (it is not silently ignored).
    let plain = pe_pipeline::render(&frame(0.4, false), &Settings::default()).unwrap();
    let warped = pe_pipeline::render(&f, &Settings::default()).unwrap();
    let diff: f32 = plain
        .data
        .iter()
        .zip(&warped.data)
        .map(|(a, b)| (a - b).abs())
        .sum::<f32>()
        / plain.data.len() as f32;
    assert!(diff > 0.01, "warp had no effect ({diff})");
}
