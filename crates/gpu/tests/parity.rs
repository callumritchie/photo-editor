//! GPU vs CPU reference: every pixel's CIEDE2000 difference on real raws under varied settings.
//! Skipped when no GPU is available (e.g. CI runners).

use std::path::PathBuf;

use pe_gpu::Gpu;
use pe_pipeline::{prepare_preview, render, Settings, WhiteBalance};

fn samples() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/raw");
    [
        "Nikon-D750.nef",
        "Fujifilm-X-T3.raf",
        "Sony-ILCE-7M3.arw",
        "Canon-EOS_R6.cr3",
    ]
    .iter()
    .map(|n| dir.join(n))
    .filter(|p| p.exists())
    .collect()
}

fn cases() -> Vec<(&'static str, Settings)> {
    let d = Settings::default();
    vec![
        ("default", d.clone()),
        (
            "shadows/highlights",
            Settings {
                shadows: 70.0,
                highlights: -80.0,
                exposure: 0.5,
                ..d.clone()
            },
        ),
        (
            "contrast/whites/blacks",
            Settings {
                contrast: 45.0,
                whites: -30.0,
                blacks: 25.0,
                ..d.clone()
            },
        ),
        (
            "auto wb, curve",
            Settings {
                white_balance: WhiteBalance::Auto,
                tone_curve: vec![[0.0, 10.0], [64.0, 50.0], [190.0, 210.0], [255.0, 250.0]],
                ..d.clone()
            },
        ),
        (
            "custom wb, negative",
            Settings {
                white_balance: WhiteBalance::Custom {
                    temperature: 3200.0,
                    tint: 15.0,
                },
                exposure: -1.0,
                contrast: -40.0,
                shadows: -50.0,
                highlights: 60.0,
                ..d.clone()
            },
        ),
        (
            "no orientation",
            Settings {
                apply_orientation: false,
                exposure: 1.5,
                ..d
            },
        ),
    ]
}

fn lab(p: &[u8]) -> [f32; 3] {
    pe_colour::srgb_to_lab([
        p[0] as f32 / 255.0,
        p[1] as f32 / 255.0,
        p[2] as f32 / 255.0,
    ])
}

#[test]
fn gpu_matches_cpu_reference() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("no GPU; skipping");
        return;
    };
    eprintln!("GPU: {}", gpu.name());
    for path in samples() {
        let frame = pe_raw::decode_file(&path).unwrap();
        let preview = prepare_preview(&frame, 1024);
        let gf = gpu.upload(&preview).unwrap();
        for (name, s) in cases() {
            let cpu = render(&preview, &s).unwrap();
            let (w, h, px) = gpu.render(&gf, &s).unwrap();
            assert_eq!((w as usize, h as usize), (cpu.width, cpu.height), "{name}");
            let mut de: Vec<f32> = px
                .chunks(4)
                .zip(cpu.data.chunks(3))
                .map(|(g, c)| {
                    let c8 = [0, 1, 2].map(|k| (c[k] * 255.0 + 0.5) as u8);
                    pe_colour::delta_e2000(lab(g), lab(&c8))
                })
                .collect();
            // Differences come only from float rounding: never more than one 8-bit step per channel.
            let max_step = px
                .chunks(4)
                .zip(cpu.data.chunks(3))
                .flat_map(|(g, c)| {
                    (0..3).map(move |k| (g[k] as i32 - ((c[k] * 255.0 + 0.5) as u8) as i32).abs())
                })
                .max()
                .unwrap();
            assert!(
                max_step <= 1,
                "{path:?} {name}: a channel differs by {max_step} steps"
            );
            de.sort_by(|a, b| a.total_cmp(b));
            let mean = de.iter().sum::<f32>() / de.len() as f32;
            let p999 = de[de.len() * 999 / 1000];
            let max = de[de.len() - 1];
            eprintln!(
                "{:<28} {:<24} mean {mean:.3}  p99.9 {p999:.3}  max {max:.3}",
                path.file_name().unwrap().to_string_lossy(),
                name
            );
            assert!(
                mean < 0.1 && p999 < 0.5,
                "{path:?} {name}: mean {mean}, p99.9 {p999}"
            );
        }
    }
}
