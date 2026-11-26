use std::path::{Path, PathBuf};

use pe_catalog::{Catalog, DevelopCommand as Cmd, Group, ImportOptions, Query, Settings};

fn jpeg(path: &Path, seed: u8) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    image::RgbImage::from_fn(40, 30, |x, y| image::Rgb([seed, x as u8 * 6, y as u8 * 8]))
        .save(path)
        .unwrap();
}

/// A catalog with `n` imported JPEGs; returns (tempdir guard, catalog, image ids).
fn setup(n: u8) -> (tempfile::TempDir, Catalog, Vec<i64>) {
    let t = tempfile::tempdir().unwrap();
    for i in 0..n {
        jpeg(&t.path().join(format!("p/{i}.jpg")), i);
    }
    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    cat.import(
        &[t.path().join("p")],
        &ImportOptions::default(),
        &mut |_| {},
    )
    .unwrap();
    let ids = cat
        .images(&Query::default())
        .unwrap()
        .iter()
        .map(|i| i.id)
        .collect();
    (t, cat, ids)
}

fn set(cat: &mut Catalog, id: i64, f: impl FnOnce(&mut Settings)) {
    let mut s = cat.develop_settings(id).unwrap();
    f(&mut s);
    cat.develop_execute(
        &[id],
        &Cmd::Set {
            settings: s,
            label: None,
        },
    )
    .unwrap();
}

fn labels(cat: &Catalog, id: i64) -> Vec<String> {
    cat.develop_state(id)
        .unwrap()
        .history
        .into_iter()
        .map(|h| h.label)
        .collect()
}

#[test]
fn history_undo_redo_and_truncation() {
    let (_t, mut cat, ids) = setup(1);
    let id = ids[0];
    let st = cat.develop_state(id).unwrap();
    assert!(!st.edited && st.history.is_empty() && !st.can_undo);

    set(&mut cat, id, |s| s.exposure = 0.5);
    set(&mut cat, id, |s| s.shadows = 20.0);
    assert_eq!(
        labels(&cat, id),
        ["Original", "Exposure +0.50 (+0.50)", "Shadows +20 (+20)"]
    );

    // A no-op commit adds nothing.
    set(&mut cat, id, |_| {});
    assert_eq!(labels(&cat, id).len(), 3);

    let st = cat.develop_execute(&[id], &Cmd::Undo).unwrap().remove(0);
    assert_eq!((st.settings.exposure, st.settings.shadows), (0.5, 0.0));
    assert!(st.can_undo && st.can_redo);
    let st = cat.develop_execute(&[id], &Cmd::Undo).unwrap().remove(0);
    assert!(!st.edited && !st.can_undo);
    let st = cat.develop_execute(&[id], &Cmd::Redo).unwrap().remove(0);
    assert_eq!(st.settings.exposure, 0.5);

    // Editing after an undo discards the redo branch.
    set(&mut cat, id, |s| s.contrast = 30.0);
    assert_eq!(
        labels(&cat, id),
        ["Original", "Exposure +0.50 (+0.50)", "Contrast +30 (+30)"]
    );
    assert!(!cat.develop_state(id).unwrap().can_redo);

    // Jump straight to a step.
    let first = cat.develop_state(id).unwrap().history[0].id;
    let st = cat
        .develop_execute(&[id], &Cmd::GoToStep { step_id: first })
        .unwrap()
        .remove(0);
    assert!(!st.edited);

    let st = cat
        .develop_execute(&[id], &Cmd::ClearHistory)
        .unwrap()
        .remove(0);
    assert!(st.history.is_empty() && !st.can_undo);
}

#[test]
fn paste_and_sync_copy_only_selected_groups() {
    let (_t, mut cat, ids) = setup(3);
    set(&mut cat, ids[1], |s| s.blacks = -10.0);
    let src = Settings {
        exposure: 1.0,
        contrast: 40.0,
        ..Default::default()
    };
    cat.develop_execute(
        &ids[1..],
        &Cmd::Paste {
            settings: src,
            groups: vec![Group::Exposure],
            label: None,
        },
    )
    .unwrap();
    for &id in &ids[1..] {
        let s = cat.develop_settings(id).unwrap();
        assert_eq!((s.exposure, s.contrast), (1.0, 0.0));
        assert_eq!(*labels(&cat, id).last().unwrap(), "Paste Settings");
    }
    assert_eq!(
        cat.develop_settings(ids[1]).unwrap().blacks,
        -10.0,
        "other values kept"
    );
    assert!(
        !cat.develop_state(ids[0]).unwrap().edited,
        "unselected image untouched"
    );
}

#[test]
fn presets_save_apply_replace_delete() {
    let (_t, mut cat, ids) = setup(1);
    let look = Settings {
        contrast: 25.0,
        exposure: 2.0,
        ..Default::default()
    };
    let p = cat
        .save_preset("Punchy", &[Group::Contrast], &look)
        .unwrap();
    assert!(cat.save_preset("", &[Group::Contrast], &look).is_err());

    set(&mut cat, ids[0], |s| s.exposure = -0.5);
    let st = cat
        .develop_execute(
            &ids,
            &Cmd::ApplyPreset {
                preset_id: p.id,
                amount: 1.0,
            },
        )
        .unwrap()
        .remove(0);
    assert_eq!(
        (st.settings.contrast, st.settings.exposure),
        (25.0, -0.5),
        "only contrast applied"
    );
    assert_eq!(st.history.last().unwrap().label, "Preset: Punchy");

    // Same name replaces.
    let p2 = cat
        .save_preset("Punchy", &[Group::Contrast, Group::Exposure], &look)
        .unwrap();
    assert_eq!(p2.id, p.id);
    assert_eq!(cat.presets().unwrap().len(), 1);
    assert_eq!(
        cat.presets().unwrap()[0].groups,
        [Group::Contrast, Group::Exposure]
    );
    cat.delete_preset(p.id).unwrap();
    assert!(cat.presets().unwrap().is_empty());
}

#[test]
fn snapshots_and_reset() {
    let (_t, mut cat, ids) = setup(1);
    let id = ids[0];
    set(&mut cat, id, |s| s.highlights = -40.0);
    let st = cat
        .develop_execute(
            &[id],
            &Cmd::CreateSnapshot {
                name: "Soft".into(),
            },
        )
        .unwrap()
        .remove(0);
    let snap = st.snapshots[0].id;
    let st = cat.develop_execute(&[id], &Cmd::Reset).unwrap().remove(0);
    assert!(!st.edited);
    let st = cat
        .develop_execute(&[id], &Cmd::ApplySnapshot { snapshot_id: snap })
        .unwrap()
        .remove(0);
    assert_eq!(st.settings.highlights, -40.0);
    assert_eq!(st.history.last().unwrap().label, "Snapshot: Soft");
    let st = cat
        .develop_execute(&[id], &Cmd::DeleteSnapshot { snapshot_id: snap })
        .unwrap()
        .remove(0);
    assert!(st.snapshots.is_empty());
}

#[test]
fn commands_roundtrip_as_json() {
    let cmds = [
        r#"{"command":"set","settings":{"exposure":0.3}}"#,
        r#"{"command":"paste","settings":{},"groups":["exposure","whiteBalance"]}"#,
        r#"{"command":"applyPreset","presetId":3}"#,
        r#"{"command":"goToStep","stepId":9}"#,
        r#"{"command":"undo"}"#,
        r#"{"command":"createSnapshot","name":"A"}"#,
    ];
    for c in cmds {
        let parsed: Cmd = serde_json::from_str(c).unwrap_or_else(|e| panic!("{c}: {e}"));
        let back: Cmd = serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(parsed, back);
    }
}

#[test]
fn edited_raw_gets_a_rendered_thumbnail() {
    let raw = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/raw/Nikon-D750.nef");
    if !raw.exists() {
        eprintln!("skip: samples not downloaded");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    cat.import(&[raw], &ImportOptions::default(), &mut |_| {})
        .unwrap();
    let id = cat.images(&Query::default()).unwrap()[0].id;
    let before = cat.thumbnail(id).unwrap().unwrap();
    set(&mut cat, id, |s| s.exposure = 2.0);
    let after = cat.thumbnail(id).unwrap().unwrap();
    assert_ne!(before, after, "thumbnail reflects the edit");
    let mean = |jpeg: &[u8]| {
        let img = image::load_from_memory(jpeg).unwrap().to_rgb8();
        img.pixels().map(|p| p[1] as f64).sum::<f64>() / (img.width() * img.height()) as f64
    };
    assert!(
        mean(&after) > mean(&before) + 20.0,
        "+2 EV is clearly brighter"
    );
}
