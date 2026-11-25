use std::path::PathBuf;

use pe_catalog::{Catalog, DevelopCommand, ImportOptions, Query, RenderStores, Settings};

fn develop_set(cat: &mut Catalog, id: i64, s: Settings) {
    cat.develop_execute(
        &[id],
        &DevelopCommand::Set {
            settings: s,
            label: None,
        },
    )
    .unwrap();
}

#[test]
fn previews_are_rendered_cached_and_invalidated_by_edits() {
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
    let mut stores = RenderStores::open(cat.dir()).unwrap();

    // Unedited: the camera thumbnail is ready at once; no preview yet.
    let (thumb, needs) = cat.thumbnail_quick(id).unwrap();
    assert!(thumb.is_some() && !needs);
    assert!(cat.preview(id).unwrap().is_none());
    let job = cat.render_job(id).unwrap().unwrap();
    assert!(!stores.is_fresh(&job).unwrap());
    assert!(
        !stores.run(&job, None).unwrap(),
        "unedited photos keep the camera thumbnail"
    );
    assert!(stores.is_fresh(&job).unwrap());
    let (jpeg, fresh) = cat.preview(id).unwrap().unwrap();
    assert!(fresh);
    let img = image::load_from_memory(&jpeg).unwrap();
    assert_eq!(img.width().max(img.height()), 2048);

    // Edited: both levels go stale, the old images are still returned, and one job refreshes them.
    develop_set(
        &mut cat,
        id,
        Settings {
            exposure: 1.0,
            ..Default::default()
        },
    );
    assert!(!cat.preview(id).unwrap().unwrap().1);
    let (stale, needs) = cat.thumbnail_quick(id).unwrap();
    assert!(stale.is_some() && needs);
    let job = cat.render_job(id).unwrap().unwrap();
    assert!(!stores.is_fresh(&job).unwrap());
    assert!(stores.run(&job, None).unwrap());
    assert!(cat.preview(id).unwrap().unwrap().1);
    let (fresh_thumb, needs) = cat.thumbnail_quick(id).unwrap();
    assert!(!needs && fresh_thumb != stale);

    // The brighter edit really is brighter.
    let mean = |j: &[u8]| {
        let i = image::load_from_memory(j).unwrap().to_luma8();
        i.pixels().map(|p| p[0] as f64).sum::<f64>() / (i.width() * i.height()) as f64
    };
    let (before, after) = (mean(&jpeg), mean(&cat.preview(id).unwrap().unwrap().0));
    assert!(after > before * 1.3, "mean {before} -> {after}");
}
