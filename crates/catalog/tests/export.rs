use pe_catalog::{Catalog, ExportSettings, ImportOptions, Query};
use pe_export::{Format, Resize};

#[test]
fn export_from_catalog_and_presets() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("p");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..3u8 {
        image::RgbImage::from_pixel(120, 80, image::Rgb([i * 60, 100, 200]))
            .save(src.join(format!("IMG_{i}.jpg")))
            .unwrap();
    }
    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    cat.import(&[src], &ImportOptions::default(), &mut |_| {})
        .unwrap();
    let ids: Vec<i64> = cat
        .images(&Query::default())
        .unwrap()
        .iter()
        .map(|i| i.id)
        .collect();

    let settings = ExportSettings {
        destination: t.path().join("out"),
        file_naming: "web_{seq:2}".into(),
        format: Format::Png { sixteen_bit: false },
        resize: Resize::LongEdge { pixels: 60 },
        ..Default::default()
    };
    let mut seen = 0;
    let report = cat.export(&ids, &settings, &mut |p| seen = p.done).unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(seen, 3);
    let names: Vec<String> = report
        .written
        .iter()
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, ["web_01.png", "web_02.png", "web_03.png"]);
    assert_eq!(image::open(&report.written[0]).unwrap().width(), 60);

    let p = cat.save_export_preset("Web", &settings).unwrap();
    assert_eq!(cat.export_presets().unwrap()[0].settings, settings);
    assert!(cat.save_export_preset(" ", &settings).is_err());
    cat.delete_export_preset(p.id).unwrap();
    assert!(cat.export_presets().unwrap().is_empty());
}
