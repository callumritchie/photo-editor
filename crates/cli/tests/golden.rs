//! Golden-image regression test.
//!
//! Each downloaded sample is rendered with default settings, downscaled to a 256 px long edge, and
//! compared with `fixtures/golden/<id>.tif` in CIELAB. Re-baseline deliberately with:
//!
//! ```sh
//! PE_UPDATE_GOLDEN=1 cargo test --release -p pe-cli --test golden
//! ```
//!
//! Goldens are derived from CC0 sample files, so they are public-domain-compatible to commit.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

const LONG_EDGE: usize = 256;
const MAX_MEAN_DE: f32 = 1.0;
const MAX_P99_DE: f32 = 4.0;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Area-average downscale of encoded RGB so the long edge is at most `LONG_EDGE`; returns 8-bit RGB.
fn thumbnail(img: &pe_pipeline::RgbImage) -> (usize, usize, Vec<u8>) {
    let s = img.width.max(img.height).div_ceil(LONG_EDGE).max(1);
    let (w, h) = (img.width / s, img.height / s);
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 3];
            for yy in y * s..(y + 1) * s {
                for xx in x * s..(x + 1) * s {
                    let i = (yy * img.width + xx) * 3;
                    for (a, v) in acc.iter_mut().zip(&img.data[i..i + 3]) {
                        *a += v;
                    }
                }
            }
            for v in acc {
                out.push((v / (s * s) as f32 * 255.0 + 0.5).clamp(0.0, 255.0) as u8);
            }
        }
    }
    (w, h, out)
}

fn write_tiff8(path: &Path, w: usize, h: usize, rgb: &[u8]) {
    use tiff::encoder::{colortype::RGB8, TiffEncoder};
    let mut enc = TiffEncoder::new(BufWriter::new(File::create(path).unwrap())).unwrap();
    enc.write_image::<RGB8>(w as u32, h as u32, rgb).unwrap();
}

fn read_tiff8(path: &Path) -> (usize, usize, Vec<u8>) {
    let mut dec = tiff::decoder::Decoder::new(File::open(path).unwrap()).unwrap();
    let (w, h) = dec.dimensions().unwrap();
    match dec.read_image().unwrap() {
        tiff::decoder::DecodingResult::U8(v) => (w as usize, h as usize, v),
        _ => panic!("{}: expected 8-bit RGB", path.display()),
    }
}

fn delta_e_stats(a: &[u8], b: &[u8]) -> (f32, f32) {
    let to_lab = |p: &[u8]| {
        pe_colour::srgb_to_lab([
            p[0] as f32 / 255.0,
            p[1] as f32 / 255.0,
            p[2] as f32 / 255.0,
        ])
    };
    let mut de: Vec<f32> = a
        .chunks(3)
        .zip(b.chunks(3))
        .map(|(x, y)| pe_colour::delta_e76(to_lab(x), to_lab(y)))
        .collect();
    let mean = de.iter().sum::<f32>() / de.len() as f32;
    de.sort_by(|x, y| x.total_cmp(y));
    (mean, de[(de.len() as f32 * 0.99) as usize])
}

#[test]
fn golden_images() {
    let root = root();
    let update = std::env::var_os("PE_UPDATE_GOLDEN").is_some();
    let golden_dir = root.join("fixtures/golden");
    std::fs::create_dir_all(&golden_dir).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("fixtures/samples.json")).unwrap())
            .unwrap();
    let mut failures = Vec::new();
    for s in manifest["samples"].as_array().unwrap() {
        let (id, file) = (s["id"].as_str().unwrap(), s["file"].as_str().unwrap());
        let raw = root.join("fixtures/raw").join(file);
        if !raw.exists() {
            eprintln!("skip (not downloaded): {id}");
            continue;
        }
        let frame = pe_raw::decode_file(&raw).unwrap();
        let img = pe_pipeline::render(&frame, &pe_pipeline::Settings::default()).unwrap();
        let (w, h, thumb) = thumbnail(&img);
        let golden = golden_dir.join(format!("{id}.tif"));
        if update {
            write_tiff8(&golden, w, h, &thumb);
            eprintln!("updated {}", golden.display());
            continue;
        }
        assert!(
            golden.exists(),
            "missing golden for {id}; run with PE_UPDATE_GOLDEN=1"
        );
        let (gw, gh, g) = read_tiff8(&golden);
        if (gw, gh) != (w, h) {
            failures.push(format!("{id}: size {w}x{h} != golden {gw}x{gh}"));
            continue;
        }
        let (mean, p99) = delta_e_stats(&thumb, &g);
        eprintln!("{id}: mean dE {mean:.3}, p99 dE {p99:.3}");
        if mean > MAX_MEAN_DE || p99 > MAX_P99_DE {
            failures.push(format!(
                "{id}: mean dE {mean:.2} (max {MAX_MEAN_DE}), p99 {p99:.2} (max {MAX_P99_DE})"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "golden mismatches:\n{}",
        failures.join("\n")
    );
}
