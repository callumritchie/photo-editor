//! GPU render timings: `cargo run --release -p pe-gpu --example bench -- FILE [LONG_EDGE]`.
use std::time::Instant;

use pe_pipeline::{prepare_preview, Settings};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: bench FILE [LONG_EDGE]");
    let long_edge: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2048);
    let t = Instant::now();
    let gpu = pe_gpu::Gpu::new().expect("no GPU");
    println!("{} (init {:.0?})", gpu.name(), t.elapsed());
    let frame = pe_raw::decode_file(path).unwrap();
    let preview = prepare_preview(&frame, long_edge);
    let t = Instant::now();
    let gf = gpu.upload(&preview).unwrap();
    println!("upload {}x{}: {:.1?}", gf.width(), gf.height(), t.elapsed());
    for (label, s) in [
        ("defaults", Settings::default()),
        (
            "shadows/highlights",
            Settings {
                shadows: 40.0,
                highlights: -40.0,
                ..Default::default()
            },
        ),
    ] {
        let mut best = f64::MAX;
        for i in 0..10 {
            let s = Settings {
                exposure: i as f32 * 0.1,
                ..s.clone()
            };
            let t = Instant::now();
            gpu.render(&gf, &s).unwrap();
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!("render {label:<20} {best:>6.1} ms (best of 10)");
    }
}
