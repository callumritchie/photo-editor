//! Colour science helpers: 3x3 matrix maths, standard primaries, transfer functions,
//! and the camera-to-output colour transform.

/// Row-major 3x3 matrix.
pub type Mat3 = [[f32; 3]; 3];

/// Linear sRGB (Rec.709 primaries, D65 white) to CIE XYZ (D65). IEC 61966-2-1.
pub const SRGB_TO_XYZ_D65: Mat3 = [
    [0.412_456_4, 0.357_576_1, 0.180_437_5],
    [0.212_672_9, 0.715_152_2, 0.072_175_0],
    [0.019_333_9, 0.119_192, 0.950_304_1],
];

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
    let mut cam_from_srgb = mul(xyz_to_cam, &SRGB_TO_XYZ_D65);
    for row in cam_from_srgb.iter_mut() {
        let s: f32 = row.iter().sum();
        if s.abs() < 1e-9 {
            return None;
        }
        for v in row.iter_mut() {
            *v /= s;
        }
    }
    invert(&cam_from_srgb)
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
    fn camera_matrix_preserves_neutral() {
        // An arbitrary plausible camera matrix (not from any real camera).
        let xyz_to_cam: Mat3 = [[0.9, -0.3, -0.05], [-0.4, 1.2, 0.2], [-0.05, 0.15, 0.6]];
        let m = camera_to_srgb(&xyz_to_cam).unwrap();
        let out = apply(&m, [1.0, 1.0, 1.0]);
        for v in out {
            assert!(close(v, 1.0, 1e-4), "{out:?}");
        }
    }
}
