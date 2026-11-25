//! `pe`: command-line front end to the photo-editor core, used for development and testing.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pe", version, about = "photo-editor development CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print what the decoder sees in a raw file.
    Info { input: PathBuf },
    /// Render a raw file to a 16-bit sRGB TIFF using default (or given) settings.
    Render {
        input: PathBuf,
        output: PathBuf,
        /// Exposure adjustment in stops.
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        exposure: f32,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Info { input } => info(&input),
        Cmd::Render {
            input,
            output,
            exposure,
        } => {
            let settings = pe_pipeline::Settings {
                exposure_ev: exposure,
            };
            render(&input, &output, &settings)
        }
    }
}

fn info(input: &Path) -> Result<()> {
    let f = pe_raw::decode_file(input).with_context(|| format!("decoding {}", input.display()))?;
    println!("camera       {} {}", f.make, f.model);
    println!("size         {} x {}", f.width, f.height);
    match &f.layout {
        pe_raw::Layout::Mosaic(cfa) => {
            println!("layout       mosaic {}x{} CFA", cfa.width, cfa.height)
        }
        pe_raw::Layout::LinearRgb => println!("layout       linear RGB"),
    }
    println!("as-shot wb   {:.4} {:.4} {:.4}", f.wb[0], f.wb[1], f.wb[2]);
    println!("orientation  {}", f.orientation);
    Ok(())
}

fn render(input: &Path, output: &Path, settings: &pe_pipeline::Settings) -> Result<()> {
    let t0 = Instant::now();
    let frame =
        pe_raw::decode_file(input).with_context(|| format!("decoding {}", input.display()))?;
    let t1 = Instant::now();
    let img = pe_pipeline::render_linear(&frame, settings)?;
    let pixels = pe_pipeline::encode_srgb16(&img);
    let t2 = Instant::now();
    write_tiff16(output, img.width, img.height, &pixels)?;
    eprintln!(
        "{} {} {}x{}: decode {:?}, render {:?}, write {:?}",
        frame.make,
        frame.model,
        img.width,
        img.height,
        t1 - t0,
        t2 - t1,
        t2.elapsed()
    );
    Ok(())
}

fn write_tiff16(path: &Path, width: usize, height: usize, rgb: &[u16]) -> Result<()> {
    use tiff::encoder::{colortype::RGB16, TiffEncoder};
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut enc = TiffEncoder::new(BufWriter::new(file))?;
    enc.write_image::<RGB16>(width as u32, height as u32, rgb)?;
    Ok(())
}
