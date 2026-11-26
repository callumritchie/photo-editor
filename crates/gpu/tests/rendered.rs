//! JPEGs on the GPU: an untouched one renders as it went in, and edits match the CPU path.
use pe_gpu::Gpu;
use pe_pipeline::Settings;

#[test]
fn jpeg_on_the_gpu_matches_source_and_cpu() {
    let Some(gpu) = Gpu::new() else {
        eprintln!("no GPU; skipping");
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("a.jpg");
    let img = image::RgbImage::from_fn(160, 120, |x, y| {
        image::Rgb([(x * 255 / 159) as u8, (y * 2) as u8, ((x * y) % 256) as u8])
    });
    img.save(&path).unwrap();
    let src = image::open(&path).unwrap().to_rgb8();
    let frame = pe_raw::decode_file(&path).unwrap();
    let preview = pe_pipeline::prepare_preview(&frame, 1024);
    let gf = gpu.upload(&preview).unwrap();

    let (w, h, px) = gpu.render(&gf, &Settings::default()).unwrap();
    assert_eq!((w, h), (160, 120));
    let worst = px
        .chunks(4)
        .zip(src.pixels())
        .flat_map(|(g, s)| (0..3).map(move |c| (g[c] as i32 - s.0[c] as i32).abs()))
        .max()
        .unwrap();
    assert!(worst <= 1, "untouched: differs by {worst} levels");

    // Edits: exposure, contrast, whites, a radial mask, crop: GPU matches CPU within one step.
    let s: Settings = serde_json::from_value(serde_json::json!({
        "exposure": 0.6, "contrast": 30, "whites": -20, "blacks": 10, "vibrance": 25,
        "crop": {"left": 0.1, "top": 0.05, "right": 0.95, "bottom": 0.9, "angle": 3},
        "masks": [{"id": "m", "components": [{"shape": {"type": "radial", "cx": 0.4, "cy": 0.5, "rx": 0.3, "ry": 0.3, "angle": 0, "feather": 0.5}}],
                   "adjust": {"exposure": -0.7, "saturation": 30}}]
    }))
    .unwrap();
    let cpu = pe_pipeline::render(&preview, &s).unwrap();
    let (w, h, px) = gpu.render(&gf, &s).unwrap();
    assert_eq!((w as usize, h as usize), (cpu.width, cpu.height));
    let worst = px
        .chunks(4)
        .zip(cpu.data.chunks(3))
        .flat_map(|(g, c)| (0..3).map(move |k| (g[k] as i32 - (c[k] * 255.0 + 0.5) as i32).abs()))
        .max()
        .unwrap();
    assert!(worst <= 1, "edited: GPU and CPU differ by {worst} levels");
}
