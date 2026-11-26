//! GPU region renders match the CPU region renders (and so the matching part of a full render).
use std::path::PathBuf;

use pe_gpu::Gpu;
use pe_pipeline::region::{crop_frame, plan_region, Ctx};

fn lab(p: &[u8]) -> [f32; 3] {
    pe_colour::srgb_to_lab([
        p[0] as f32 / 255.0,
        p[1] as f32 / 255.0,
        p[2] as f32 / 255.0,
    ])
}

#[test]
fn gpu_region_matches_cpu_region() {
    let Some(gpu) = Gpu::new() else { return };
    let raw = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/raw/Nikon-D750.nef");
    if !raw.exists() {
        return;
    }
    let frame = pe_raw::decode_file(&raw).unwrap();
    let only = std::env::var("REGION_CASES").ok();
    let mut cases = vec![
        serde_json::json!({
            "exposure": 0.7, "shadows": 60, "highlights": -50, "clarity": 35, "texture": 25, "dehaze": 20,
            "whiteBalance": {"mode": "auto"},
            "noiseReduction": {"luminance": 40, "color": 40}, "sharpening": {"amount": 50},
            "vignette": {"amount": -30}, "grain": {"amount": 20}, "lens": {"vignetting": 30},
            "masks": [{"id": "m", "components": [{"shape": {"type": "radial", "cx": 0.45, "cy": 0.45, "rx": 0.2, "ry": 0.2}}],
                       "adjust": {"exposure": 0.6, "clarity": 30}}]
        }),
        serde_json::json!({
            "crop": {"left": 0.05, "top": 0.05, "right": 0.95, "bottom": 0.95, "angle": 3},
            "transform": {"vertical": 15}, "lens": {"distortion": 20, "caRedCyan": 30}
        }),
    ];
    if only.is_some() {
        cases = [
            r#"{"exposure": 0.7}"#, r#"{"shadows": 60, "highlights": -50}"#, r#"{"clarity": 35}"#,
            r#"{"texture": 25}"#, r#"{"dehaze": 20}"#, r#"{"whiteBalance": {"mode": "auto"}}"#,
            r#"{"noiseReduction": {"luminance": 40, "color": 40}}"#, r#"{"sharpening": {"amount": 50}}"#,
            r#"{"vignette": {"amount": -30}}"#, r#"{"grain": {"amount": 20}}"#, r#"{"lens": {"vignetting": 30}}"#,
            r#"{"masks": [{"id": "m", "components": [{"shape": {"type": "radial", "cx": 0.45, "cy": 0.45, "rx": 0.2, "ry": 0.2}}], "adjust": {"exposure": 0.6}}]}"#,
        ]
        .iter()
        .map(|j| serde_json::from_str(j).unwrap())
        .collect();
    }
    for settings in cases {
        eprintln!("case {settings}");
        let s: pe_pipeline::Settings = serde_json::from_value(settings).unwrap();
        let globals = pe_pipeline::measure_globals(&frame, &s).unwrap();
        let (fw, fh) = pe_pipeline::geometry::GeometryParams::new(
            &s,
            frame.width,
            frame.height,
            frame.orientation,
        )
        .output_size();
        let out = [fw / 3, fh / 3, 1200, 800];
        let (rect, region) = plan_region(&frame, &s, out);
        // Demosaic the region once; both renderers start from the same camera RGB.
        let crop = crop_frame(&frame, rect[0], rect[1], rect[2], rect[3]);
        let rgb = pe_raw::RawFrame {
            layout: pe_raw::Layout::LinearRgb,
            data: pe_pipeline::camera_rgb(&crop, &s),
            ..crop.clone_header()
        };
        let cpu = pe_pipeline::render_region(&rgb, &s, &region, &globals).unwrap();
        let ctx = Ctx {
            full_w: frame.width,
            full_h: frame.height,
            x0: rect[0],
            y0: rect[1],
        };
        let gf = gpu.upload_region(&rgb, ctx).unwrap();
        let (w, h, px) = gpu.render_region(&gf, &s, out, &globals).unwrap();
        assert_eq!((w as usize, h as usize), (cpu.width, cpu.height));
        let mut de: Vec<f32> = Vec::with_capacity(px.len() / 4);
        let mut max_step = 0;
        for (g, c) in px.chunks(4).zip(cpu.data.chunks(3)) {
            let c8 = [0, 1, 2].map(|k| (c[k] * 255.0 + 0.5) as u8);
            de.push(pe_colour::delta_e2000(lab(g), lab(&c8)));
            for k in 0..3 {
                max_step = max_step.max((g[k] as i32 - c8[k] as i32).abs());
            }
        }
        de.sort_by(|a, b| a.total_cmp(b));
        let mean = de.iter().sum::<f32>() / de.len() as f32;
        let p999 = de[de.len() * 999 / 1000];
        eprintln!("region: mean dE {mean:.4}, p99.9 {p999:.3}, max step {max_step}");
        // Only float rounding may differ: at most one 8-bit step anywhere. This night scene is mostly
        // deep shadow, where one step is up to ~1 dE2000, hence the percentile bound.
        if only.is_none() {
            assert!(
                mean < 0.1 && p999 < 1.0 && max_step <= 1,
                "mean {mean} p99.9 {p999} step {max_step}"
            );
        }
    }
}

#[test]
fn gpu_demosaic_matches_cpu() {
    let Some(gpu) = Gpu::new() else { return };
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/raw");
    for name in ["Nikon-D750.nef", "Fujifilm-X-T3.raf", "Canon-EOS_R6.cr3"] {
        let p = dir.join(name);
        if !p.exists() {
            continue;
        }
        let frame = pe_raw::decode_file(&p).unwrap();
        // A region (aligned to the CFA) keeps the test quick.
        let period = match &frame.layout {
            pe_raw::Layout::Mosaic(c) => c.width.max(c.height),
            _ => 1,
        };
        let x0 = (frame.width / 3) / period * period;
        let y0 = (frame.height / 3) / period * period;
        let crop = crop_frame(&frame, x0, y0, 1500, 1000);
        let t = std::time::Instant::now();
        let gpu_rgb = gpu.demosaic(&crop).unwrap();
        let gt = t.elapsed();
        let t = std::time::Instant::now();
        let cpu_rgb = pe_pipeline::camera_rgb(&crop, &pe_pipeline::Settings::default());
        let ct = t.elapsed();
        let worst = gpu_rgb
            .data
            .iter()
            .zip(&cpu_rgb)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        let differing = gpu_rgb
            .data
            .iter()
            .zip(&cpu_rgb)
            .filter(|(a, b)| (*a - *b).abs() > 1e-5)
            .count();
        eprintln!(
            "{name}: worst {worst:e}, {differing} of {} values differ; GPU {gt:.1?}, CPU {ct:.1?}",
            cpu_rgb.len()
        );
        // Only rounding may differ.
        assert!(
            worst < 1e-4 && differing * 10_000 < cpu_rgb.len(),
            "{name}: worst {worst}, {differing} differ"
        );
    }
}
