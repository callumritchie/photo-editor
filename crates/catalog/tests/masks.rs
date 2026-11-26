use std::path::PathBuf;

use pe_catalog::*;

#[test]
fn raster_masks_are_stored_and_used_by_renders_and_exports() {
    let raw = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/raw/Nikon-D750.nef");
    if !raw.exists() {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("p");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::copy(&raw, src.join("a.nef")).unwrap();
    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    cat.import(&[src], &ImportOptions::default(), &mut |_| {})
        .unwrap();
    let id = cat.images(&Query::default()).unwrap()[0].id;

    // A raster covering the left half of the frame.
    let (w, h) = (64u32, 43u32);
    let data: Vec<u8> = (0..w * h)
        .map(|i| if i % w < w / 2 { 255 } else { 0 })
        .collect();
    let rid = cat.save_raster(id, w, h, &data).unwrap();
    assert!(
        cat.save_raster(id, w, h, &data[1..]).is_err(),
        "size checked"
    );

    let masks: Vec<pe_pipeline::masks::Mask> = serde_json::from_value(serde_json::json!([{
        "id": "left", "components": [{"shape": {"type": "raster", "rasterId": rid}}],
        "adjust": {"exposure": 2.0}
    }]))
    .unwrap();
    let settings = Settings {
        masks,
        ..Default::default()
    };
    cat.develop_execute(
        &[id],
        &DevelopCommand::Set {
            settings,
            label: None,
        },
    )
    .unwrap();
    assert_eq!(cat.render_settings(id).unwrap().rasters.len(), 1);

    let out = t.path().join("out");
    let r = cat
        .export(
            &[id],
            &ExportSettings {
                destination: out,
                resize: pe_export::Resize::LongEdge { pixels: 400 },
                sharpen: None,
                ..Default::default()
            },
            &mut |_| {},
        )
        .unwrap();
    assert!(r.failed.is_empty(), "{r:?}");
    let img = image::open(&r.written[0]).unwrap().to_luma8();
    let mean = |x0: u32, x1: u32| {
        let mut s = 0u64;
        let mut n = 0u64;
        for y in 0..img.height() {
            for x in x0..x1 {
                s += img.get_pixel(x, y)[0] as u64;
                n += 1;
            }
        }
        s as f64 / n as f64
    };
    let (iw, _) = img.dimensions();
    // Compare with an unmasked export of the same frame region.
    let plain = t.path().join("plain");
    cat.develop_execute(&[id], &DevelopCommand::Reset).unwrap();
    let r2 = cat
        .export(
            &[id],
            &ExportSettings {
                destination: plain,
                resize: pe_export::Resize::LongEdge { pixels: 400 },
                sharpen: None,
                ..Default::default()
            },
            &mut |_| {},
        )
        .unwrap();
    let base = image::open(&r2.written[0]).unwrap().to_luma8();
    let base_mean = |x0: u32, x1: u32| {
        let mut s = 0u64;
        let mut n = 0u64;
        for y in 0..base.height() {
            for x in x0..x1 {
                s += base.get_pixel(x, y)[0] as u64;
                n += 1;
            }
        }
        s as f64 / n as f64
    };
    let left_gain = mean(0, iw / 2 - 8) / base_mean(0, iw / 2 - 8);
    let right_gain = mean(iw / 2 + 8, iw) / base_mean(iw / 2 + 8, iw);
    assert!(left_gain > 1.5, "masked half brighter: {left_gain}");
    assert!(
        (right_gain - 1.0).abs() < 0.05,
        "other half unchanged: {right_gain}"
    );
}
