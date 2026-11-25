//! File writers: JPEG, PNG and TIFF, each with an embedded ICC profile and optional EXIF.

use std::io::Cursor;

use image::{DynamicImage, ImageBuffer, Rgb};

use crate::ExportError;

/// Pixel data ready to write: encoded values in [0, 1], interleaved RGB.
pub struct Encoded<'a> {
    pub width: u32,
    pub height: u32,
    pub data: &'a [f32],
}

fn to_u8(e: &Encoded) -> Vec<u8> {
    e.data
        .iter()
        .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect()
}

fn to_u16(e: &Encoded) -> Vec<u16> {
    e.data
        .iter()
        .map(|v| (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16)
        .collect()
}

/// Insert APP1 (EXIF) and APP2 (ICC) segments right after the JPEG SOI marker.
fn jpeg_insert_segments(jpeg: Vec<u8>, icc: &[u8], exif: Option<&[u8]>) -> Vec<u8> {
    let mut segs = Vec::new();
    if let Some(ex) = exif {
        let len = 2 + 6 + ex.len();
        if len <= 0xFFFF {
            segs.extend([0xFF, 0xE1]);
            segs.extend((len as u16).to_be_bytes());
            segs.extend(b"Exif\0\0");
            segs.extend(ex);
        }
    }
    // ICC may span several APP2 chunks of at most 65519 bytes of profile data each.
    let chunks: Vec<&[u8]> = icc.chunks(65_519).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let len = 2 + 12 + 2 + chunk.len();
        segs.extend([0xFF, 0xE2]);
        segs.extend((len as u16).to_be_bytes());
        segs.extend(b"ICC_PROFILE\0");
        segs.push(i as u8 + 1);
        segs.push(chunks.len() as u8);
        segs.extend(*chunk);
    }
    let mut out = Vec::with_capacity(jpeg.len() + segs.len());
    out.extend(&jpeg[..2]); // SOI
    out.extend(segs);
    out.extend(&jpeg[2..]);
    out
}

pub fn jpeg(
    e: &Encoded,
    quality: u8,
    icc: &[u8],
    exif: Option<&[u8]>,
) -> Result<Vec<u8>, ExportError> {
    let img: ImageBuffer<Rgb<u8>, _> = ImageBuffer::from_raw(e.width, e.height, to_u8(e))
        .ok_or(ExportError::Internal("buffer size"))?;
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
        .encode_image(&img)?;
    Ok(jpeg_insert_segments(out, icc, exif))
}

/// CRC-32 (ISO 3309 / PNG) of `bytes`.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn png_chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut c = Vec::with_capacity(body.len() + 12);
    c.extend((body.len() as u32).to_be_bytes());
    c.extend(kind);
    c.extend(body);
    let mut crc_input = kind.to_vec();
    crc_input.extend(body);
    c.extend(crc32(&crc_input).to_be_bytes());
    c
}

pub fn png(
    e: &Encoded,
    sixteen_bit: bool,
    icc: &[u8],
    exif: Option<&[u8]>,
) -> Result<Vec<u8>, ExportError> {
    let img = if sixteen_bit {
        DynamicImage::ImageRgb16(
            ImageBuffer::from_raw(e.width, e.height, to_u16(e))
                .ok_or(ExportError::Internal("buffer size"))?,
        )
    } else {
        DynamicImage::ImageRgb8(
            ImageBuffer::from_raw(e.width, e.height, to_u8(e))
                .ok_or(ExportError::Internal("buffer size"))?,
        )
    };
    let mut raw = Vec::new();
    img.write_to(&mut Cursor::new(&mut raw), image::ImageFormat::Png)?;
    // Insert iCCP (and eXIf) after IHDR: signature (8) + IHDR chunk (25).
    let mut iccp = b"ICC profile\0\0".to_vec(); // name, NUL, compression method 0 (zlib)
    iccp.extend(miniz_oxide::deflate::compress_to_vec_zlib(icc, 9));
    let mut extra = png_chunk(b"iCCP", &iccp);
    if let Some(ex) = exif {
        extra.extend(png_chunk(b"eXIf", ex));
    }
    let mut out = Vec::with_capacity(raw.len() + extra.len());
    out.extend(&raw[..33]);
    out.extend(extra);
    out.extend(&raw[33..]);
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TiffCompression {
    None,
    #[default]
    Lzw,
    Deflate,
}

pub struct TiffTags<'a> {
    pub make: Option<&'a str>,
    pub model: Option<&'a str>,
    pub date_time: Option<String>,
    pub artist: Option<&'a str>,
    pub copyright: Option<&'a str>,
    pub ppi: u32,
}

pub fn tiff(
    e: &Encoded,
    sixteen_bit: bool,
    compression: TiffCompression,
    icc: &[u8],
    tags: &TiffTags,
) -> Result<Vec<u8>, ExportError> {
    use tiff::encoder::{colortype, Compression, TiffEncoder};
    use tiff::tags::{ResolutionUnit, Tag};
    let comp = match compression {
        TiffCompression::None => Compression::Uncompressed,
        TiffCompression::Lzw => Compression::Lzw,
        TiffCompression::Deflate => Compression::Deflate(tiff::encoder::DeflateLevel::Balanced),
    };
    let mut buf = Cursor::new(Vec::new());
    let mut enc = TiffEncoder::new(&mut buf)?.with_compression(comp);
    macro_rules! write {
        ($ct:ty, $data:expr) => {{
            let mut img = enc.new_image::<$ct>(e.width, e.height)?;
            img.resolution(
                ResolutionUnit::Inch,
                tiff::encoder::Rational { n: tags.ppi, d: 1 },
            );
            let d = img.encoder();
            d.write_tag(Tag::Unknown(34675), icc)?;
            d.write_tag(
                Tag::Software,
                concat!("photo-editor ", env!("CARGO_PKG_VERSION")),
            )?;
            if let Some(v) = tags.make {
                d.write_tag(Tag::Make, v)?;
            }
            if let Some(v) = tags.model {
                d.write_tag(Tag::Model, v)?;
            }
            if let Some(v) = &tags.date_time {
                d.write_tag(Tag::DateTime, v.as_str())?;
            }
            if let Some(v) = tags.artist {
                d.write_tag(Tag::Artist, v)?;
            }
            if let Some(v) = tags.copyright {
                d.write_tag(Tag::Copyright, v)?;
            }
            img.write_data($data)?;
        }};
    }
    if sixteen_bit {
        write!(colortype::RGB16, &to_u16(e));
    } else {
        write!(colortype::RGB8, &to_u8(e));
    }
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn writers_produce_decodable_files() {
        let data: Vec<f32> = (0..8 * 6 * 3).map(|i| (i % 7) as f32 / 7.0).collect();
        let e = Encoded {
            width: 8,
            height: 6,
            data: &data,
        };
        let icc = pe_colour::icc::profile(pe_colour::ColorSpace::DisplayP3);
        for bytes in [
            jpeg(&e, 90, &icc, Some(b"II*\0\x08\0\0\0\0\0\0\0\0\0")).unwrap(),
            png(&e, false, &icc, None).unwrap(),
            png(&e, true, &icc, None).unwrap(),
        ] {
            let img = image::load_from_memory(&bytes).unwrap();
            assert_eq!((img.width(), img.height()), (8, 6));
        }
        let t = tiff(
            &e,
            true,
            TiffCompression::Lzw,
            &icc,
            &TiffTags {
                make: Some("X"),
                model: None,
                date_time: None,
                artist: None,
                copyright: None,
                ppi: 300,
            },
        )
        .unwrap();
        let mut dec = tiff::decoder::Decoder::new(Cursor::new(t)).unwrap();
        assert_eq!(dec.dimensions().unwrap(), (8, 6));
        let icc_back = dec.get_tag_u8_vec(tiff::tags::Tag::Unknown(34675)).unwrap();
        assert_eq!(icc_back, icc);
    }
}
