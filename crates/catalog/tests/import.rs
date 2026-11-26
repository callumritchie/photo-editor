use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use pe_catalog::{Catalog, Flag, ImportMode, ImportOptions, Query};

/// Write a small, distinct JPEG with a fixed modification time.
fn jpeg(path: &Path, seed: u8, mtime_secs: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let img = image::RgbImage::from_fn(64, 48, |x, y| {
        image::Rgb([seed, (x * 4) as u8, (y * 5) as u8])
    });
    img.save(path).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(mtime_secs))
        .unwrap();
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            stack.extend(fs::read_dir(&p).unwrap().flatten().map(|e| e.path()));
        } else {
            out.push(p);
        }
    }
    out
}

fn noop(_: &pe_catalog::ImportProgress) {}

#[test]
fn add_import_then_reimport_is_idempotent() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("photos");
    jpeg(&src.join("a.jpg"), 1, 1_600_000_000);
    jpeg(&src.join("trip/b.jpg"), 2, 1_600_000_100);
    jpeg(&src.join("trip/day2/c.JPG"), 3, 1_600_000_200);
    fs::write(src.join("notes.txt"), "ignored").unwrap();

    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    let r = cat
        .import(
            std::slice::from_ref(&src),
            &ImportOptions::default(),
            &mut noop,
        )
        .unwrap();
    assert_eq!(
        (r.found, r.imported, r.duplicates, r.thumbnails_failed),
        (3, 3, 0, 0)
    );
    let folders: Vec<String> = cat
        .folders()
        .unwrap()
        .into_iter()
        .map(|f| f.rel_path)
        .collect();
    assert_eq!(folders, ["", "trip", "trip/day2"]);
    assert!(cat
        .thumbnail(cat.images(&Query::default()).unwrap()[0].id)
        .unwrap()
        .is_some());

    let r2 = cat
        .import(&[src], &ImportOptions::default(), &mut noop)
        .unwrap();
    assert_eq!((r2.imported, r2.already_in_catalog), (0, 3));
    assert_eq!(cat.image_count().unwrap(), 3);
}

#[test]
fn duplicates_are_detected_by_quick_hash() {
    let t = tempfile::tempdir().unwrap();
    jpeg(&t.path().join("card1/IMG_1.jpg"), 7, 1_650_000_000);
    fs::create_dir_all(t.path().join("card2")).unwrap();
    fs::copy(
        t.path().join("card1/IMG_1.jpg"),
        t.path().join("card2/IMG_1.jpg"),
    )
    .unwrap();
    fs::File::options()
        .write(true)
        .open(t.path().join("card2/IMG_1.jpg"))
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_650_000_000))
        .unwrap();

    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    let r = cat
        .import(
            &[t.path().join("card1"), t.path().join("card2")],
            &ImportOptions::default(),
            &mut noop,
        )
        .unwrap();
    assert_eq!((r.imported, r.duplicates), (1, 1));
}

#[test]
fn copy_into_dated_folders_preserving_mtime() {
    let t = tempfile::tempdir().unwrap();
    // 2021-06-26T22:17:11Z
    jpeg(&t.path().join("card/DSC_1.jpg"), 9, 1_624_745_831);
    let dest = t.path().join("Pictures");
    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    let opts = ImportOptions {
        mode: ImportMode::Copy {
            dest: dest.clone(),
            template: "%Y/%Y-%m-%d".into(),
        },
        ..Default::default()
    };
    let r = cat
        .import(&[t.path().join("card")], &opts, &mut noop)
        .unwrap();
    assert_eq!(r.imported, 1);
    let copied = dest.join("2021/2021-06-26/DSC_1.jpg");
    assert!(copied.exists());
    let mtime = fs::metadata(&copied).unwrap().modified().unwrap();
    assert_eq!(
        mtime.duration_since(UNIX_EPOCH).unwrap().as_secs(),
        1_624_745_831
    );
    let img = &cat.images(&Query::default()).unwrap()[0];
    assert_eq!(
        fs::canonicalize(&img.path).unwrap(),
        fs::canonicalize(&copied).unwrap()
    );
}

#[test]
fn crash_mid_import_then_resume() {
    let t = tempfile::tempdir().unwrap();
    for i in 0..10u8 {
        jpeg(
            &t.path().join(format!("card/IMG_{i:02}.jpg")),
            i,
            1_600_000_000 + i as u64,
        );
    }
    let dest = t.path().join("lib-files");
    let catdir = t.path().join("lib.pecat");
    let opts = ImportOptions {
        mode: ImportMode::Copy {
            dest: dest.clone(),
            template: "%Y".into(),
        },
        batch_size: 3,
        simulate_crash_in_batch: Some(2),
        ..Default::default()
    };
    {
        let mut cat = Catalog::create(&catdir).unwrap();
        let err = cat.import(&[t.path().join("card")], &opts, &mut noop);
        assert!(err.is_err(), "simulated crash should abort");
    } // drop = process "dies"

    let mut cat = Catalog::open(&catdir).unwrap();
    assert_eq!(cat.integrity_check().unwrap(), "ok");
    assert_eq!(
        cat.image_count().unwrap(),
        6,
        "batches 0 and 1 committed, batch 2 not"
    );

    let resume = ImportOptions {
        simulate_crash_in_batch: None,
        ..opts
    };
    let r = cat
        .import(&[t.path().join("card")], &resume, &mut noop)
        .unwrap();
    assert_eq!((r.imported, r.duplicates), (4, 6));
    assert_eq!(cat.image_count().unwrap(), 10);
    let on_disk = files_under(&dest);
    assert_eq!(on_disk.len(), 10, "no duplicate copies: {on_disk:?}");
    assert!(on_disk
        .iter()
        .all(|p| !p.to_string_lossy().contains("pe-partial")));
}

#[test]
fn rating_flag_label_and_filters() {
    let t = tempfile::tempdir().unwrap();
    for i in 0..4u8 {
        jpeg(
            &t.path().join(format!("p/{i}.jpg")),
            i,
            1_600_000_000 + i as u64,
        );
    }
    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    cat.import(&[t.path().join("p")], &ImportOptions::default(), &mut noop)
        .unwrap();
    let ids: Vec<i64> = cat
        .images(&Query::default())
        .unwrap()
        .iter()
        .map(|i| i.id)
        .collect();
    cat.set_rating(&ids[..2], 4).unwrap();
    cat.set_flag(&ids[1..2], Flag::Picked).unwrap();
    cat.set_flag(&ids[3..], Flag::Rejected).unwrap();
    cat.set_label(&ids[2..3], Some("green")).unwrap();
    assert!(cat.set_rating(&ids, 6).is_err());
    assert!(cat.set_label(&ids, Some("pink")).is_err());

    assert_eq!(
        cat.images(&Query {
            min_rating: 3,
            ..Default::default()
        })
        .unwrap()
        .len(),
        2
    );
    assert_eq!(
        cat.images(&Query {
            flag: Some(1),
            ..Default::default()
        })
        .unwrap()[0]
            .id,
        ids[1]
    );
    assert_eq!(
        cat.images(&Query {
            flag: Some(-1),
            ..Default::default()
        })
        .unwrap()[0]
            .id,
        ids[3]
    );
    assert_eq!(
        cat.images(&Query {
            label: Some("green".into()),
            ..Default::default()
        })
        .unwrap()[0]
            .id,
        ids[2]
    );
}

#[test]
fn sample_raws_import_with_metadata_and_thumbnails() {
    let raw_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/raw");
    if !raw_dir.is_dir() || fs::read_dir(&raw_dir).unwrap().next().is_none() {
        eprintln!("skip: samples not downloaded");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    let r = cat
        .import(&[raw_dir], &ImportOptions::default(), &mut noop)
        .unwrap();
    assert!(r.imported >= 1 && r.failed.is_empty(), "{r:?}");
    assert_eq!(r.thumbnails_failed, 0, "{r:?}");
    for img in cat.images(&Query::default()).unwrap() {
        assert!(img.camera.is_some(), "{}: no camera", img.name);
        assert!(img.capture_time.is_some(), "{}: no capture time", img.name);
    }
}

/// A JPEG with a hand-built EXIF block: Make, Model, Orientation 6 (rotate 90° clockwise to view),
/// DateTimeOriginal, OffsetTimeOriginal and FNumber.
fn exif_jpeg(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let img = image::RgbImage::from_fn(64, 48, |x, y| {
        image::Rgb([200, (x * 4) as u8, (y * 5) as u8])
    });
    let mut plain = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut plain),
        image::ImageFormat::Jpeg,
    )
    .unwrap();

    // Little-endian TIFF: IFD0 then the EXIF IFD, values after each.
    type Entry = (u16, u16, u32, Vec<u8>);
    fn ifd(entries: &[Entry], start: usize, next_ifd: Option<usize>, out: &mut Vec<u8>) {
        let table_len = 2 + entries.len() * 12 + 4;
        let mut data = Vec::new();
        out.extend((entries.len() as u16).to_le_bytes());
        for (tag, typ, count, bytes) in entries {
            out.extend(tag.to_le_bytes());
            out.extend(typ.to_le_bytes());
            out.extend(count.to_le_bytes());
            if bytes.len() <= 4 {
                let mut b = bytes.clone();
                b.resize(4, 0);
                out.extend(b);
            } else {
                out.extend(((start + table_len + data.len()) as u32).to_le_bytes());
                data.extend(bytes);
                if data.len() % 2 == 1 {
                    data.push(0);
                }
            }
        }
        out.extend((next_ifd.unwrap_or(0) as u32).to_le_bytes());
        out.extend(data);
    }
    let ascii = |s: &str| {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        b
    };
    let exif_entries: Vec<Entry> = vec![
        (
            0x829D,
            5,
            1,
            [28u32.to_le_bytes(), 10u32.to_le_bytes()].concat(),
        ), // FNumber 2.8
        (0x9003, 2, 20, ascii("2024:06:01 10:20:30")),
        (0x9011, 2, 7, ascii("+01:00")),
    ];
    let mut ifd0_entries: Vec<Entry> = vec![
        (0x010F, 2, 9, ascii("FUJIFILM")),
        (0x0110, 2, 5, ascii("X-T3")),
        (0x0112, 3, 1, 6u16.to_le_bytes().to_vec()),
        (0x8769, 4, 1, vec![0; 4]), // EXIF IFD offset, filled in below
    ];
    let mut tiff = b"II*\0".to_vec();
    tiff.extend(8u32.to_le_bytes());
    // IFD0 size first (to place the EXIF IFD after it).
    let mut probe = Vec::new();
    ifd(&ifd0_entries, 8, None, &mut probe);
    let exif_at = 8 + probe.len();
    ifd0_entries[3].3 = (exif_at as u32).to_le_bytes().to_vec();
    ifd(&ifd0_entries, 8, None, &mut tiff);
    ifd(&exif_entries, exif_at, None, &mut tiff);

    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend(tiff);
    let mut out = plain[..2].to_vec(); // SOI
    out.extend([0xFF, 0xE1]);
    out.extend(((app1.len() + 2) as u16).to_be_bytes());
    out.extend(app1);
    out.extend(&plain[2..]);
    fs::write(path, out).unwrap();
}

#[test]
fn jpeg_exif_is_read_at_import_and_for_older_catalogs() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("photos");
    exif_jpeg(&src.join("rotated.jpg"));
    let lib = t.path().join("lib.pecat");
    let mut cat = Catalog::create(&lib).unwrap();
    cat.import(&[src], &ImportOptions::default(), &mut noop)
        .unwrap();
    let check = |cat: &Catalog| {
        let img = &cat.images(&Query::default()).unwrap()[0];
        assert_eq!(img.orientation, 6);
        let camera = img.camera.as_deref().unwrap_or("");
        assert!(
            camera.contains("FUJIFILM") && camera.contains("X-T3"),
            "{camera}"
        );
        assert_eq!(img.capture_time.as_deref(), Some("2024-06-01T10:20:30"));
        assert!((img.f_number.unwrap() - 2.8).abs() < 1e-9);
        // The thumbnail is upright: 64 x 48 pixels turned on their side.
        let jpeg = cat.thumbnail(img.id).unwrap().unwrap();
        let th = image::load_from_memory(&jpeg).unwrap();
        assert!(th.height() > th.width(), "{}x{}", th.width(), th.height());
    };
    check(&cat);
    drop(cat);

    // A catalog from before JPEG EXIF was read: blank the fields and forget the one-time pass.
    {
        let c = rusqlite::Connection::open(lib.join("catalog.sqlite")).unwrap();
        c.execute_batch(
            "UPDATE images SET orientation = 1, camera_make = NULL, camera_model = NULL,
                 capture_time = NULL, capture_offset = NULL, f_number = NULL;
             DELETE FROM meta WHERE key = 'image_exif_read';",
        )
        .unwrap();
    }
    let cat = Catalog::open(&lib).unwrap();
    check(&cat);
}
