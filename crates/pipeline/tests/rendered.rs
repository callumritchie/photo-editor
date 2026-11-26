//! JPEGs (already-rendered pictures): an untouched one renders as it went in; edits act in linear
//! light.
use pe_colour::ColorSpace;
use pe_pipeline::{render, Settings};

/// A test picture: smooth ramps plus muted colour patches (inside every output gamut).
fn pixels(w: u32, h: u32) -> image::RgbImage {
    image::RgbImage::from_fn(w, h, |x, y| {
        if y < h / 2 {
            let v = (x * 255 / (w - 1)) as u8;
            image::Rgb([v, ((x + y) * 3 % 256) as u8, 255 - v])
        } else {
            let k = (x / 16 + y / 16) % 4;
            [
                image::Rgb([150, 120, 100]),
                image::Rgb([90, 130, 110]),
                image::Rgb([110, 110, 150]),
                image::Rgb([30, 30, 30]),
            ][k as usize]
        }
    })
}

fn write_jpeg(path: &std::path::Path, img: &image::RgbImage, icc: Option<Vec<u8>>) {
    use image::ImageEncoder;
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 100);
    if let Some(p) = icc {
        enc.set_icc_profile(p).unwrap();
    }
    enc.write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )
    .unwrap();
    std::fs::write(path, out).unwrap();
}

/// What the file decodes to (JPEG is lossy; compare against the decoded pixels, not the source).
fn decoded(path: &std::path::Path) -> image::RgbImage {
    image::open(path).unwrap().to_rgb8()
}

#[test]
fn untouched_jpeg_renders_as_it_went_in() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("a.jpg");
    write_jpeg(&path, &pixels(128, 96), None);
    let frame = pe_raw::decode_file(&path).unwrap();
    assert!(frame.rendered);
    let (temp, tint) = pe_pipeline::wb::temperature_of(&frame.xyz_to_cam, frame.wb).unwrap();
    // D65 sits slightly off the Planckian locus, hence a small tint.
    assert!(
        (temp - 6504.0).abs() < 150.0 && tint.abs() < 15.0,
        "as shot {temp} K {tint}"
    );
    let out = render(&frame, &Settings::default()).unwrap();
    let want = decoded(&path);
    let worst = out
        .data
        .iter()
        .zip(want.as_raw())
        .map(|(a, b)| ((a * 255.0).round() as i32 - *b as i32).abs())
        .max()
        .unwrap();
    assert!(worst <= 1, "differs by {worst} levels");
}

#[test]
fn display_p3_jpeg_is_converted_not_reinterpreted() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("p3.jpg");
    write_jpeg(
        &path,
        &pixels(128, 96),
        Some(pe_colour::icc::profile(ColorSpace::DisplayP3)),
    );
    let frame = pe_raw::decode_file(&path).unwrap();
    let out = render(&frame, &Settings::default()).unwrap();
    // Expected: the P3 values converted to sRGB.
    let m = ColorSpace::Srgb.from_space(&ColorSpace::DisplayP3.to_xyz_d65());
    let want = decoded(&path);
    let mut worst = 0;
    for (o, p) in out.data.chunks(3).zip(want.pixels()) {
        let lin = p.0.map(|v| ColorSpace::DisplayP3.decode(v as f32 / 255.0));
        let s = pe_colour::apply(&m, lin);
        if s.iter().any(|v| !(0.0..=1.0).contains(v)) {
            continue; // out of sRGB: gamut-mapped, not compared
        }
        for c in 0..3 {
            let e = (ColorSpace::Srgb.encode(s[c]) * 255.0).round() as i32;
            worst = worst.max(((o[c] * 255.0).round() as i32 - e).abs());
        }
    }
    assert!(worst <= 1, "differs by {worst} levels");
    // And the P3 frame is not the same as reading the numbers as sRGB.
    assert_eq!(
        pe_raw::space_of_icc(&pe_colour::icc::profile(ColorSpace::DisplayP3)),
        ColorSpace::DisplayP3
    );
}

#[test]
fn edits_act_in_linear_light() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("a.jpg");
    write_jpeg(&path, &pixels(128, 96), None);
    let frame = pe_raw::decode_file(&path).unwrap();
    let base = pe_pipeline::render_display(&frame, &Settings::default()).unwrap();
    let up: Settings = serde_json::from_value(serde_json::json!({"exposure": 1.0})).unwrap();
    let brighter = pe_pipeline::render_display(&frame, &up).unwrap();
    // +1 stop doubles display-linear values where nothing clips.
    for (a, b) in base.data.iter().zip(&brighter.data) {
        if *a > 0.02 && *a < 0.4 {
            assert!((b / a - 2.0).abs() < 0.02, "{a} -> {b}");
        }
    }
    // Whites moves the white point; Blacks lifts / crushes the bottom.
    let w: Settings = serde_json::from_value(serde_json::json!({"whites": 50})).unwrap();
    let ww = pe_pipeline::render_display(&frame, &w).unwrap();
    assert!(ww.data.iter().sum::<f32>() > base.data.iter().sum::<f32>());
}
