//! Side-by-side 1:1 crop of an X-Trans raw demosaiced by the old plain averaging (left) and the
//! directional method (right): `cargo run --release -p pe-pipeline --example xtrans_compare -- RAW X Y OUT.ppm`.
use pe_pipeline::demosaic::{basic_xtrans, high_quality};
use pe_raw::Layout;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let f = pe_raw::decode_file(&a[0]).unwrap();
    let Layout::Mosaic(cfa) = &f.layout else {
        panic!("not a mosaic")
    };
    let (x0, y0) = (
        a[1].parse::<usize>().unwrap() / 6 * 6,
        a[2].parse::<usize>().unwrap() / 6 * 6,
    );
    let (cw, ch) = (300, 300);
    let crop: Vec<f32> = (0..ch)
        .flat_map(|y| (0..cw).map(move |x| (x, y)))
        .map(|(x, y)| f.data[(y0 + y) * f.width + x0 + x])
        .collect();
    let cfa = cfa.shifted(x0, y0);
    let show = |rgb: &[f32]| -> Vec<u8> {
        rgb.chunks(3)
            .flat_map(|p| {
                let wb = [f.wb[0] * p[0], p[1], f.wb[2] * p[2]];
                wb.map(|v| ((v * 2.5).clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8)
            })
            .collect()
    };
    let (l, r) = (
        show(&basic_xtrans(&crop, cw, ch, &cfa)),
        show(&high_quality(&crop, cw, ch, &cfa)),
    );
    let mut out = format!("P6\n{} {}\n255\n", cw * 2 + 4, ch).into_bytes();
    for y in 0..ch {
        out.extend(&l[y * cw * 3..(y + 1) * cw * 3]);
        out.extend([255u8; 12]);
        out.extend(&r[y * cw * 3..(y + 1) * cw * 3]);
    }
    std::fs::write(&a[3], out).unwrap();
}
