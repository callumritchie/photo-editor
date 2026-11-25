//! `pe`: command-line front end to the photo-editor core, used for development and testing.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use pe_pipeline::{Demosaic, Settings, WhiteBalance};

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
    /// Render a raw file to a 16-bit sRGB TIFF.
    Render {
        input: PathBuf,
        output: PathBuf,
        #[command(flatten)]
        opts: RenderOpts,
    },
    /// Print the default settings as JSON (a starting point for --settings files).
    Settings,
    /// Create a new, empty catalog (a folder, conventionally named *.pecat).
    New { catalog: PathBuf },
    /// Import files or folders into a catalog.
    Import {
        catalog: PathBuf,
        sources: Vec<PathBuf>,
        /// Copy into this folder (organised by --template) instead of cataloguing in place.
        #[arg(long)]
        copy_to: Option<PathBuf>,
        /// Folder template for --copy-to, from capture date: %Y %m %d %H %M %S.
        #[arg(long, default_value = "%Y/%Y-%m-%d")]
        template: String,
        /// Import files even if they look like duplicates of catalogued files.
        #[arg(long)]
        allow_duplicates: bool,
        /// Only the given folders, not their subfolders.
        #[arg(long)]
        no_recurse: bool,
    },
    /// List images in a catalog.
    Ls {
        catalog: PathBuf,
        #[arg(long, default_value_t = 0)]
        min_rating: u8,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// List folders in a catalog with image counts.
    Folders { catalog: PathBuf },
    /// Run a develop command (JSON) on images and print their new state, e.g.
    /// `pe develop Lib.pecat 7 '{"command":"set","settings":{"exposure":0.5}}'`.
    Develop {
        catalog: PathBuf,
        ids: Vec<i64>,
        #[arg(long)]
        command: String,
    },
    /// Write an image's grid thumbnail (JPEG).
    Thumb {
        catalog: PathBuf,
        id: i64,
        output: PathBuf,
    },
}

#[derive(Args)]
struct RenderOpts {
    /// JSON settings file; individual flags below override it.
    #[arg(long)]
    settings: Option<PathBuf>,
    /// Exposure in stops.
    #[arg(long, allow_hyphen_values = true)]
    exposure: Option<f32>,
    /// -100..100
    #[arg(long, allow_hyphen_values = true)]
    contrast: Option<f32>,
    /// -100..100
    #[arg(long, allow_hyphen_values = true)]
    highlights: Option<f32>,
    /// -100..100
    #[arg(long, allow_hyphen_values = true)]
    shadows: Option<f32>,
    /// -100..100
    #[arg(long, allow_hyphen_values = true)]
    whites: Option<f32>,
    /// -100..100
    #[arg(long, allow_hyphen_values = true)]
    blacks: Option<f32>,
    /// White balance: "as-shot", "auto", or "<kelvin>[,<tint>]" e.g. "5500,10".
    #[arg(long)]
    wb: Option<String>,
    /// Point curve "x,y;x,y;..." on 0..255 display values.
    #[arg(long)]
    curve: Option<String>,
    /// Use the fast (soft) demosaic.
    #[arg(long)]
    fast: bool,
    /// Do not rotate/flip according to the camera orientation.
    #[arg(long)]
    no_orient: bool,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Info { input } => info(&input),
        Cmd::Render {
            input,
            output,
            opts,
        } => render(&input, &output, &settings_from(&opts)?),
        Cmd::Settings => {
            println!("{}", serde_json::to_string_pretty(&Settings::default())?);
            Ok(())
        }
        Cmd::New { catalog } => {
            pe_catalog::Catalog::create(&catalog)?;
            println!("created {}", catalog.display());
            Ok(())
        }
        Cmd::Import {
            catalog,
            sources,
            copy_to,
            template,
            allow_duplicates,
            no_recurse,
        } => {
            let mut cat = pe_catalog::Catalog::open(&catalog)?;
            let opts = pe_catalog::ImportOptions {
                mode: match copy_to {
                    Some(dest) => pe_catalog::ImportMode::Copy { dest, template },
                    None => pe_catalog::ImportMode::Add,
                },
                recursive: !no_recurse,
                skip_duplicates: !allow_duplicates,
                ..Default::default()
            };
            let t0 = Instant::now();
            let report = cat.import(&sources, &opts, &mut |p| {
                eprint!("\r{}/{}", p.done, p.total)
            })?;
            eprintln!();
            println!(
                "found {}, imported {}, duplicates {}, already in catalog {}, failed {}, missing thumbnails {} ({:.1?})",
                report.found,
                report.imported,
                report.duplicates,
                report.already_in_catalog,
                report.failed.len(),
                report.thumbnails_failed,
                t0.elapsed()
            );
            for (p, e) in &report.failed {
                println!("  failed: {p}: {e}");
            }
            Ok(())
        }
        Cmd::Ls {
            catalog,
            min_rating,
            json,
        } => {
            let cat = pe_catalog::Catalog::open(&catalog)?;
            let rows = cat.images(&pe_catalog::Query {
                min_rating,
                ..Default::default()
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for r in rows {
                    println!(
                        "{:>6}  {:<19}  {}{:<5}  {:<28}  {}",
                        r.id,
                        r.capture_time.as_deref().unwrap_or("-"),
                        "*".repeat(r.rating as usize),
                        "",
                        r.camera.as_deref().unwrap_or("-"),
                        r.path
                    );
                }
            }
            Ok(())
        }
        Cmd::Folders { catalog } => {
            for f in pe_catalog::Catalog::open(&catalog)?.folders()? {
                println!("{:>6}  {:>6}  {}", f.id, f.image_count, f.path);
            }
            Ok(())
        }
        Cmd::Develop {
            catalog,
            ids,
            command,
        } => {
            let mut cat = pe_catalog::Catalog::open(&catalog)?;
            let cmd: pe_catalog::DevelopCommand = serde_json::from_str(&command)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&cat.develop_execute(&ids, &cmd)?)?
            );
            Ok(())
        }
        Cmd::Thumb {
            catalog,
            id,
            output,
        } => {
            let cat = pe_catalog::Catalog::open(&catalog)?;
            let jpeg = cat.thumbnail(id)?.context("no thumbnail available")?;
            std::fs::write(&output, jpeg)?;
            Ok(())
        }
    }
}

fn settings_from(o: &RenderOpts) -> Result<Settings> {
    let mut s: Settings = match &o.settings {
        Some(p) => serde_json::from_str(
            &std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        )
        .with_context(|| format!("parsing {}", p.display()))?,
        None => Settings::default(),
    };
    let set = |dst: &mut f32, v: Option<f32>| {
        if let Some(v) = v {
            *dst = v
        }
    };
    set(&mut s.exposure, o.exposure);
    set(&mut s.contrast, o.contrast);
    set(&mut s.highlights, o.highlights);
    set(&mut s.shadows, o.shadows);
    set(&mut s.whites, o.whites);
    set(&mut s.blacks, o.blacks);
    if let Some(wb) = &o.wb {
        s.white_balance = parse_wb(wb)?;
    }
    if let Some(c) = &o.curve {
        s.tone_curve = parse_curve(c)?;
    }
    if o.fast {
        s.demosaic = Demosaic::Fast;
    }
    if o.no_orient {
        s.apply_orientation = false;
    }
    Ok(s)
}

fn parse_wb(s: &str) -> Result<WhiteBalance> {
    Ok(match s {
        "as-shot" | "asshot" => WhiteBalance::AsShot,
        "auto" => WhiteBalance::Auto,
        _ => {
            let mut it = s.split(',');
            let temperature: f32 = it
                .next()
                .unwrap_or("")
                .trim()
                .parse()
                .context("--wb kelvin")?;
            let tint: f32 = it
                .next()
                .map(|t| t.trim().parse())
                .transpose()
                .context("--wb tint")?
                .unwrap_or(0.0);
            WhiteBalance::Custom { temperature, tint }
        }
    })
}

fn parse_curve(s: &str) -> Result<Vec<[f32; 2]>> {
    s.split(';')
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let v: Vec<f32> = p
                .split(',')
                .map(|n| n.trim().parse())
                .collect::<Result<_, _>>()?;
            if v.len() != 2 {
                bail!("curve point must be x,y: {p:?}");
            }
            Ok([v[0], v[1]])
        })
        .collect()
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
    if let Some((t, tint)) = pe_pipeline::wb::temperature_of(&f.xyz_to_cam, f.wb) {
        println!("as-shot      {t:.0} K, tint {tint:+.0}");
    }
    println!("orientation  {}", f.orientation);
    Ok(())
}

fn render(input: &Path, output: &Path, settings: &Settings) -> Result<()> {
    let t0 = Instant::now();
    let frame =
        pe_raw::decode_file(input).with_context(|| format!("decoding {}", input.display()))?;
    let t1 = Instant::now();
    let img = pe_pipeline::render(&frame, settings)?;
    let pixels = pe_pipeline::to_u16(&img);
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

pub fn write_tiff16(path: &Path, width: usize, height: usize, rgb: &[u16]) -> Result<()> {
    use tiff::encoder::{colortype::RGB16, TiffEncoder};
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut enc = TiffEncoder::new(BufWriter::new(file))?;
    enc.write_image::<RGB16>(width as u32, height as u32, rgb)?;
    Ok(())
}
