//! A 1:1 region of a real raw matches the same part of a full-resolution render.
use std::path::PathBuf;

use pe_pipeline::region::{crop_frame, plan_region};

#[test]
fn region_of_a_real_raw_matches_full_render() {
    let raw = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/raw/Nikon-D750.nef");
    if !raw.exists() {
        return;
    }
    let frame = pe_raw::decode_file(&raw).unwrap();
    let settings: pe_pipeline::Settings = serde_json::from_value(serde_json::json!({
        "exposure": 0.7, "shadows": 60, "highlights": -50, "clarity": 35, "texture": 25, "dehaze": 20,
        "noiseReduction": {"luminance": 40, "color": 40}, "sharpening": {"amount": 50},
        "vignette": {"amount": -30}, "grain": {"amount": 20},
        "crop": {"left": 0.05, "top": 0.05, "right": 0.95, "bottom": 0.95, "angle": 2}
    }))
    .unwrap();
    let full = pe_pipeline::render_display(&frame, &settings).unwrap();
    let globals = pe_pipeline::measure_globals(&frame, &settings).unwrap();
    let out = [full.width / 3, full.height / 3, 1200, 800];
    let (rect, region) = plan_region(&frame, &settings, out);
    assert!(
        rect[2] * rect[3] < frame.width * frame.height / 3,
        "region is small: {rect:?}"
    );
    let crop = crop_frame(&frame, rect[0], rect[1], rect[2], rect[3]);
    let part = pe_pipeline::render_region_display(&crop, &settings, &region, &globals).unwrap();
    let mut worst = 0.0f32;
    for y in 0..out[3] {
        for x in 0..out[2] {
            for c in 0..3 {
                let a = part.data[(y * out[2] + x) * 3 + c];
                let b = full.data[((y + out[1]) * full.width + x + out[0]) * 3 + c];
                worst = worst.max((a - b).abs());
            }
        }
    }
    assert!(worst < 2e-3, "worst difference {worst} (region {rect:?})");
}
