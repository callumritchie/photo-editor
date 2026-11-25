//! Minimal EXIF writer (TIFF-structured, little-endian), from the public EXIF 2.32 / TIFF 6.0 specs.
//!
//! Produces the bytes that follow the "Exif\0\0" header in a JPEG APP1 segment, or the body of a PNG
//! `eXIf` chunk. Only the tags this app knows are written.

/// Descriptive metadata about a photo, as available from the catalog.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhotoMeta {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    /// Local capture time "YYYY-MM-DDTHH:MM:SS".
    pub capture_time: Option<String>,
    /// e.g. "+01:00".
    pub capture_offset: Option<String>,
    pub iso: Option<u32>,
    pub exposure_time: Option<f64>,
    pub f_number: Option<f64>,
    pub focal_length: Option<f64>,
}

/// Which metadata to embed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MetadataMode {
    /// Camera, exposure, lens, capture time, plus artist/copyright.
    #[default]
    All,
    /// Only artist and copyright.
    CopyrightOnly,
    /// No EXIF at all (the ICC profile is still embedded).
    None,
}

enum Value {
    Ascii(String),
    Short(u16),
    Long(u32),
    Rational(u32, u32),
    Undefined(Vec<u8>),
}

impl Value {
    fn type_count(&self) -> (u16, u32) {
        match self {
            Value::Ascii(s) => (2, s.len() as u32 + 1),
            Value::Short(_) => (3, 1),
            Value::Long(_) => (4, 1),
            Value::Rational(..) => (5, 1),
            Value::Undefined(b) => (7, b.len() as u32),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        match self {
            Value::Ascii(s) => {
                let mut b = s.as_bytes().to_vec();
                b.push(0);
                b
            }
            Value::Short(v) => v.to_le_bytes().to_vec(),
            Value::Long(v) => v.to_le_bytes().to_vec(),
            Value::Rational(n, d) => [n.to_le_bytes(), d.to_le_bytes()].concat(),
            Value::Undefined(b) => b.clone(),
        }
    }
}

fn rational(v: f64) -> Value {
    if v >= 1.0 || v <= 0.0 {
        Value::Rational((v * 10_000.0).round().max(0.0) as u32, 10_000)
    } else {
        // Exposure-style values: 1/x when that is exact enough, else fixed point.
        let inv = 1.0 / v;
        if (inv - inv.round()).abs() < 0.01 {
            Value::Rational(1, inv.round() as u32)
        } else {
            Value::Rational((v * 1_000_000.0).round() as u32, 1_000_000)
        }
    }
}

/// Serialise one IFD at `start` (absolute offset). Returns (bytes, offset of the "next IFD" pointer).
fn write_ifd(entries: &mut [(u16, Value)], start: usize) -> (Vec<u8>, usize) {
    entries.sort_by_key(|e| e.0);
    let table_len = 2 + entries.len() * 12 + 4;
    let mut table = Vec::with_capacity(table_len);
    let mut data = Vec::new();
    table.extend((entries.len() as u16).to_le_bytes());
    for (tag, v) in entries.iter() {
        let (ty, count) = v.type_count();
        let bytes = v.bytes();
        table.extend(tag.to_le_bytes());
        table.extend(ty.to_le_bytes());
        table.extend(count.to_le_bytes());
        if bytes.len() <= 4 {
            let mut inline = bytes.clone();
            inline.resize(4, 0);
            table.extend(inline);
        } else {
            let off = start + table_len + data.len();
            table.extend((off as u32).to_le_bytes());
            data.extend(&bytes);
            if data.len() % 2 == 1 {
                data.push(0); // word alignment
            }
        }
    }
    let next_ptr = start + table.len();
    table.extend(0u32.to_le_bytes()); // no next IFD
    table.extend(data);
    (table, next_ptr)
}

/// Build an EXIF block. Returns `None` for [`MetadataMode::None`].
pub fn build(
    meta: &PhotoMeta,
    mode: MetadataMode,
    artist: Option<&str>,
    copyright: Option<&str>,
    ppi: u32,
    srgb: bool,
) -> Option<Vec<u8>> {
    if mode == MetadataMode::None {
        return None;
    }
    let all = mode == MetadataMode::All;
    let mut ifd0: Vec<(u16, Value)> = vec![
        (274, Value::Short(1)), // Orientation: pixels are already upright
        (282, Value::Rational(ppi, 1)),
        (283, Value::Rational(ppi, 1)),
        (296, Value::Short(2)), // inches
        (
            305,
            Value::Ascii(concat!("photo-editor ", env!("CARGO_PKG_VERSION")).into()),
        ),
    ];
    if let Some(a) = artist.filter(|s| !s.is_empty()) {
        ifd0.push((315, Value::Ascii(a.into())));
    }
    if let Some(c) = copyright.filter(|s| !s.is_empty()) {
        ifd0.push((33432, Value::Ascii(c.into())));
    }
    let mut exif: Vec<(u16, Value)> = vec![
        (36864, Value::Undefined(b"0232".to_vec())),
        (40961, Value::Short(if srgb { 1 } else { 0xFFFF })),
    ];
    if all {
        if let Some(v) = &meta.make {
            ifd0.push((271, Value::Ascii(v.clone())));
        }
        if let Some(v) = &meta.model {
            ifd0.push((272, Value::Ascii(v.clone())));
        }
        if let Some(t) = &meta.capture_time {
            let exif_time = t.replacen('-', ":", 2).replace('T', " ");
            ifd0.push((306, Value::Ascii(exif_time.clone())));
            exif.push((36867, Value::Ascii(exif_time)));
            if let Some(o) = &meta.capture_offset {
                exif.push((36881, Value::Ascii(o.clone())));
            }
        }
        if let Some(v) = meta.exposure_time {
            exif.push((33434, rational(v)));
        }
        if let Some(v) = meta.f_number {
            exif.push((33437, rational(v)));
        }
        if let Some(v) = meta.iso {
            exif.push((34855, Value::Short(v.min(65535) as u16)));
        }
        if let Some(v) = meta.focal_length {
            exif.push((37386, rational(v)));
        }
        if let Some(v) = &meta.lens {
            exif.push((42036, Value::Ascii(v.clone())));
        }
    }
    // Layout: header (8) | IFD0 | Exif IFD. IFD0 holds a pointer (34665) to the Exif IFD, whose offset
    // depends on IFD0's size, so size IFD0 first with a placeholder.
    ifd0.push((34665, Value::Long(0)));
    let (probe, _) = write_ifd(&mut ifd0, 8);
    let exif_start = 8 + probe.len();
    if let Some(e) = ifd0.iter_mut().find(|e| e.0 == 34665) {
        e.1 = Value::Long(exif_start as u32);
    }
    let (ifd0_bytes, _) = write_ifd(&mut ifd0, 8);
    let (exif_bytes, _) = write_ifd(&mut exif, exif_start);
    let mut out = b"II*\0".to_vec();
    out.extend(8u32.to_le_bytes());
    out.extend(ifd0_bytes);
    out.extend(exif_bytes);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PhotoMeta {
        PhotoMeta {
            make: Some("Nikon".into()),
            model: Some("D750".into()),
            lens: Some("50mm f/1.8".into()),
            capture_time: Some("2021-06-26T22:17:11".into()),
            capture_offset: Some("+01:00".into()),
            iso: Some(400),
            exposure_time: Some(1.0 / 250.0),
            f_number: Some(2.8),
            focal_length: Some(50.0),
        }
    }

    fn parse(bytes: Vec<u8>) -> exif::Exif {
        exif::Reader::new().read_raw(bytes).expect("valid EXIF")
    }

    fn field(e: &exif::Exif, tag: exif::Tag) -> String {
        e.get_field(tag, exif::In::PRIMARY)
            .map(|f| f.display_value().to_string())
            .unwrap_or_default()
    }

    #[test]
    fn full_metadata_parses_back() {
        let e = parse(
            build(
                &sample(),
                MetadataMode::All,
                Some("Ann"),
                Some("(c) Ann"),
                300,
                true,
            )
            .unwrap(),
        );
        assert_eq!(field(&e, exif::Tag::Make), "\"Nikon\"");
        assert_eq!(field(&e, exif::Tag::Model), "\"D750\"");
        assert_eq!(
            field(&e, exif::Tag::DateTimeOriginal),
            "2021-06-26 22:17:11"
        );
        assert_eq!(field(&e, exif::Tag::ExposureTime), "1/250");
        assert_eq!(field(&e, exif::Tag::FNumber), "2.8");
        assert_eq!(field(&e, exif::Tag::PhotographicSensitivity), "400");
        assert_eq!(field(&e, exif::Tag::FocalLength), "50");
        assert_eq!(field(&e, exif::Tag::LensModel), "\"50mm f/1.8\"");
        assert_eq!(field(&e, exif::Tag::Artist), "\"Ann\"");
        assert_eq!(
            field(&e, exif::Tag::Orientation),
            "row 0 at top and column 0 at left"
        );
    }

    #[test]
    fn copyright_only_omits_camera_details() {
        let e = parse(
            build(
                &sample(),
                MetadataMode::CopyrightOnly,
                None,
                Some("(c) Ann"),
                240,
                false,
            )
            .unwrap(),
        );
        assert!(field(&e, exif::Tag::Make).is_empty());
        assert!(field(&e, exif::Tag::DateTimeOriginal).is_empty());
        assert_eq!(field(&e, exif::Tag::Copyright), "\"(c) Ann\"");
        assert!(build(&sample(), MetadataMode::None, None, None, 240, true).is_none());
    }
}
