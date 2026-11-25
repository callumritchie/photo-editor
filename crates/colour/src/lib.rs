//! Colour science helpers: 3x3 matrix maths, standard primaries, transfer functions,
//! camera-to-working-space transforms, CIELAB, and correlated colour temperature.

pub mod cct;
pub mod icc;
pub mod space;

pub use space::ColorSpace;

/// Row-major 3x3 matrix.
pub type Mat3 = [[f32; 3]; 3];

/// Linear sRGB (Rec.709 primaries, D65 white) to CIE XYZ (D65). IEC 61966-2-1.
pub const SRGB_TO_XYZ_D65: Mat3 = [
    [0.412_456_4, 0.357_576_1, 0.180_437_5],
    [0.212_672_9, 0.715_152_2, 0.072_175_0],
    [0.019_333_9, 0.119_192, 0.950_304_1],
];

/// Linear Rec.2020 (ITU-R BT.2020 primaries, D65 white) to CIE XYZ (D65).
pub const REC2020_TO_XYZ_D65: Mat3 = [
    [0.636_958, 0.144_617, 0.168_881],
    [0.262_700, 0.677_998, 0.059_302],
    [0.000_000, 0.028_073, 1.060_985],
];

/// CIE XYZ of the D65 white point (Y = 1).
pub const D65_XYZ: [f32; 3] = [0.950_47, 1.0, 1.088_83];

/// Luminance weights of linear Rec.2020 (second row of its RGB->XYZ matrix).
pub const REC2020_LUMA: [f32; 3] = [0.262_700, 0.677_998, 0.059_302];

pub fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

#[inline]
pub fn apply(m: &Mat3, v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Inverse of a 3x3 matrix, or `None` if singular.
pub fn invert(m: &Mat3) -> Option<Mat3> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-12 {
        return None;
    }
    let d = 1.0 / det;
    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * d,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * d,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * d,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * d,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * d,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * d,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * d,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * d,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * d,
        ],
    ])
}

/// Matrix taking white-balanced camera RGB to linear sRGB.
///
/// Method: form camera-from-sRGB = xyz_to_cam * sRGB_to_XYZ, normalise each row to sum to 1
/// (so that sRGB white maps to camera "white", i.e. equal values after white balance), then invert.
/// The result maps a white-balanced neutral (1,1,1) in camera space to (1,1,1) in sRGB.
pub fn camera_to_srgb(xyz_to_cam: &Mat3) -> Option<Mat3> {
    camera_to_space(xyz_to_cam, &SRGB_TO_XYZ_D65)
}

/// Matrix taking white-balanced camera RGB to the linear RGB space defined by `space_to_xyz`
/// (any D65-white space), mapping camera neutral (1,1,1) to (1,1,1). See [`camera_to_srgb`].
pub fn camera_to_space(xyz_to_cam: &Mat3, space_to_xyz: &Mat3) -> Option<Mat3> {
    let mut cam_from_space = mul(xyz_to_cam, space_to_xyz);
    for row in cam_from_space.iter_mut() {
        let s: f32 = row.iter().sum();
        if s.abs() < 1e-9 {
            return None;
        }
        for v in row.iter_mut() {
            *v /= s;
        }
    }
    invert(&cam_from_space)
}

/// Linear-RGB-to-linear-RGB conversion matrix between two D65-white spaces.
pub fn space_to_space(src_to_xyz: &Mat3, dst_to_xyz: &Mat3) -> Option<Mat3> {
    Some(mul(&invert(dst_to_xyz)?, src_to_xyz))
}

/// CIE XYZ to CIELAB relative to the given white (e.g. [`D65_XYZ`]).
pub fn xyz_to_lab(xyz: [f32; 3], white: [f32; 3]) -> [f32; 3] {
    fn f(t: f32) -> f32 {
        const D: f32 = 6.0 / 29.0;
        if t > D * D * D {
            t.cbrt()
        } else {
            t / (3.0 * D * D) + 4.0 / 29.0
        }
    }
    let (fx, fy, fz) = (
        f(xyz[0] / white[0]),
        f(xyz[1] / white[1]),
        f(xyz[2] / white[2]),
    );
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// Encoded sRGB (0..1 per channel) to CIELAB (D65).
pub fn srgb_to_lab(rgb: [f32; 3]) -> [f32; 3] {
    let lin = [
        srgb_decode(rgb[0]),
        srgb_decode(rgb[1]),
        srgb_decode(rgb[2]),
    ];
    xyz_to_lab(apply(&SRGB_TO_XYZ_D65, lin), D65_XYZ)
}

/// CIE 1976 colour difference.
pub fn delta_e76(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// CIEDE2000 colour difference (CIE 142-2001), following Sharma, Wu & Dalal, "The CIEDE2000
/// colour-difference formula: implementation notes, supplementary test data, and mathematical
/// observations" (Color Res. Appl. 30, 2005). kL = kC = kH = 1.
pub fn delta_e2000(lab1: [f32; 3], lab2: [f32; 3]) -> f32 {
    use std::f64::consts::PI;
    let [l1, a1, b1] = lab1.map(f64::from);
    let [l2, a2, b2] = lab2.map(f64::from);
    let c_bar = ((a1 * a1 + b1 * b1).sqrt() + (a2 * a2 + b2 * b2).sqrt()) / 2.0;
    let c7 = c_bar.powi(7);
    let g = 0.5 * (1.0 - (c7 / (c7 + 25f64.powi(7))).sqrt());
    let (a1p, a2p) = ((1.0 + g) * a1, (1.0 + g) * a2);
    let (c1p, c2p) = ((a1p * a1p + b1 * b1).sqrt(), (a2p * a2p + b2 * b2).sqrt());
    let hue = |b: f64, a: f64| {
        if a == 0.0 && b == 0.0 {
            0.0
        } else {
            let h = b.atan2(a).to_degrees();
            if h < 0.0 {
                h + 360.0
            } else {
                h
            }
        }
    };
    let (h1p, h2p) = (hue(b1, a1p), hue(b2, a2p));
    let dl = l2 - l1;
    let dc = c2p - c1p;
    let dh = if c1p * c2p == 0.0 {
        0.0
    } else if (h2p - h1p).abs() <= 180.0 {
        h2p - h1p
    } else if h2p - h1p > 180.0 {
        h2p - h1p - 360.0
    } else {
        h2p - h1p + 360.0
    };
    let dhh = 2.0 * (c1p * c2p).sqrt() * (dh.to_radians() / 2.0).sin();
    let l_bar = (l1 + l2) / 2.0;
    let cp_bar = (c1p + c2p) / 2.0;
    let hp_bar = if c1p * c2p == 0.0 {
        h1p + h2p
    } else if (h1p - h2p).abs() <= 180.0 {
        (h1p + h2p) / 2.0
    } else if h1p + h2p < 360.0 {
        (h1p + h2p + 360.0) / 2.0
    } else {
        (h1p + h2p - 360.0) / 2.0
    };
    let t = 1.0 - 0.17 * (hp_bar - 30.0).to_radians().cos()
        + 0.24 * (2.0 * hp_bar).to_radians().cos()
        + 0.32 * (3.0 * hp_bar + 6.0).to_radians().cos()
        - 0.20 * (4.0 * hp_bar - 63.0).to_radians().cos();
    let d_theta = 30.0 * (-((hp_bar - 275.0) / 25.0).powi(2)).exp();
    let cp7 = cp_bar.powi(7);
    let rc = 2.0 * (cp7 / (cp7 + 25f64.powi(7))).sqrt();
    let sl = 1.0 + 0.015 * (l_bar - 50.0).powi(2) / (20.0 + (l_bar - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * cp_bar;
    let sh = 1.0 + 0.015 * cp_bar * t;
    let rt = -(2.0 * d_theta * PI / 180.0).sin() * rc;
    let (x, y, z) = (dl / sl, dc / sc, dhh / sh);
    (x * x + y * y + z * z + rt * y * z).sqrt() as f32
}

/// sRGB opto-electronic transfer function (linear -> encoded), IEC 61966-2-1.
#[inline]
pub fn srgb_encode(x: f32) -> f32 {
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Inverse sRGB transfer function (encoded -> linear).
#[inline]
pub fn srgb_decode(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn invert_roundtrip() {
        let inv = invert(&SRGB_TO_XYZ_D65).unwrap();
        let id = mul(&SRGB_TO_XYZ_D65, &inv);
        for (i, row) in id.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                assert!(close(*v, if i == j { 1.0 } else { 0.0 }, 1e-5));
            }
        }
    }

    #[test]
    fn srgb_white_is_d65() {
        let w = apply(&SRGB_TO_XYZ_D65, [1.0, 1.0, 1.0]);
        assert!(
            close(w[0], 0.950_47, 1e-4) && close(w[1], 1.0, 1e-4) && close(w[2], 1.088_83, 1e-4)
        );
    }

    #[test]
    fn transfer_roundtrip() {
        for i in 0..=100 {
            let x = i as f32 / 100.0;
            assert!(close(srgb_decode(srgb_encode(x)), x, 1e-5));
        }
    }

    #[test]
    fn rec2020_white_is_d65() {
        let w = apply(&REC2020_TO_XYZ_D65, [1.0, 1.0, 1.0]);
        for (a, b) in w.iter().zip(D65_XYZ) {
            assert!(close(*a, b, 1e-3), "{w:?}");
        }
    }

    #[test]
    fn lab_of_white_and_grey() {
        let white = srgb_to_lab([1.0, 1.0, 1.0]);
        assert!(close(white[0], 100.0, 0.05) && white[1].abs() < 0.05 && white[2].abs() < 0.05);
        let grey = srgb_to_lab([0.466, 0.466, 0.466]); // ~18% linear
        assert!(close(grey[0], 49.5, 0.5), "{grey:?}");
    }

    #[test]
    fn camera_matrix_preserves_neutral() {
        // An arbitrary plausible camera matrix (not from any real camera).
        let xyz_to_cam: Mat3 = [[0.9, -0.3, -0.05], [-0.4, 1.2, 0.2], [-0.05, 0.15, 0.6]];
        let m = camera_to_srgb(&xyz_to_cam).unwrap();
        let out = apply(&m, [1.0, 1.0, 1.0]);
        for v in out {
            assert!(close(v, 1.0, 1e-4), "{out:?}");
        }
    }

    #[test]
    fn delta_e2000_matches_published_test_data() {
        // Pairs 1, 7, 17, 25 and 34 from Sharma, Wu & Dalal (2005), Table 1.
        let cases = [
            ([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485], 2.0425),
            ([50.0, 0.0, 0.0], [50.0, -1.0, 2.0], 2.3669),
            ([50.0, 2.5, 0.0], [73.0, 25.0, -18.0], 27.1492),
            (
                [60.2574, -34.0099, 36.2677],
                [60.4626, -34.1751, 39.4387],
                1.2644,
            ),
            (
                [2.0776, 0.0795, -1.1350],
                [0.9033, -0.0636, -0.5514],
                0.9082,
            ),
        ];
        for (a, b, want) in cases {
            let got = delta_e2000(a, b);
            assert!((got - want).abs() < 1e-3, "{a:?} {b:?}: {got} vs {want}");
        }
    }
}
