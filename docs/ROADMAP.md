# Roadmap

Each milestone ends with something runnable and covered by automated tests.

| # | Milestone | Done when |
|---|---|---|
| **M0** ✅ | **Foundations:** workspace, CI, CC0 sample set, raw decode, CPU render to TIFF (`pe render`) | Every sample renders with plausible colour |
| **M1** ✅ | **Library and import:** SQLite catalog (folders, files, images, ratings/flags/labels), Add/Copy import with date-based folders, duplicate detection, metadata harvesting, embedded-preview thumbnails; SwiftUI grid and loupe | Verified: 5,000-file import (about 28 files/s with thumbnails); `kill -9` mid-import then resume, giving exactly 5,000 images with integrity OK. Follow-ups: background thumbnail building, faster reduced-scale JPEG decode, a measured scrolling benchmark |
| **M2** ✅ | **Raw to a pleasing image (CPU):** edge-directed demosaic + false-colour suppression (Bayer and X-Trans), highlight reconstruction, white balance (as-shot/auto/temperature-tint), wide-gamut working space, exposure, local shadows/highlights, contrast, filmic display transform with whites/blacks, point curve, orientation, golden-image regression tests | Default renders look natural across the sample set (see `docs/PIPELINE.md`; follow-ups listed there) |
| M3 | **Non-destructive editing model:** typed settings schema, settings digest, command system, undo/redo, per-photo history (one step per gesture), snapshots, presets, copy/paste/sync | Each slider drag creates one history step; partial presets work |
| M4 | **GPU pipeline and preview cache:** wgpu/Metal versions of each stage checked against the CPU reference; fast interactive previews; preview pyramid keyed by settings digest; background jobs | Under 50 ms slider feedback on a 45 MP raw; CPU/GPU difference ΔE₀₀ < 0.5 |
| M5 | **Export:** JPEG/TIFF/PNG, colour spaces (sRGB, Display P3, Adobe RGB, ProPhoto, custom ICC), resizing, output sharpening, metadata options, renaming, presets | Exports match the on-screen render |
| M6 | **Develop controls:** HSL/colour mixer, colour grading, black & white, texture/clarity/dehaze, noise reduction, sharpening, vignette, grain, lens corrections, crop/straighten/perspective | All controls work on CPU and GPU with parity tests |
| M7 | **Library depth:** collections and sets, rule-based smart collections, hierarchical keywords, filters, virtual copies, stacks | Smart collections update live |
| M8 | **Masks:** linear, radial, brush and colour/luminance range masks with add/subtract/intersect/invert; local versions of the basic controls; subject/sky masks via Apple's on-device frameworks | Mask composition matches expected rasters |
| M9 | **Migration (optional):** import an existing photo catalog and XMP develop settings (see `INTEROP.md`) | Folder/collection/keyword/rating structure imports exactly; looks are approximated |

## Algorithm sources (open literature)
Demosaic: RCD, AMaZE, Markesteijn. Local tone mapping: local Laplacian filters (Paris, Hasinoff, Kautz 2011). Texture/clarity: guided filter
(He, Sun, Tang 2010). Dehaze: dark channel prior (He, Sun, Tang 2009). Noise reduction: à-trous wavelets, non-local means. Colour: CIE
colorimetry, Oklab (Ottosson 2020), DNG Specification colour model. Grain: Newson et al. 2017. Colour management: LittleCMS.
