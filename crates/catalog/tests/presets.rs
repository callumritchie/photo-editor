//! Presets: editing, our file format, Adobe-style XMP import, amount, applying to many photos and at
//! import.
use std::path::Path;

use pe_catalog::{Catalog, DevelopCommand as Cmd, Group, ImportOptions, Query, Settings};

fn jpeg(path: &Path, seed: u8) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    image::RgbImage::from_fn(40, 30, |x, y| image::Rgb([seed, x as u8 * 6, y as u8 * 8]))
        .save(path)
        .unwrap();
}

fn setup(n: u8) -> (tempfile::TempDir, Catalog, Vec<i64>) {
    let t = tempfile::tempdir().unwrap();
    for i in 0..n {
        jpeg(&t.path().join(format!("p/{i}.jpg")), i * 30);
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

fn look() -> Settings {
    serde_json::from_value(serde_json::json!({
        "exposure": 1.0, "contrast": 40, "vibrance": 30,
        "toneCurve": [[0.0, 0.05], [0.5, 0.55], [1.0, 1.0]],
        "treatment": "monochrome"
    }))
    .unwrap()
}

#[test]
fn edit_duplicate_and_file_round_trip() {
    let (_t, mut cat, ids) = setup(1);
    let p = cat
        .save_preset("Warm", &[Group::Exposure, Group::Contrast], &look())
        .unwrap();
    let other = cat
        .save_preset("Other", &[Group::Exposure], &look())
        .unwrap();
    // Rename, regroup, and take values from a photo.
    let mut s = look();
    s.exposure = -0.5;
    let p = cat
        .update_preset(p.id, "Warmer", &[Group::Exposure, Group::Color], Some(&s))
        .unwrap();
    assert_eq!(p.name, "Warmer");
    assert_eq!(p.groups, vec![Group::Exposure, Group::Color]);
    assert_eq!(p.settings.exposure, -0.5);
    // Name clash and empty groups are refused; nothing changes.
    assert!(cat
        .update_preset(p.id, "Other", &[Group::Exposure], None)
        .is_err());
    assert!(cat.update_preset(p.id, "Warmer", &[], None).is_err());
    let d = cat.duplicate_preset(p.id).unwrap();
    assert_eq!(d.name, "Warmer copy");
    assert_eq!(d.settings.exposure, -0.5);

    // Export -> import gives an equal preset (only its groups' values travel).
    let text = cat.export_preset(p.id).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["format"], "photo-editor-preset");
    assert!(
        v["settings"].get("contrast").is_none(),
        "only included groups: {text}"
    );
    let imported = cat.import_preset(&text, "Warmer.pepreset").unwrap();
    assert_eq!(imported.preset.name, "Warmer (2)");
    assert_eq!(imported.preset.groups, p.groups);
    let apply = |cat: &mut Catalog, id| {
        cat.develop_execute(&ids, &Cmd::Reset).unwrap();
        cat.develop_execute(
            &ids,
            &Cmd::ApplyPreset {
                preset_id: id,
                amount: 1.0,
            },
        )
        .unwrap();
        cat.develop_settings(ids[0]).unwrap()
    };
    assert_eq!(apply(&mut cat, p.id), apply(&mut cat, imported.preset.id));
    let _ = other;
    // Newer versions and other files are refused.
    let newer = text.replace("\"version\": 1", "\"version\": 2");
    assert!(cat
        .import_preset(&newer, "x.pepreset")
        .unwrap_err()
        .to_string()
        .contains("version 2"));
    assert!(cat.import_preset("{}", "x.pepreset").is_err());
    assert!(cat.import_preset("hello", "x.xmp").is_err());
}

#[test]
fn amount_blends_and_preview_does_not_commit() {
    let (_t, mut cat, ids) = setup(3);
    let p = cat
        .save_preset(
            "Look",
            &[
                Group::Exposure,
                Group::Contrast,
                Group::ToneCurve,
                Group::BlackWhite,
            ],
            &look(),
        )
        .unwrap();
    let id = ids[0];
    let mut base = cat.develop_settings(id).unwrap();
    base.exposure = 0.2;
    cat.develop_execute(
        &[id],
        &Cmd::Set {
            settings: base.clone(),
            label: None,
        },
    )
    .unwrap();

    // Preview: the would-be settings, nothing committed.
    let steps = cat.develop_state(id).unwrap().history.len();
    let half = cat.preview_preset(id, p.id, 0.5).unwrap();
    assert!((half.exposure - 0.6).abs() < 1e-5, "{}", half.exposure);
    assert!((half.contrast - 20.0).abs() < 1e-4);
    assert_eq!(cat.develop_state(id).unwrap().history.len(), steps);
    assert_eq!(cat.develop_settings(id).unwrap(), base);
    // 0 %: unchanged; 100 %: a plain apply; 200 %: twice as far, clamped.
    assert_eq!(cat.preview_preset(id, p.id, 0.0).unwrap(), base);
    let full = cat.preview_preset(id, p.id, 1.0).unwrap();
    assert_eq!(full.exposure, 1.0);
    assert_eq!(full.tone_curve, look().tone_curve);
    let double = cat.preview_preset(id, p.id, 2.0).unwrap();
    assert!((double.exposure - 1.8).abs() < 1e-5 && (double.contrast - 80.0).abs() < 1e-4);
    // Curves with a different number of points and choices are taken as they are.
    assert_eq!(half.tone_curve, look().tone_curve);
    assert_eq!(half.treatment, pe_pipeline::Treatment::Monochrome);

    // Applying at 50 % to several photos at once, labelled with the amount.
    let states = cat
        .develop_execute(
            &ids,
            &Cmd::ApplyPreset {
                preset_id: p.id,
                amount: 0.5,
            },
        )
        .unwrap();
    assert_eq!(states.len(), 3);
    for s in &states {
        assert!(
            s.history.last().unwrap().label.contains("Look (50%)"),
            "{:?}",
            s.history.last()
        );
    }
    assert!((cat.develop_settings(ids[1]).unwrap().exposure - 0.5).abs() < 1e-5);
}

#[test]
fn preset_applied_at_import() {
    let (t, mut cat, _) = setup(1);
    let p = cat
        .save_preset("Bright", &[Group::Exposure], &look())
        .unwrap();
    for i in 0..3 {
        jpeg(&t.path().join(format!("q/{i}.jpg")), 100 + i);
    }
    let opts = ImportOptions {
        develop_preset: Some(p.id),
        ..Default::default()
    };
    cat.import(&[t.path().join("q")], &opts, &mut |_| {})
        .unwrap();
    let new: Vec<i64> = cat
        .images(&Query::default())
        .unwrap()
        .iter()
        .filter(|i| i.path.contains("/q/"))
        .map(|i| i.id)
        .collect();
    assert_eq!(new.len(), 3);
    for id in new {
        let st = cat.develop_state(id).unwrap();
        assert_eq!(st.settings.exposure, 1.0);
        assert!(st.history.iter().any(|h| h.label == "Preset: Bright"));
    }
    // A missing preset fails before importing anything.
    jpeg(&t.path().join("r/0.jpg"), 7);
    let bad = ImportOptions {
        develop_preset: Some(9999),
        ..Default::default()
    };
    assert!(cat
        .import(&[t.path().join("r")], &bad, &mut |_| {})
        .is_err());
    assert!(!cat
        .images(&Query::default())
        .unwrap()
        .iter()
        .any(|i| i.path.contains("/r/")));
}

#[test]
fn adobe_style_xmp_presets_map_onto_our_settings() {
    let (_t, mut cat, _) = setup(1);
    let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    crs:PresetType="Normal" crs:Version="15.0" crs:ProcessVersion="11.0"
    crs:Exposure2012="+0.50" crs:Contrast2012="+25" crs:Shadows2012="+40"
    crs:Vibrance="+15" crs:HueAdjustmentOrange="-10" crs:SaturationAdjustmentBlue="-30"
    crs:ColorGradeShadowHue="210" crs:ColorGradeShadowSat="20" crs:ColorGradeBlending="60"
    crs:Sharpness="40" crs:SharpenRadius="+1.0" crs:LuminanceSmoothing="10"
    crs:PostCropVignetteAmount="-15" crs:GrainAmount="12"
    crs:WhiteBalance="Custom" crs:Temperature="5600" crs:Tint="+8"
    crs:CameraProfile="Some Profile" crs:LensProfileEnable="1" crs:ShadowTint="0">
   <crs:Name><rdf:Alt><rdf:li xml:lang="x-default">Soft Film</rdf:li></rdf:Alt></crs:Name>
   <crs:Clarity2012>+12</crs:Clarity2012>
   <crs:ToneCurvePV2012><rdf:Seq><rdf:li>0, 20</rdf:li><rdf:li>128, 132</rdf:li><rdf:li>255, 245</rdf:li></rdf:Seq></crs:ToneCurvePV2012>
   <crs:ToneCurvePV2012Red><rdf:Seq><rdf:li>0, 0</rdf:li><rdf:li>255, 240</rdf:li></rdf:Seq></crs:ToneCurvePV2012Red>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
    let r = cat.import_preset(xmp, "film.xmp").unwrap();
    let s = &r.preset.settings;
    assert_eq!(r.preset.name, "Soft Film");
    assert_eq!(s.exposure, 0.5);
    assert_eq!(
        (s.contrast, s.shadows, s.clarity, s.vibrance),
        (25.0, 40.0, 12.0, 15.0)
    );
    assert_eq!((s.hsl.hue[1], s.hsl.saturation[5]), (-10.0, -30.0));
    assert_eq!(
        (
            s.grading.shadows.hue,
            s.grading.shadows.saturation,
            s.grading.blending
        ),
        (210.0, 20.0, 60.0)
    );
    assert_eq!((s.sharpening.amount, s.sharpening.radius), (40.0, 1.0));
    assert_eq!(
        (
            s.noise_reduction.luminance,
            s.vignette.amount,
            s.grain.amount
        ),
        (10.0, -15.0, 12.0)
    );
    assert_eq!(
        s.white_balance,
        pe_pipeline::WhiteBalance::Custom {
            temperature: 5600.0,
            tint: 8.0
        }
    );
    assert_eq!(s.tone_curve.len(), 3);
    assert!((s.tone_curve[0][1] - 20.0 / 255.0).abs() < 1e-6);
    for g in [
        Group::Exposure,
        Group::Contrast,
        Group::Shadows,
        Group::Presence,
        Group::Color,
        Group::ColorMixer,
        Group::ColorGrading,
        Group::ToneCurve,
        Group::Sharpening,
        Group::NoiseReduction,
        Group::Vignette,
        Group::Grain,
        Group::WhiteBalance,
    ] {
        assert!(
            r.preset.groups.contains(&g),
            "{g:?} missing from {:?}",
            r.preset.groups
        );
    }
    assert!(!r.preset.groups.contains(&Group::Highlights));
    // Reported: the profile, lens profile and red curve; not bookkeeping or neutral values.
    assert_eq!(
        r.unmapped,
        vec!["CameraProfile", "LensProfileEnable", "ToneCurvePV2012Red"]
    );
    // No settings we can use: refused.
    assert!(cat
        .import_preset(r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="r"><rdf:Description crs:Version="1" xmlns:crs="c"/></rdf:RDF></x:xmpmeta>"#, "empty.xmp")
        .is_err());
}
