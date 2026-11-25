# photo-editor

An experimental, non-destructive raw photo editor and photo library for macOS. It has a Rust core with a SwiftUI shell
(the shell is planned).

This is a personal, non-commercial project, built in the open for learning. It is not a product and not affiliated with any camera or
software vendor.

## Status

**M0 (foundations).** The core can decode raw files from all major camera makes (Bayer and X-Trans sensors, and DNG) and render them
to a 16-bit sRGB TIFF on the CPU. The pipeline is: black/white levels → demosaic → white balance → camera matrix → exposure → sRGB.
There is no tone curve yet, so renders look flat by design. See [`docs/ROADMAP.md`](docs/ROADMAP.md) for what comes next.

## Build and run

Requires a stable Rust toolchain (1.89+).

```sh
cargo build --release
scripts/fetch-samples.sh              # downloads CC0 sample raws into fixtures/raw/ (about 260 MB, SHA-256 verified)
./target/release/pe info fixtures/raw/Nikon-D750.nef
./target/release/pe render fixtures/raw/Nikon-D750.nef out.tif --exposure 1.0
cargo test --workspace --release      # includes rendering every downloaded sample
```

## Layout

| Crate | Purpose |
|---|---|
| `crates/raw` (`pe-raw`) | Raw decoding into a decoder-independent `RawFrame` |
| `crates/colour` (`pe-colour`) | Matrix maths, primaries, transfer functions, camera → output colour |
| `crates/pipeline` (`pe-pipeline`) | CPU reference render pipeline (demosaic and later stages) |
| `crates/catalog` (`pe-catalog`) | Photo library database (M1) |
| `crates/preview` (`pe-preview`) | Preview cache (M4) |
| `crates/export` (`pe-export`) | Export (M5) |
| `crates/cli` (`pe`) | Development command-line tool |

## Principles

- **Independent implementation.** All code here is original or comes from openly licensed libraries. No proprietary code, assets,
  presets, profiles or tuning data are used.
- **Interoperability from permitted sources only.** Compatibility with other applications' files (for example reading an existing
  photo catalog or XMP sidecars) is built only from public specifications, observation of the user's own data files, and open
  literature. Every such fact is recorded with its source in [`INTEROP.md`](INTEROP.md).
- **Test data is public domain.** Sample raws come from [raw.pixls.us](https://raw.pixls.us/) and are limited to files dedicated
  under CC0. They are downloaded on demand and never committed.

## Licence

GPL-3.0-or-later. See [`LICENSE`](LICENSE) and third-party notices in [`THIRD_PARTY.md`](THIRD_PARTY.md).

Adobe, Lightroom and DNG are trademarks of Adobe. Other camera and software names are trademarks of their respective owners and are
used only to describe compatibility.
