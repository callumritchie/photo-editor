//! Stage timings: `cargo run --release -p pe-pipeline --example bench -- FILE [LONG_EDGE]`.
use std::time::Instant;

use pe_pipeline::{prepare_preview, render, render_display, render_scene, Settings};

fn time<T>(label: &str, runs: usize, mut f: impl FnMut() -> T) -> T {
    let mut best = f64::MAX;
    let mut out = None;
    for _ in 0..runs {
        let t = Instant::now();
        out = Some(f());
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
    }
    println!("{label:<40} {best:>8.1} ms");
    out.unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: bench FILE [LONG_EDGE]");
    let long_edge: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2048);
    let frame = time("decode", 1, || pe_raw::decode_file(path).unwrap());
    println!(
        "  {} x {} ({:.1} MP)",
        frame.width,
        frame.height,
        (frame.width * frame.height) as f64 / 1e6
    );
    let preview = time(&format!("prepare_preview({long_edge})"), 1, || {
        prepare_preview(&frame, long_edge)
    });
    println!("  preview {} x {}", preview.width, preview.height);
    let plain = Settings::default();
    let local = Settings {
        shadows: 40.0,
        highlights: -40.0,
        exposure: 0.3,
        ..Default::default()
    };
    time("preview render (defaults)", 5, || {
        render(&preview, &plain).unwrap()
    });
    time("preview render_scene", 5, || {
        render_scene(&preview, &local).unwrap()
    });
    time("preview render (shadows/highlights)", 5, || {
        render(&preview, &local).unwrap()
    });
    if let pe_raw::Layout::Mosaic(cfa) = &frame.layout {
        time("full demosaic only", 1, || {
            pe_pipeline::demosaic::high_quality(&frame.data, frame.width, frame.height, cfa)
        });
    }
    time("full render_display (shadows/highlights)", 1, || {
        render_display(&frame, &local).unwrap()
    });
}
