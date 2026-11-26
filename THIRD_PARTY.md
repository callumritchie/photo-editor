# Third-party components

| Component | Use | Licence |
|---|---|---|
| [rawler](https://github.com/dnglab/dnglab) | Raw file decoding | LGPL-2.1 (compatible with this project's GPL-3.0-or-later) |
| [tiff](https://github.com/image-rs/image-tiff) | TIFF writing | MIT |
| [wgpu](https://github.com/gfx-rs/wgpu) | GPU compute (Metal on macOS) | MIT / Apache-2.0 |
| [pollster](https://github.com/zesterer/pollster), [bytemuck](https://github.com/Lokathor/bytemuck) | Blocking on GPU setup; GPU buffer layout | MIT / Apache-2.0 (bytemuck also Zlib) |
| [rayon](https://github.com/rayon-rs/rayon) | Parallelism | MIT / Apache-2.0 |
| [clap](https://github.com/clap-rs/clap) | CLI parsing | MIT / Apache-2.0 |
| [thiserror](https://github.com/dtolnay/thiserror), [anyhow](https://github.com/dtolnay/anyhow) | Error handling | MIT / Apache-2.0 |
| [rusqlite](https://github.com/rusqlite/rusqlite) with bundled [SQLite](https://sqlite.org) | Catalog database | MIT; SQLite is public domain |
| [image](https://github.com/image-rs/image) | JPEG/PNG decode and encode, Lanczos resizing | MIT / Apache-2.0 |
| [miniz_oxide](https://github.com/Frommi/miniz_oxide) | zlib compression of embedded PNG profiles | MIT / Zlib / Apache-2.0 |
| [kamadak-exif](https://github.com/kamadak/exif-rs) | Tests only: independent EXIF reader to check our writer | BSD-2-Clause |
| [uuid](https://github.com/uuid-rs/uuid) | Record identifiers | MIT / Apache-2.0 |
| [tempfile](https://github.com/Stebalien/tempfile) | Tests | MIT / Apache-2.0 |
| [serde](https://github.com/serde-rs/serde), [serde_json](https://github.com/serde-rs/json) | Settings files, fixtures manifest | MIT / Apache-2.0 |
| [quick-xml](https://github.com/tafia/quick-xml) | Reading the lens database | MIT |

## Data

| Data | Use | Licence |
|---|---|---|
| [Lensfun](https://github.com/lensfun/lensfun) lens database, in `data/lensfun/` (unmodified) | Lens profile corrections (distortion, chromatic aberration, vignetting) | [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/), © the Lensfun contributors; see `data/lensfun/README.md` |

The lens database is data read at run time and shipped beside the program (in the app bundle's
`Resources/lensfun`), under its own licence; it is not part of the GPL-licensed source code. This
project uses none of Lensfun's code: `crates/lens` is our own reader, written from the database's
documented format.

Sample raw files (downloaded on demand, not distributed with this repository) come from [raw.pixls.us](https://raw.pixls.us/). Only
files marked CC0 1.0 (public domain dedication) are used. See `fixtures/samples.json` for the exact files and checksums.

Golden images in `fixtures/golden/` are small renders derived from those CC0 samples.
