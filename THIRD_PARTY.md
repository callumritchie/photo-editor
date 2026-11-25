# Third-party components

| Component | Use | Licence |
|---|---|---|
| [rawler](https://github.com/dnglab/dnglab) | Raw file decoding | LGPL-2.1 (compatible with this project's GPL-3.0-or-later) |
| [tiff](https://github.com/image-rs/image-tiff) | TIFF writing | MIT |
| [rayon](https://github.com/rayon-rs/rayon) | Parallelism | MIT / Apache-2.0 |
| [clap](https://github.com/clap-rs/clap) | CLI parsing | MIT / Apache-2.0 |
| [thiserror](https://github.com/dtolnay/thiserror), [anyhow](https://github.com/dtolnay/anyhow) | Error handling | MIT / Apache-2.0 |
| [rusqlite](https://github.com/rusqlite/rusqlite) with bundled [SQLite](https://sqlite.org) | Catalog database | MIT; SQLite is public domain |
| [image](https://github.com/image-rs/image) | JPEG decode/encode, resizing | MIT / Apache-2.0 |
| [uuid](https://github.com/uuid-rs/uuid) | Record identifiers | MIT / Apache-2.0 |
| [tempfile](https://github.com/Stebalien/tempfile) | Tests | MIT / Apache-2.0 |
| [serde](https://github.com/serde-rs/serde), [serde_json](https://github.com/serde-rs/json) | Settings files, fixtures manifest | MIT / Apache-2.0 |

Sample raw files (downloaded on demand, not distributed with this repository) come from [raw.pixls.us](https://raw.pixls.us/). Only
files marked CC0 1.0 (public domain dedication) are used. See `fixtures/samples.json` for the exact files and checksums.

Golden images in `fixtures/golden/` are small renders derived from those CC0 samples.
