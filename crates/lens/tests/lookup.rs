//! Lookups against the shipped database (data/lensfun).
use pe_lens::{lookup, Shot};

fn shot(make: &str, model: &str, lens: &str, focal: f32, aperture: f32) -> Shot {
    Shot {
        camera_make: make.into(),
        camera_model: model.into(),
        lens: lens.into(),
        focal_length: focal,
        aperture,
    }
}

#[test]
fn finds_lenses_by_their_exif_names() {
    let cases = [
        (
            shot(
                "Canon",
                "EOS 5D Mark III",
                "Canon EF 70-200mm f/2.8L IS II USM",
                70.0,
                2.8,
            ),
            "Canon EF 70-200mm f/2.8L IS II USM",
        ),
        (
            shot(
                "Canon",
                "EOS R6",
                "Canon RF 70-200mm F2.8L IS USM",
                200.0,
                25.0,
            ),
            "Canon RF 70-200mm F2.8L IS USM",
        ),
        (
            shot("Fujifilm", "X-T3", "XF35mmF2 R WR", 35.0, 3.6),
            "Fujifilm XF35mmF2 R WR",
        ),
        (
            shot(
                "Nikon",
                "D750",
                "Tamron SP 35mm f/1.8 Di VC USD (F012)",
                35.0,
                1.8,
            ),
            "Tamron SP 35mm f/1.8 Di VC USD",
        ),
        (
            shot(
                "Olympus",
                "E-M1MarkII",
                "Olympus M.Zuiko Digital ED 12-40mm F2.8 Pro",
                28.0,
                2.8,
            ),
            "Olympus M.Zuiko Digital ED 12-40mm f/2.8 Pro",
        ),
        (
            shot("Panasonic", "DC-G9", "Lumix G 25mm F1.7 Asph.", 25.0, 1.7),
            "Panasonic Lumix G 25mm f/1.7 Asph.",
        ),
        (
            shot(
                "Pentax",
                "K-1",
                "Pentax smc PENTAX-DA 35mm F2.4 AL",
                35.0,
                14.0,
            ),
            "Pentax smc Pentax-DA 35mm f/2.4 AL",
        ),
    ];
    for (s, want) in cases {
        let m = lookup(&s).unwrap_or_else(|| panic!("no match for {}", s.lens));
        assert!(
            m.name.to_lowercase().starts_with(&want.to_lowercase()),
            "{} -> {} (want {want})",
            s.lens,
            m.name
        );
    }
    let m = lookup(&shot(
        "Sony",
        "ILCE-7M3",
        "Sony FE 16-35mm F2.8 GM",
        32.0,
        4.0,
    ))
    .unwrap();
    assert!(m.name.ends_with("FE 16-35mm f/2.8 GM"), "{}", m.name);
    // Versions must agree: II is not III.
    let m = lookup(&shot(
        "Canon",
        "EOS 5D Mark III",
        "Canon EF 16-35mm f/2.8L III USM",
        20.0,
        8.0,
    ))
    .unwrap();
    assert!(m.name.contains("III"), "{}", m.name);
    let m = lookup(&shot(
        "Canon",
        "EOS 5D Mark III",
        "Canon EF 16-35mm f/2.8L II USM",
        20.0,
        8.0,
    ))
    .unwrap();
    assert!(
        m.name.contains("L II ") && !m.name.contains("III"),
        "{}",
        m.name
    );
    // Not in the database, and no lens at all.
    assert!(lookup(&shot(
        "Sony",
        "ILCE-7M3",
        "Sony FE 17-41mm F3.3 XYZ",
        32.0,
        4.0
    ))
    .is_none());
    assert!(lookup(&shot("Pentax", "K-1", "", 35.0, 14.0)).is_none());
    // A different focal length or aperture in the name is not a match.
    assert!(
        lookup(&shot("Fujifilm", "X-T3", "XF23mmF2 R WR", 23.0, 2.0))
            .is_none_or(|m| !m.name.contains("35"))
    );
}

#[test]
fn interpolates_and_normalises() {
    // XF35mmF2 R WR: ptlens a=0.00956 b=-0.07057 c=0.07334; pa at f/2 k1=-1.3696769 ...; crop 1.528.
    let m = lookup(&shot("Fujifilm", "X-T3", "XF35mmF2 R WR", 35.0, 2.0)).unwrap();
    let p = m.profile;
    assert!(p.has_distortion && p.has_tca && p.has_vignetting);
    assert!(
        (p.distortion_factor(1.0) - 1.0).abs() < 1e-6,
        "ptlens factor is 1 at r = 1"
    );
    assert!((p.vignetting[0] + 1.369_677).abs() < 1e-5);
    // Same crop factor as the calibration: distortion unit is half the short side of a 3:2 frame.
    assert!(
        (p.dist_scale - (1.0f32 + 1.5 * 1.5).sqrt()).abs() < 1e-2,
        "{}",
        p.dist_scale
    );
    assert!((p.vig_scale - 1.0).abs() < 1e-2);
    // Between f/2 (k1 -1.37) and f/2.8 (k1 -0.217): their geometric mean interpolates halfway.
    let h = lookup(&shot(
        "Fujifilm",
        "X-T3",
        "XF35mmF2 R WR",
        35.0,
        (2.0f32 * 2.8).sqrt(),
    ))
    .unwrap();
    let mid = (-1.369_677 + -0.216_64) / 2.0;
    assert!(
        (h.profile.vignetting[0] - mid).abs() < 1e-3,
        "{}",
        h.profile.vignetting[0]
    );
    // Zoom: interpolated between calibrated focal lengths, so it differs from both ends.
    let z = |f| {
        lookup(&shot(
            "Canon",
            "EOS 5D Mark III",
            "Canon EF 70-200mm f/2.8L IS II USM",
            f,
            2.8,
        ))
        .unwrap()
        .profile
        .distortion
    };
    let (a, b, c) = (z(70.0), z(200.0), z(120.0));
    assert!(a != b && c != a && c != b);
}
