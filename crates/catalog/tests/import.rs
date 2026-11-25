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
