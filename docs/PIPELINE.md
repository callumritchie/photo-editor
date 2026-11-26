# Render pipeline

The CPU reference pipeline lives in `crates/pipeline`; the GPU version in `crates/gpu` is tested against it (see below).
All processing is 32-bit float. Values stay scene-linear and unclipped until the display transform.

| # | Stage | Module | Method |
|---|---|---|---|
| 1 | Decode | `pe-raw` | `rawler` decodes the file; we crop to the camera's recommended area, subtract per-position black levels and scale so saturation = 1.0 |
| 2 | Demosaic | `demosaic.rs` | **Bayer:** edge-directed green with Laplacian correction (Hamilton & Adams, US 5,629,734, expired), then red/blue as colour differences. **Other CFAs (X-Trans):** green by neighbourhood average, then colour differences. **Both:** 3×3 median on R−G and B−G to suppress false colour (Freeman 1988). `--fast` uses plain neighbourhood averaging |
| 3 | White balance | `wb.rs` | Camera multipliers from: as-shot; auto (grey-world over well-exposed pixels); or temperature/tint. Temperature/tint → CIE xy via the Planckian locus (Kim et al. 2002) with tint as Duv; xy → XYZ → camera space through the camera matrix |
| 4 | Highlight reconstruction | `lib.rs` | A clipped channel is raised to the brightest unclipped channel of the same pixel, so blown areas trend to neutral instead of magenta, and stay above 1.0 for the tone stage |
| 5 | Camera → working space | `pe-colour` | Camera matrix composed with Rec.2020 primaries, rows normalised so camera white → (1,1,1), inverted |
| 6 | Exposure | `lib.rs` | Gain of 2^EV in scene-linear light |
| 6a | Noise reduction, sharpening | `detail.rs` | On square-rooted (variance-stabilised) values in an opponent space (mean; (R−B)/2; (2G−R−B)/4). **Noise reduction:** five-level à-trous B3-spline wavelets (Starck & Murtagh), soft-thresholding each level at a multiple of the noise measured in the frame (MAD of the finest level on a 1-in-16 grid, before exposure, scaled by √gain), so the sliders mean the same on previews and exports. Luminance keeps part of the removed detail (Detail); colour does not. **Sharpening:** unsharp mask on luminance, radius in sensor pixels (scaled by the frame's reduction), detail soft-limited against halos, optional edge mask (Masking) |
| 6b | Dehaze | `local.rs` | Dark channel prior (He, Sun & Tang 2009): airlight from the brightest 0.1% of dark-channel samples (1-in-4 grid); transmission from a patch minimum (~1% radius) refined by a guided filter with luminance as guide (~3% radius); scene recovered as (I − A)/t + A, t ≥ 0.1. Negative values mix towards the airlight |
| 7 | Shadows / Highlights | `tone.rs`, `filter.rs` | log2 luminance split into base + detail with a fast guided filter (He & Sun 2015) at ~2% radius. The base is shifted by up to ±1.5 EV with smooth masks for dark/bright regions and applied as an RGB gain, so local detail is preserved and halos are limited |
| 7b | Clarity / Texture | `local.rs` | One luminance gain with shadows/highlights. **Clarity:** log-luminance detail above a guided-filter base (~0.8% radius), weighted to midtones. **Texture:** detail above a ~0.2% box blur, soft-limited so edges do not halo. All radii are fractions of the short side, so previews and exports match |
| 8 | Contrast | `tone.rs` | Power curve on luminance pivoting at scene mid-grey (0.18), applied as RGB gain |
| 9 | Display transform | `tone.rs` | Narkowicz (2015) ACES-fitted curve on the max(R,G,B) norm, with RGB ratios preserved (hue-stable), normalised so a scene white point (default 16, moved by **Whites**) maps to 1.0. Colours near white desaturate to roll off. **Blacks** offsets the black level |
| 9b | Colour | `look.rs` | In OKLab/OKLCh (Ottosson 2020) on display-linear values. **Colour mixer:** eight hue bands with smooth weights; hue ±30°, chroma gain, lightness ±0.6 stop, fading out towards neutrals. **Saturation:** uniform chroma gain. **Vibrance:** chroma gain weighted to muted colours, halved on skin hues. **Black & white:** OKLab lightness, ±0.8 stop per hue band. **Colour grading:** shadow/midtone/highlight weights from lightness (balance moves the split, blending widens the transitions), each adding an a/b tint and a lightness shift |
| 10 | Output | `lib.rs`, `tone.rs` | Rec.2020 → linear sRGB; out-of-gamut colours desaturated towards their luminance; sRGB transfer function |
| 11 | Point curve | `tone.rs` | Monotone cubic (Fritsch & Carlson 1980) through user points on encoded 0–255 values |
| 12 | Orientation | `orient.rs` | EXIF orientation codes 1–8 (rotate/flip) |
| 12b | Geometry | `geometry.rs` | Each output pixel is mapped back through one inverse transform: crop (fractions of the corrected frame) → straighten (rotation about the centre) → perspective (homography, Vertical/Horizontal tilt, Scale) → orientation → radial lens distortion (k₁ r², r relative to the half-diagonal) → per-channel radial scale for lateral chromatic aberration; sampled bilinearly, black outside the frame. Done after all per-pixel processing on both CPU and GPU. Lens vignetting is a radial gain (stops at the corners) in the scene stage |
| 13 | Effects | `effects.rs` | In output coordinates normalised to the frame. **Vignette:** distance with roundness (ellipse fitted to the frame, towards a circle, or a p-norm rounded rectangle), smoothstep between Midpoint and Feather; darkens (sparing highlights if asked) or lightens towards white. **Grain:** two octaves of hashed value noise (cell size a fraction of the short side, mixed by Roughness), applied as a midtone-weighted gain |

## Settings (`pe settings` prints the defaults)

| Key | Range | Default |
|---|---|---|
| `whiteBalance` | `{"mode":"asShot"}`, `{"mode":"auto"}`, `{"mode":"custom","temperature":K,"tint":t}` (1667–25000 K, tint −150…150, + = magenta) | as shot |
| `exposure` | −5…5 EV | 0 |
| `contrast`, `highlights`, `shadows`, `whites`, `blacks` | −100…100 | 0 |
| `toneCurve` | list of `[x, y]` on 0…255 | identity |
| `texture`, `clarity`, `dehaze` | −100…100 | 0 |
| `sharpening` | `amount` 0…150, `radius` 0.5…3 (sensor px), `detail` 0…100, `masking` 0…100 | 0, 1.0, 25, 0 |
| `vignette` | `amount` −100…100, `midpoint` 0…100, `roundness` −100…100, `feather` 0…100, `highlights` 0…100 | 0, 50, 0, 50, 0 |
| `grain` | `amount`, `size`, `roughness`: 0…100 | 0, 25, 50 |
| `crop` | `left`, `top`, `right`, `bottom` (fractions 0…1), `angle` −45…45° | full frame, 0 |
| `transform` | `vertical`, `horizontal` −100…100, `scale` 50…150 | 0, 0, 100 |
| `lens` | `distortion`, `vignetting`, `caRedCyan`, `caBlueYellow`: −100…100 | 0 |
| `noiseReduction` | `luminance`, `detail`, `color`: 0…100 | 0, 50, 0 |
| `vibrance`, `saturation` | −100…100 | 0 |
| `hsl` | `{"hue":[8], "saturation":[8], "luminance":[8]}`, each −100…100, bands red, orange, yellow, green, aqua, blue, purple, magenta | 0 |
| `treatment` | `color` / `monochrome` | color |
| `bwMix` | 8 values, −100…100 | 0 |
| `grading` | `shadows`/`midtones`/`highlights`/`global`: `{"hue":0…360, "saturation":0…100, "luminance":−100…100}`; `blending` 0…100; `balance` −100…100 | blending 50 |
| `demosaic` | `auto` / `fast` | auto |
| `applyOrientation` | bool | true |

## GPU implementation
`crates/gpu` runs stages 3–12 as WGSL compute shaders through wgpu (Metal on macOS) on a linear-RGB frame (the reduced
preview). Each shader entry point mirrors one CPU function, including the guided filter's clamped box means and bilinear
upsampling. `tests/parity.rs` renders four cameras under six sets of settings both ways and requires CIEDE2000 mean < 0.1
and 99.9th percentile < 0.5, with no channel more than one 8-bit step apart. In practice every percentile is 0.000;
the few differing pixels come from float rounding. Without a GPU (or with `PE_NO_GPU` set) everything falls back to the CPU.

## Known limitations (planned)
- A single colour matrix per camera (the daylight-like one). Dual-illuminant interpolation is still to do.
- No baseline-exposure offset per camera, so some cameras render darker than others at exposure 0.
- Demosaic is a solid classical method. RCD (Bayer) and Markesteijn (X-Trans) are candidates for higher detail.
- Highlight reconstruction is per-pixel. Spatial (inpainting-style) reconstruction would recover more colour in large blown areas.
- Demosaic runs on the CPU only, so a full-resolution render takes about 1–2.5 s for 24–36 MP. Live editing uses a reduced
  preview, which the GPU renders in milliseconds.
