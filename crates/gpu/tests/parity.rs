//! GPU vs CPU reference: every pixel's CIEDE2000 difference on real raws under varied settings.
//! Skipped when no GPU is available (e.g. CI runners).

use std::path::PathBuf;

use pe_gpu::Gpu;
use pe_pipeline::masks::{Combine, LocalAdjust, Mask, MaskComponent, Raster, Shape, Stroke};
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
                ..d.clone()
            },
        ),
        (
            "vibrance, saturation, mixer",
            Settings {
                vibrance: 60.0,
                saturation: -20.0,
                hsl: pe_pipeline::Hsl {
                    hue: [30.0, -20.0, 10.0, 40.0, -30.0, 25.0, 0.0, -50.0],
                    saturation: [-40.0, 30.0, 20.0, -60.0, 50.0, 40.0, -10.0, 0.0],
                    luminance: [20.0, -30.0, 40.0, -50.0, 10.0, -40.0, 30.0, 0.0],
                },
                ..d.clone()
            },
        ),
        (
            "texture, clarity, dehaze",
            Settings {
                texture: 70.0,
                clarity: 60.0,
                dehaze: 50.0,
                shadows: 30.0,
                ..d.clone()
            },
        ),
        (
            "negative texture/clarity, add haze",
            Settings {
                texture: -80.0,
                clarity: -60.0,
                dehaze: -40.0,
                ..d.clone()
            },
        ),
        (
            "noise reduction",
            Settings {
                noise_reduction: pe_pipeline::NoiseReduction {
                    luminance: 90.0,
                    detail: 30.0,
                    color: 100.0,
                },
                exposure: 1.0,
                ..d.clone()
            },
        ),
        (
            "sharpening with masking + nr",
            Settings {
                sharpening: pe_pipeline::Sharpening {
                    amount: 150.0,
                    radius: 2.5,
                    detail: 60.0,
                    masking: 40.0,
                },
                noise_reduction: pe_pipeline::NoiseReduction {
                    luminance: 30.0,
                    detail: 50.0,
                    color: 25.0,
                },
                ..d.clone()
            },
        ),
        (
            "vignette, grain",
            Settings {
                vignette: pe_pipeline::Vignette {
                    amount: -70.0,
                    midpoint: 30.0,
                    roundness: -40.0,
                    feather: 70.0,
                    highlights: 50.0,
                },
                grain: pe_pipeline::Grain {
                    amount: 60.0,
                    size: 40.0,
                    roughness: 70.0,
                },
                ..d.clone()
            },
        ),
        (
            "lighten vignette, round",
            Settings {
                vignette: pe_pipeline::Vignette {
                    amount: 60.0,
                    roundness: 80.0,
                    ..Default::default()
                },
                apply_orientation: false,
                ..d.clone()
            },
        ),
        (
            "crop, straighten, perspective",
            Settings {
                crop: pe_pipeline::Crop {
                    left: 0.1,
                    top: 0.15,
                    right: 0.85,
                    bottom: 0.9,
                    angle: 7.5,
                },
                transform: pe_pipeline::Transform {
                    vertical: 40.0,
                    horizontal: -25.0,
                    scale: 90.0,
                },
                vignette: pe_pipeline::Vignette {
                    amount: -50.0,
                    ..Default::default()
                },
                ..d.clone()
            },
        ),
        (
            "lens corrections",
            Settings {
                lens: pe_pipeline::LensCorrection {
                    distortion: 60.0,
                    vignetting: 50.0,
                    ca_red_cyan: 80.0,
                    ca_blue_yellow: -60.0,
                    ..Default::default()
                },
                dehaze: 20.0,
                noise_reduction: pe_pipeline::NoiseReduction {
                    luminance: 30.0,
                    ..Default::default()
                },
                ..d.clone()
            },
        ),
        ("masks: gradients and brush", masks_case_a()),
        ("masks: ranges, raster, local detail", masks_case_b()),
        (
            "masks: local noise reduction, dehaze, sharpness",
            masks_case_c(),
        ),
        (
            "lens profile",
            Settings {
                lens: pe_pipeline::LensCorrection {
                    profile: pe_pipeline::ProfileMode::On,
                    profile_vignetting: 150.0,
                    ..Default::default()
                },
                ..Settings::default()
            },
        ),
        (
            "black & white, grading",
            Settings {
                treatment: pe_pipeline::Treatment::Monochrome,
                bw_mix: [40.0, 20.0, -30.0, -60.0, 0.0, 50.0, 10.0, -20.0],
                grading: pe_pipeline::Grading {
                    shadows: pe_pipeline::GradeWheel {
                        hue: 220.0,
                        saturation: 60.0,
                        luminance: -10.0,
                    },
                    highlights: pe_pipeline::GradeWheel {
                        hue: 45.0,
                        saturation: 40.0,
                        luminance: 15.0,
                    },
                    global: pe_pipeline::GradeWheel {
                        hue: 300.0,
                        saturation: 10.0,
                        luminance: 5.0,
                    },
                    blending: 80.0,
                    balance: -30.0,
                    ..Default::default()
                },
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
    let mut profiled = 0;
    let n_samples = samples().len();
    for path in samples() {
        let frame = pe_raw::decode_file(&path).unwrap();
        let preview = prepare_preview(&frame, 1024);
        let gf = gpu.upload(&preview).unwrap();
        let reference = render(&preview, &Settings::default()).unwrap();
        for (name, s) in cases() {
            let cpu = render(&preview, &s).unwrap();
            // Every case must visibly change the picture, or a feature that silently did nothing on
            // both paths would pass.
            // The lens-profile case changes only photos whose lens is in the database.
            let unprofiled = name == "lens profile"
                && pe_pipeline::geometry::profile_for(&s, &preview).is_none();
            if name == "lens profile" && !unprofiled {
                profiled += 1;
            }
            if name != "default"
                && !unprofiled
                && (cpu.width, cpu.height) == (reference.width, reference.height)
            {
                let to8 = |v: f32| (v * 255.0 + 0.5) as u8;
                let change = cpu
                    .data
                    .chunks(3)
                    .zip(reference.data.chunks(3))
                    .map(|(a, b)| {
                        pe_colour::delta_e2000(
                            lab(&a.iter().map(|v| to8(*v)).collect::<Vec<_>>()),
                            lab(&b.iter().map(|v| to8(*v)).collect::<Vec<_>>()),
                        )
                    })
                    .sum::<f32>()
                    / (cpu.data.len() / 3) as f32;
                assert!(
                    change > 0.3,
                    "{path:?} {name}: settings changed nothing (mean dE {change})"
                );
            }
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
    assert!(
        n_samples == 0 || profiled > 0,
        "no sample photo matched a lens profile"
    );
}

fn comp(mode: Combine, invert: bool, shape: Shape) -> MaskComponent {
    MaskComponent {
        mode,
        invert,
        shape,
    }
}

fn mask(id: &str, components: Vec<MaskComponent>, adjust: LocalAdjust) -> Mask {
    Mask {
        id: id.into(),
        name: String::new(),
        enabled: true,
        amount: 85.0,
        components,
        adjust,
    }
}

fn masks_case_a() -> Settings {
    let sky = mask(
        "sky",
        vec![comp(
            Combine::Add,
            false,
            Shape::Linear {
                x0: 0.5,
                y0: 0.0,
                x1: 0.5,
                y1: 0.55,
            },
        )],
        LocalAdjust {
            exposure: -0.8,
            temperature: -40.0,
            saturation: 30.0,
            highlights: -50.0,
            ..Default::default()
        },
    );
    let spot = mask(
        "spot",
        vec![
            comp(
                Combine::Add,
                false,
                Shape::Radial {
                    cx: 0.4,
                    cy: 0.6,
                    rx: 0.25,
                    ry: 0.3,
                    angle: 20.0,
                    feather: 0.6,
                },
            ),
            comp(
                Combine::Subtract,
                false,
                Shape::Brush {
                    strokes: vec![Stroke {
                        points: vec![[0.3, 0.6], [0.5, 0.65]],
                        size: 0.04,
                        feather: 0.5,
                        flow: 1.0,
                        erase: false,
                    }],
                },
            ),
        ],
        LocalAdjust {
            exposure: 0.7,
            contrast: 30.0,
            shadows: 40.0,
            tint: 20.0,
            ..Default::default()
        },
    );
    let edge = mask(
        "edge",
        vec![comp(
            Combine::Add,
            true,
            Shape::Radial {
                cx: 0.5,
                cy: 0.5,
                rx: 0.45,
                ry: 0.45,
                angle: 0.0,
                feather: 0.8,
            },
        )],
        LocalAdjust {
            whites: -40.0,
            blacks: 30.0,
            saturation: -60.0,
            ..Default::default()
        },
    );
    Settings {
        masks: vec![sky, spot, edge],
        ..Default::default()
    }
}

fn masks_case_b() -> Settings {
    let mut rasters = std::collections::HashMap::new();
    let (rw, rh) = (64, 48);
    rasters.insert(
        "subject".to_string(),
        Raster {
            width: rw,
            height: rh,
            data: (0..rw * rh)
                .map(|i| ((i % rw) as f32 / rw as f32).powi(2))
                .collect(),
        },
    );
    let lum = mask(
        "bright",
        vec![
            comp(
                Combine::Add,
                false,
                Shape::Luminance {
                    lo: 0.55,
                    hi: 1.0,
                    smoothness: 0.4,
                },
            ),
            comp(
                Combine::Intersect,
                false,
                Shape::Linear {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 0.0,
                    y1: 1.0,
                },
            ),
        ],
        LocalAdjust {
            clarity: 60.0,
            texture: 50.0,
            exposure: -0.3,
            ..Default::default()
        },
    );
    let colour = mask(
        "warm",
        vec![comp(
            Combine::Add,
            false,
            Shape::Color {
                colors: vec![[0.6, 0.08, 0.1], [0.5, 0.02, 0.12]],
                tolerance: 60.0,
            },
        )],
        LocalAdjust {
            saturation: 50.0,
            temperature: 25.0,
            ..Default::default()
        },
    );
    let subject = mask(
        "subject",
        vec![comp(
            Combine::Add,
            false,
            Shape::Raster {
                raster_id: "subject".into(),
                label: String::new(),
            },
        )],
        LocalAdjust {
            exposure: 0.5,
            clarity: -40.0,
            ..Default::default()
        },
    );
    Settings {
        masks: vec![lum, colour, subject],
        rasters: std::sync::Arc::new(rasters),
        ..Default::default()
    }
}

fn masks_case_c() -> Settings {
    let radial = |cx: f32| Shape::Radial {
        cx,
        cy: 0.5,
        rx: 0.3,
        ry: 0.4,
        angle: 20.0,
        feather: 0.6,
    };
    let a = mask(
        "left",
        vec![comp(Combine::Add, false, radial(0.3))],
        LocalAdjust {
            noise: 80.0,
            dehaze: 60.0,
            sharpness: 70.0,
            ..Default::default()
        },
    );
    let b = mask(
        "right",
        vec![comp(Combine::Add, false, radial(0.75))],
        LocalAdjust {
            dehaze: -70.0,
            sharpness: -60.0,
            ..Default::default()
        },
    );
    Settings {
        shadows: 40.0,
        clarity: 25.0,
        masks: vec![a, b],
        ..Settings::default()
    }
}
