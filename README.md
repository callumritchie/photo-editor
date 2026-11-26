# photo-editor

An experimental, non-destructive raw photo editor and photo library for macOS. It has a Rust core and a SwiftUI shell.

This is a personal, non-commercial project, built in the open for learning. It is not a product and not affiliated with any camera or
software vendor.

## Status

**M0–M5 done.** There is a working Mac app (photo library, import, grid, an editing view with live GPU-rendered sliders, history, snapshots and presets, cached previews, and export), and the core decodes and renders raws.

**Rendering (M2).** The core decodes raw files from all major camera makes (Bayer and X-Trans sensors, and DNG)
and renders them on the CPU:

black/white levels → edge-directed demosaic with false-colour suppression → white balance (as-shot, auto, or temperature/tint) with
highlight reconstruction → wide-gamut (Rec.2020) scene-linear working space → exposure → local shadows/highlights → contrast →
filmic display transform (whites/blacks) → gamut-mapped sRGB → point curve → orientation.

See [`docs/PIPELINE.md`](docs/PIPELINE.md) for how each stage works and [`docs/ROADMAP.md`](docs/ROADMAP.md) for what comes next.

## Build and run

Requires a stable Rust toolchain (1.89+).

```sh
cargo build --release
scripts/fetch-samples.sh              # downloads CC0 sample raws into fixtures/raw/ (about 260 MB, SHA-256 verified)
./target/release/pe info fixtures/raw/Nikon-D750.nef
./target/release/pe render fixtures/raw/Nikon-D750.nef out.tif
./target/release/pe render fixtures/raw/Nikon-D750.nef out.tif \
    --exposure 0.5 --shadows 40 --highlights -60 --contrast 20 --wb 5600,5 --curve "0,0;64,52;192,206;255,255"
./target/release/pe settings > look.json && ./target/release/pe render in.raw out.tif --settings look.json
cargo test --workspace --release      # unit tests, sample renders, golden-image regression
```

Golden images (`fixtures/golden/`) are small renders of the CC0 samples. After an intended visual change, re-baseline with
`PE_UPDATE_GOLDEN=1 cargo test --release -p pe-cli --test golden` and review the diff.

## Mac app (M1)

```sh
app/build.sh                      # builds the Rust core + SwiftUI shell into app/build/PhotoEditor.app
open app/build/PhotoEditor.app    # library defaults to ~/Pictures/PhotoEditor Library.pecat
open --env PE_CATALOG=/path/Test.pecat app/build/PhotoEditor.app   # use another library
```

- **Import** (⇧⌘I) a folder: *Add* catalogs files in place; *Copy* puts them into dated folders (e.g. `%Y/%Y-%m-%d`).
  Suspected duplicates are skipped. Imports are crash-safe and resumable.
- **Grid:** folders in the sidebar, rating filter, thumbnail size. Keys: `0`–`5` rating, `P` pick, `X` reject, `U` unflag,
  `6`–`9` red/yellow/green/blue label, arrows to move, `Return`/`Space` to open in the loupe.
- **Edit (loupe):** `Return` on a photo opens it with the Develop panel: white balance (as shot / auto / custom temperature and tint),
  exposure, contrast, highlights, shadows, whites and blacks; vibrance and saturation; a colour mixer (hue, saturation and
  luminance for eight colour bands); black & white with a per-colour mix; colour grading for shadows, midtones, highlights
  and overall. Sliders update a live preview while you drag, rendered on the GPU (Metal)
  in about 5–15 ms; without a GPU the CPU takes 25–55 ms. Each release records one history step. ⌘Z/⇧⌘Z undo and redo; click any history step to return to it.
  You can also create snapshots, save and apply presets (choosing which groups to include), and copy, paste or sync settings to
  selected photos (⇧⌘C / ⇧⌘V). Edits are stored in the library, never in your files. Grid thumbnails update to show edits.
  `Esc` returns to the grid.
- **Previews:** the loupe opens on a cached 2048 px preview straight away, then switches to the live render. While you view a
  photo, the ones either side are rendered in the background; *File › Build Previews* renders the selection (or all shown).
  Edited grid thumbnails are re-rendered in the background too. Cached previews live in `previews.sqlite` inside the library
  (1 GB budget, least recently used dropped first); deleting that file is safe.
- **Export** (⇧⌘E) the selected photos: JPEG, PNG or TIFF (8 or 16-bit) or the original file; sRGB, Display P3, Adobe RGB
  or ProPhoto with an embedded ICC profile; resize by long/short edge, width, height, fit, megapixels or percentage; output
  sharpening for screen, matte or glossy paper; metadata (all, copyright only, or none); file names from tokens such as
  `{date}_{seq:3}`. Settings can be saved as export presets. Rendering happens in the background; when it finishes,
  *Show in Finder* reveals the files.

Command-line equivalents: `pe new`, `pe import [--copy-to DIR --template "%Y/%Y-%m-%d"]`, `pe ls`, `pe folders`, `pe thumb`,
`pe develop LIB ID --command '{"command":"set","settings":{"exposure":0.5}}'` (also `undo`, `redo`, `reset`, `applyPreset`, …),
`pe export LIB [IDS…] --to DIR [--format jpeg|png|tiff|original --space srgb|p3|adobe|prophoto --long-edge 2048 --name "{date}_{seq:3}"]`
(or `--settings FILE` with JSON from `pe export-settings`).

## Layout

| Crate | Purpose |
|---|---|
| `crates/raw` (`pe-raw`) | Raw decoding into a decoder-independent `RawFrame` |
| `crates/colour` (`pe-colour`) | Matrix maths, primaries, transfer functions, output colour spaces, ICC profile writer |
| `crates/pipeline` (`pe-pipeline`) | CPU reference render pipeline (demosaic and later stages) |
| `crates/gpu` (`pe-gpu`) | GPU (wgpu/Metal) version of the develop pipeline, tested against the CPU reference |
| `crates/develop` (`pe-develop`) | Editing model: slider schema, groups, settings digest, history labels |
| `crates/catalog` (`pe-catalog`) | Photo library: SQLite catalog, import, queries, ratings/flags/labels, develop history/snapshots/presets |
| `crates/preview` (`pe-preview`) | Thumbnails and quick previews; rendered preview cache comes in M4 |
| `crates/export` (`pe-export`) | Export: resizing, output sharpening, JPEG/PNG/TIFF writers with ICC profiles and EXIF, file naming |
| `crates/ffi` (`pe-ffi`) | C interface used by the Mac app |
| `crates/cli` (`pe`) | Development command-line tool |
| `app/` | SwiftUI Mac app (Swift package + `build.sh`) |

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
