# Interoperability facts and their sources

This project reads (and may later write) files produced by other photo applications, such as catalogs, XMP sidecars and presets.
Every compatibility fact that the code relies on must be listed here **with a permitted source**. Nothing may be added that is known
only from decompiling or disassembling another program.

## Permitted sources

| Tag | Meaning |
|---|---|
| `[public-doc]` | Published by the format owner: specifications, SDK documentation, developer guides (e.g. Adobe XMP Specification, DNG Specification, Lightroom Classic SDK Guide). Cite the document and section. |
| `[data]` | Observed by inspecting the user's own data files (catalog databases, sidecars, preview caches, preference files) with ordinary tools (SQLite, hex viewer, text editor). Describe what was inspected; never commit the files themselves. |
| `[literature]` | Published papers, standards (ISO/IEC/CIE/EXIF/IPTC) or openly licensed projects' documentation. Cite the reference. |

## Rules

1. One row per fact the code depends on, added in the same change as the code.
2. Describe facts in our own words. Do not paste third-party code, SQL, schemas, presets or other copyrightable text.
3. If a fact can't be sourced as above, the code must not depend on it. Design our own behaviour instead.

## Facts

| Area | Fact | Source |
|---|---|---|
| — | *(none yet; compatibility work starts in milestone M9)* | — |
