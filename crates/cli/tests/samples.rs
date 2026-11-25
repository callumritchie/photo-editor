//! Renders every downloaded CC0 sample (see `scripts/fetch-samples.sh`) and checks basic sanity.
//! Samples that are not present are skipped, so this passes on a fresh checkout.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn render_all_samples() {
    let root = workspace_root();
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("fixtures/samples.json")).unwrap())
            .unwrap();
    let mut rendered = 0;
    for s in manifest["samples"].as_array().unwrap() {
        let path = root.join("fixtures/raw").join(s["file"].as_str().unwrap());
        if !path.exists() {
            eprintln!("skip (not downloaded): {}", path.display());
            continue;
        }
        let name = path.display();
        let frame = pe_raw::decode_file(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            frame.width > 1000 && frame.height > 1000,
            "{name}: tiny image"
        );

        let settings = pe_pipeline::Settings::default();
        let scene = pe_pipeline::render_scene(&frame, &settings).unwrap();
        assert!(
            scene.data.iter().all(|v| v.is_finite()),
            "{name}: non-finite scene values"
        );
        let mean: f64 = scene
            .data
            .iter()
            .map(|&v| v.clamp(0.0, 1.0) as f64)
            .sum::<f64>()
            / scene.data.len() as f64;
        assert!(
            (0.005..0.9).contains(&mean),
            "{name}: implausible scene mean {mean}"
        );

        let out = pe_pipeline::render(&frame, &settings).unwrap();
        assert!(
            out.data.iter().all(|v| (0.0..=1.0).contains(v)),
            "{name}: output outside [0,1]"
        );
        let mean_out: f64 = out.data.iter().map(|&v| v as f64).sum::<f64>() / out.data.len() as f64;
        assert!(
            (0.05..0.95).contains(&mean_out),
            "{name}: implausible display mean {mean_out}"
        );
        rendered += 1;
    }
    eprintln!("rendered {rendered} samples");
}
