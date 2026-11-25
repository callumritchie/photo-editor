# Render pipeline

The CPU reference pipeline lives in `crates/pipeline`. A GPU version (milestone M4) will be verified against it.
All processing is 32-bit float. Values stay scene-linear and unclipped until the display transform.

| # | Stage | Module | Method |
|---|---|---|---|
| 1 | Decode | `pe-raw` | `rawler` decodes the file; we crop to the camera's recommended area, subtract per-position black levels and scale so saturation = 1.0 |
| 2 | Demosaic | `demosaic.rs` | **Bayer:** edge-directed green with Laplacian correction (Hamilton & Adams, US 5,629,734, expired), then red/blue as colour differences. **Other CFAs (X-Trans):** green by neighbourhood average, then colour differences. **Both:** 3×3 median on R−G and B−G to suppress false colour (Freeman 1988). `--fast` uses plain neighbourhood averaging |
| 3 | White balance | `wb.rs` | Camera multipliers from: as-shot; auto (grey-world over well-exposed pixels); or temperature/tint. Temperature/tint → CIE xy via the Planckian locus (Kim et al. 2002) with tint as Duv; xy → XYZ → camera space through the camera matrix |
| 4 | Highlight reconstruction | `lib.rs` | A clipped channel is raised to the brightest unclipped channel of the same pixel, so blown areas trend to neutral instead of magenta, and stay above 1.0 for the tone stage |
| 5 | Camera → working space | `pe-colour` | Camera matrix composed with Rec.2020 primaries, rows normalised so camera white → (1,1,1), inverted |
| 6 | Exposure | `lib.rs` | Gain of 2^EV in scene-linear light |
| 7 | Shadows / Highlights | `tone.rs`, `filter.rs` | log2 luminance split into base + detail with a fast guided filter (He & Sun 2015) at ~2% radius. The base is shifted by up to ±1.5 EV with smooth masks for dark/bright regions and applied as an RGB gain, so local detail is preserved and halos are limited |
| 8 | Contrast | `tone.rs` | Power curve on luminance pivoting at scene mid-grey (0.18), applied as RGB gain |
| 9 | Display transform | `tone.rs` | Narkowicz (2015) ACES-fitted curve on the max(R,G,B) norm, with RGB ratios preserved (hue-stable), normalised so a scene white point (default 16, moved by **Whites**) maps to 1.0. Colours near white desaturate to roll off. **Blacks** offsets the black level |
| 10 | Output | `lib.rs`, `tone.rs` | Rec.2020 → linear sRGB; out-of-gamut colours desaturated towards their luminance; sRGB transfer function |
| 11 | Point curve | `tone.rs` | Monotone cubic (Fritsch & Carlson 1980) through user points on encoded 0–255 values |
| 12 | Orientation | `orient.rs` | EXIF orientation codes 1–8 (rotate/flip) |

## Settings (`pe settings` prints the defaults)

| Key | Range | Default |
|---|---|---|
| `whiteBalance` | `{"mode":"asShot"}`, `{"mode":"auto"}`, `{"mode":"custom","temperature":K,"tint":t}` (1667–25000 K, tint −150…150, + = magenta) | as shot |
| `exposure` | −5…5 EV | 0 |
| `contrast`, `highlights`, `shadows`, `whites`, `blacks` | −100…100 | 0 |
| `toneCurve` | list of `[x, y]` on 0…255 | identity |
| `demosaic` | `auto` / `fast` | auto |
| `applyOrientation` | bool | true |

## Known limitations (planned)
- A single colour matrix per camera (the daylight-like one). Dual-illuminant interpolation is still to do.
- No baseline-exposure offset per camera, so some cameras render darker than others at exposure 0.
- Demosaic is a solid classical method. RCD (Bayer) and Markesteijn (X-Trans) are candidates for higher detail.
- Highlight reconstruction is per-pixel. Spatial (inpainting-style) reconstruction would recover more colour in large blown areas.
- CPU only, so a full render takes about 1–2 s for 24–36 MP. Interactive speed comes with the GPU pipeline (M4).
