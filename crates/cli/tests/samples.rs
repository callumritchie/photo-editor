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
        let frame =
            pe_raw::decode_file(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            frame.width > 1000 && frame.height > 1000,
            "{}: tiny image",
            path.display()
        );
        let img = pe_pipeline::render_linear(&frame, &pe_pipeline::Settings::default()).unwrap();
        assert!(
            img.data.iter().all(|v| v.is_finite()),
            "{}: non-finite pixels",
            path.display()
        );

        // The mean of a normal photo, in linear light, should be neither black nor blown.
        let mean: f64 = img
            .data
            .iter()
            .map(|&v| v.clamp(0.0, 1.0) as f64)
            .sum::<f64>()
            / img.data.len() as f64;
        assert!(
            (0.005..0.9).contains(&mean),
            "{}: implausible mean {mean}",
            path.display()
        );
        rendered += 1;
    }
    eprintln!("rendered {rendered} samples");
}
