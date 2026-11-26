//! XMP metadata (ISO 16684-1) for exported files: keywords as Dublin Core `dc:subject`, artist as
//! `dc:creator`, copyright as `dc:rights`. Embedding follows the XMP specification, part 3: a JPEG
//! APP1 segment with the XMP namespace header, a PNG `iTXt` chunk named `XML:com.adobe.xmp`, and TIFF
//! tag 700 (XMLPacket).

use crate::exif::MetadataMode;

/// JPEG APP1 identifier for XMP (XMP specification part 3).
pub const JPEG_XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// PNG iTXt keyword for XMP.
pub const PNG_XMP_KEYWORD: &str = "XML:com.adobe.xmp";

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' => {}
            c => o.push(c),
        }
    }
    o
}

/// The XMP packet, or `None` when there is nothing to write (or metadata is off).
pub fn build(
    keywords: &[String],
    mode: MetadataMode,
    artist: Option<&str>,
    copyright: Option<&str>,
) -> Option<String> {
    if mode == MetadataMode::None {
        return None;
    }
    let keywords: &[String] = if mode == MetadataMode::All {
        keywords
    } else {
        &[]
    };
    let artist = artist.filter(|s| !s.is_empty());
    let copyright = copyright.filter(|s| !s.is_empty());
    if keywords.is_empty() && artist.is_none() && copyright.is_none() {
        return None;
    }
    let mut body = String::new();
    if !keywords.is_empty() {
        body.push_str("   <dc:subject>\n    <rdf:Bag>\n");
        for k in keywords {
            body.push_str(&format!("     <rdf:li>{}</rdf:li>\n", esc(k)));
        }
        body.push_str("    </rdf:Bag>\n   </dc:subject>\n");
    }
    if let Some(a) = artist {
        body.push_str(&format!(
            "   <dc:creator>\n    <rdf:Seq>\n     <rdf:li>{}</rdf:li>\n    </rdf:Seq>\n   </dc:creator>\n",
            esc(a)
        ));
    }
    if let Some(c) = copyright {
        body.push_str(&format!(
            "   <dc:rights>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    </rdf:Alt>\n   </dc:rights>\n",
            esc(c)
        ));
    }
    Some(format!(
        "<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
  <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n\
{body}\
  </rdf:Description>\n\
 </rdf:RDF>\n\
</x:xmpmeta>\n\
<?xpacket end=\"w\"?>"
    ))
}

/// Keywords for export from keyword paths ("Animals > Birds"): every level, without repeats.
pub fn expand_paths(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        for part in p.split('>').map(str::trim).filter(|s| !s.is_empty()) {
            if !out.iter().any(|k| k.eq_ignore_ascii_case(part)) {
                out.push(part.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_contents_and_escaping() {
        let x = build(
            &["Birds".into(), "R&D <test>".into()],
            MetadataMode::All,
            Some("Ann"),
            Some("(c) Ann"),
        )
        .unwrap();
        assert!(x.contains("<rdf:li>Birds</rdf:li>"));
        assert!(x.contains("<rdf:li>R&amp;D &lt;test&gt;</rdf:li>"));
        assert!(x.contains("<dc:creator>") && x.contains("(c) Ann"));
        let c = build(
            &["Birds".into()],
            MetadataMode::CopyrightOnly,
            None,
            Some("(c) Ann"),
        )
        .unwrap();
        assert!(
            !c.contains("Birds") && c.contains("(c) Ann"),
            "copyright only: no keywords"
        );
        assert!(build(&["Birds".into()], MetadataMode::None, None, None).is_none());
        assert!(build(&[], MetadataMode::All, None, None).is_none());
        // Well-formed XML (checked with the system's XML parser where available).
        if std::process::Command::new("xmllint")
            .arg("--version")
            .output()
            .is_ok()
        {
            let dir = tempfile::tempdir().unwrap();
            let f = dir.path().join("x.xml");
            std::fs::write(&f, &x).unwrap();
            let ok = std::process::Command::new("xmllint")
                .arg("--noout")
                .arg(&f)
                .status()
                .unwrap();
            assert!(ok.success(), "{x}");
        }
    }

    #[test]
    fn keyword_paths_expand_to_every_level() {
        let k = expand_paths(&[
            "Animals > Birds > Owl".into(),
            "Animals > Cats".into(),
            "night".into(),
            "Night".into(),
        ]);
        assert_eq!(k, ["Animals", "Birds", "Owl", "Cats", "night"]);
    }
}
