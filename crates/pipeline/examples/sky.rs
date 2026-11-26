//! Select Sky on a raw file and write the mask as an 8-bit PGM: `sky FILE OUT.pgm`.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let frame = pe_raw::decode_file(&a[1]).unwrap();
    let p = pe_pipeline::prepare_preview(&frame, 1024);
    let out = pe_pipeline::render(&p, &pe_pipeline::Settings::default()).unwrap();
    let t = std::time::Instant::now();
    let m = pe_pipeline::select::select_sky(&out.data, out.width, out.height);
    eprintln!("{}x{} in {:.1?}", m.width, m.height, t.elapsed());
    let mut bytes = format!("P5\n{} {}\n255\n", m.width, m.height).into_bytes();
    bytes.extend(m.data.iter().map(|v| (v * 255.0) as u8));
    std::fs::write(&a[2], bytes).unwrap();
}
