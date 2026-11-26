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

#[test]
fn keywords_are_embedded_as_xmp_in_every_format() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("p");
    std::fs::create_dir_all(&src).unwrap();
    image::RgbImage::from_pixel(64, 48, image::Rgb([90, 120, 160]))
        .save(src.join("a.jpg"))
        .unwrap();
    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    cat.import(&[src], &ImportOptions::default(), &mut |_| {})
        .unwrap();
    let id = cat.images(&Query::default()).unwrap()[0].id;
    cat.add_keywords(&[id], &["Animals > Birds".into(), "R&D".into()])
        .unwrap();
    for (format, ext) in [
        (Format::Jpeg { quality: 90 }, "jpg"),
        (Format::Png { sixteen_bit: false }, "png"),
        (
            Format::Tiff {
                sixteen_bit: false,
                compression: pe_export::TiffCompression::Lzw,
            },
            "tif",
        ),
    ] {
        let out = t.path().join(ext);
        let s = ExportSettings {
            destination: out,
            format,
            artist: Some("Ann".into()),
            ..Default::default()
        };
        let r = cat.export(&[id], &s, &mut |_| {}).unwrap();
        let bytes = std::fs::read(&r.written[0]).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        for k in [
            "<rdf:li>Animals</rdf:li>",
            "<rdf:li>Birds</rdf:li>",
            "<rdf:li>R&amp;D</rdf:li>",
            "<rdf:li>Ann</rdf:li>",
        ] {
            assert!(text.contains(k), "{ext}: {k} missing");
        }
        // Still a valid image.
        assert!(image::load_from_memory(&bytes).is_ok() || ext == "tif");
        let none = ExportSettings {
            metadata: pe_export::MetadataMode::None,
            destination: t.path().join(format!("{ext}-none")),
            ..s
        };
        let r = cat.export(&[id], &none, &mut |_| {}).unwrap();
        assert!(
            !String::from_utf8_lossy(&std::fs::read(&r.written[0]).unwrap()).contains("dc:subject")
        );
    }
}

#[test]
fn edited_jpeg_exports_with_its_edits() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("p");
    std::fs::create_dir_all(&src).unwrap();
    image::RgbImage::from_pixel(120, 80, image::Rgb([60, 60, 60]))
        .save(src.join("a.jpg"))
        .unwrap();
    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    cat.import(&[src], &ImportOptions::default(), &mut |_| {})
        .unwrap();
    let id = cat.images(&Query::default()).unwrap()[0].id;
    let out = |cat: &Catalog, name: &str| {
        let settings = ExportSettings {
            destination: t.path().join(name),
            format: Format::Png { sixteen_bit: false },
            ..Default::default()
        };
        let r = cat.export(&[id], &settings, &mut |_| {}).unwrap();
        image::open(&r.written[0]).unwrap().to_rgb8()
    };
    // Untouched: the same pixels (JPEG decodes 60 -> about 60).
    let plain = out(&cat, "plain");
    assert!((plain.get_pixel(60, 40).0[1] as i32 - 60).abs() <= 2);
    // +1 stop and a crop: brighter (linear x2 -> sRGB 60 becomes ~ 86) and smaller.
    let mut s = cat.develop_settings(id).unwrap();
    s.exposure = 1.0;
    s.crop.right = 0.5;
    cat.develop_execute(
        &[id],
        &pe_catalog::DevelopCommand::Set {
            settings: s,
            label: None,
        },
    )
    .unwrap();
    let edited = out(&cat, "edited");
    assert_eq!(edited.width(), 60);
    let v = edited.get_pixel(30, 40).0[1] as i32;
    assert!((v - 86).abs() <= 3, "{v}");
}
