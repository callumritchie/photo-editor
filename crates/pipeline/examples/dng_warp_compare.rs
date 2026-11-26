//! A DNG rendered without (top) and with (bottom) its own WarpRectilinear lens correction, reduced:
//! `cargo run --release -p pe-pipeline --example dng_warp_compare -- FILE.dng OUT.ppm`.
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let full = pe_raw::decode_file(&a[0]).unwrap();
    let with = pe_pipeline::prepare_preview(&full, 900);
    let mut without = with.clone();
    without.dng_warp = None;
    let s = pe_pipeline::Settings::default();
    let g = pe_pipeline::geometry::GeometryParams::for_frame(&s, &with, with.width, with.height);
    eprintln!(
        "warp {:?}\nframe {}x{}, fill zoom {}",
        with.dng_warp, with.width, with.height, g.pfit
    );
    for (x, y) in [
        (0, 0),
        (with.width - 1, 0),
        (0, with.height / 2),
        (with.width / 2, 0),
    ] {
        eprintln!("output ({x},{y}) samples {:?}", g.source(x, y, 1));
    }
    let (t, b) = (
        pe_pipeline::render(&without, &s).unwrap(),
        pe_pipeline::render(&with, &s).unwrap(),
    );
    let w = t.width.min(b.width);
    let mut out = format!("P6\n{} {}\n255\n", w, t.height + b.height + 6).into_bytes();
    rows(&mut out, &t, w);
    out.extend(std::iter::repeat_n(255u8, w * 3 * 6));
    rows(&mut out, &b, w);
    std::fs::write(&a[1], out).unwrap();
}

fn rows(out: &mut Vec<u8>, img: &pe_pipeline::RgbImage, w: usize) {
    for y in 0..img.height {
        for x in 0..w {
            let i = (y * img.width + x) * 3;
            out.extend((0..3).map(|c| (img.data[i + c] * 255.0 + 0.5) as u8));
        }
    }
}
