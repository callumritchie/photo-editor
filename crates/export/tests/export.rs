use std::path::{Path, PathBuf};

use pe_colour::ColorSpace;
use pe_export::*;

fn sample() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/raw/Nikon-D750.nef");
    p.exists().then_some(p)
}

fn item(path: &Path) -> ExportItem {
    ExportItem {
        source: path.to_path_buf(),
        settings: pe_pipeline::Settings {
            exposure: 0.3,
            ..Default::default()
        },
        meta: PhotoMeta {
            make: Some("Nikon".into()),
            model: Some("D750".into()),
            capture_time: Some("2017-01-15T21:38:43".into()),
            iso: Some(800),
            ..Default::default()
        },
    }
}

/// Area-average an encoded RGB f32 image down to w x h.
fn shrink(data: &[f32], sw: usize, sh: usize, w: usize, h: usize) -> Vec<f32> {
    let mut out = vec![0.0; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let (x0, x1) = (x * sw / w, ((x + 1) * sw / w).max(x * sw / w + 1));
            let (y0, y1) = (y * sh / h, ((y + 1) * sh / h).max(y * sh / h + 1));
            let n = ((x1 - x0) * (y1 - y0)) as f32;
            for c in 0..3 {
                let mut s = 0.0;
                for yy in y0..y1 {
                    for xx in x0..x1 {
                        s += data[(yy * sw + xx) * 3 + c];
                    }
                }
                out[(y * w + x) * 3 + c] = s / n;
            }
        }
    }
    out
}

#[test]
fn jpeg_export_has_size_metadata_profile_and_matches_render() {
    let Some(raw) = sample() else { return };
    let t = tempfile::tempdir().unwrap();
    let s = ExportSettings {
        destination: t.path().to_path_buf(),
        file_naming: "{date}_{name}".into(),
        resize: Resize::LongEdge { pixels: 1024 },
        sharpen: None,
        copyright: Some("(c) Test".into()),
        ..Default::default()
    };
    let it = item(&raw);
    let report = export(std::slice::from_ref(&it), &s, &mut |_| {});
    assert!(report.failed.is_empty(), "{report:?}");
    let out = PathBuf::from(&report.written[0]);
    assert_eq!(out.file_name().unwrap(), "2017-01-15_Nikon-D750.jpg");

    let bytes = std::fs::read(&out).unwrap();
    assert!(
        bytes.windows(12).any(|w| w == b"ICC_PROFILE\0"),
        "ICC embedded"
    );
    let exif = ::exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(&bytes))
        .unwrap();
    let model = exif
        .get_field(::exif::Tag::Model, ::exif::In::PRIMARY)
        .unwrap();
    assert_eq!(model.display_value().to_string(), "\"D750\"");

    let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
    assert_eq!(img.width(), 1024);

    // Same pipeline, rendered directly in sRGB then shrunk: the export must match closely.
    let frame = pe_raw::decode_file(&raw).unwrap();
    let full = pe_pipeline::render(&frame, &it.settings).unwrap();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let reference = shrink(&full.data, full.width, full.height, w, h);
    let mut de: Vec<f32> = img
        .pixels()
        .zip(reference.chunks(3))
        .map(|(p, r)| {
            let a = pe_colour::srgb_to_lab([
                p[0] as f32 / 255.0,
                p[1] as f32 / 255.0,
                p[2] as f32 / 255.0,
            ]);
            pe_colour::delta_e76(a, pe_colour::srgb_to_lab([r[0], r[1], r[2]]))
        })
        .collect();
    de.sort_by(|a, b| a.total_cmp(b));
    let mean = de.iter().sum::<f32>() / de.len() as f32;
    let p95 = de[de.len() * 95 / 100];
    assert!(mean < 2.0 && p95 < 5.0, "mean dE {mean}, p95 {p95}");
}

#[test]
fn wide_gamut_png_and_tiff_and_original_copy() {
    let Some(raw) = sample() else { return };
    let t = tempfile::tempdir().unwrap();
    let it = item(&raw);
    for (format, space, ext) in [
        (
            Format::Png { sixteen_bit: true },
            ColorSpace::ProPhoto,
            "png",
        ),
        (
            Format::Tiff {
                sixteen_bit: true,
                compression: TiffCompression::Lzw,
            },
            ColorSpace::AdobeRgb,
            "tif",
        ),
    ] {
        let s = ExportSettings {
            destination: t.path().to_path_buf(),
            format,
            color_space: space,
            resize: Resize::LongEdge { pixels: 600 },
            ..Default::default()
        };
        let r = export(std::slice::from_ref(&it), &s, &mut |_| {});
        assert!(r.failed.is_empty(), "{r:?}");
        assert!(r.written[0].ends_with(ext));
    }
    // TIFF round-trip with the tiff crate.
    let tif = std::fs::read_dir(t.path())
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().unwrap() == "tif")
        .unwrap();
    let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(tif.path()).unwrap()).unwrap();
    assert_eq!(dec.dimensions().unwrap().0, 600);

    // Original: byte-identical copy; second export with Skip writes nothing.
    let s = ExportSettings {
        destination: t.path().join("orig"),
        format: Format::Original,
        ..Default::default()
    };
    let r = export(std::slice::from_ref(&it), &s, &mut |_| {});
    assert_eq!(
        std::fs::read(&r.written[0]).unwrap(),
        std::fs::read(&raw).unwrap()
    );
    let skip = ExportSettings {
        collision: Collision::Skip,
        ..s
    };
    let r2 = export(std::slice::from_ref(&it), &skip, &mut |_| {});
    assert_eq!((r2.written.len(), r2.skipped.len()), (0, 1));
}

#[test]
fn settings_roundtrip_as_json() {
    let s = ExportSettings {
        format: Format::Tiff {
            sixteen_bit: true,
            compression: TiffCompression::Deflate,
        },
        resize: Resize::Fit {
            width: 2000,
            height: 1500,
        },
        color_space: ColorSpace::DisplayP3,
        ..Default::default()
    };
    let json = serde_json::to_string(&s).unwrap();
    let back: ExportSettings = serde_json::from_str(&json).unwrap();
    assert_eq!(back, s);
    let partial: ExportSettings =
        serde_json::from_str(r#"{"format":{"type":"jpeg","quality":80}}"#).unwrap();
    assert_eq!(partial.format, Format::Jpeg { quality: 80 });
    assert_eq!(partial.file_naming, "{name}");
}
