//! Output RGB colour spaces: primaries, white point, transfer function.
//!
//! Primaries and transfer functions are from the published specifications:
//! sRGB (IEC 61966-2-1), Display P3 (DCI-P3 primaries, D65 white, sRGB transfer), Adobe RGB (1998)
//! (gamma 563/256), and ProPhoto / ROMM RGB (ISO 22028-2, D50 white; encoded here with a pure 1.8
//! gamma to match the embedded ICC profile).

use crate::{invert, mul, Mat3};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ColorSpace {
    #[default]
    Srgb,
    DisplayP3,
    AdobeRgb,
    ProPhoto,
}

pub const D65_XY: [f64; 2] = [0.3127, 0.3290];
pub const D50_XY: [f64; 2] = [0.3457, 0.3585];

/// Bradford cone response matrix.
const BRADFORD: [[f64; 3]; 3] = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];

fn xy_to_xyz([x, y]: [f64; 2]) -> [f64; 3] {
    [x / y, 1.0, (1.0 - x - y) / y]
}

fn m3(m: [[f64; 3]; 3]) -> Mat3 {
    m.map(|r| r.map(|v| v as f32))
}

fn mul64(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

fn inv64(m: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let d = 1.0 / det;
    [
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
    ]
}

/// RGB -> XYZ for the given primaries and white (standard derivation, white has Y = 1).
pub fn rgb_to_xyz(primaries: [[f64; 2]; 3], white: [f64; 2]) -> [[f64; 3]; 3] {
    let cols = primaries.map(xy_to_xyz);
    let p = [
        [cols[0][0], cols[1][0], cols[2][0]],
        [cols[0][1], cols[1][1], cols[2][1]],
        [cols[0][2], cols[1][2], cols[2][2]],
    ];
    let w = xy_to_xyz(white);
    let pi = inv64(&p);
    let s: Vec<f64> = (0..3)
        .map(|i| (0..3).map(|k| pi[i][k] * w[k]).sum())
        .collect();
    [
        [p[0][0] * s[0], p[0][1] * s[1], p[0][2] * s[2]],
        [p[1][0] * s[0], p[1][1] * s[1], p[1][2] * s[2]],
        [p[2][0] * s[0], p[2][1] * s[1], p[2][2] * s[2]],
    ]
}

/// Bradford chromatic adaptation matrix (XYZ under `from` white -> XYZ under `to` white).
pub fn bradford(from: [f64; 2], to: [f64; 2]) -> [[f64; 3]; 3] {
    let (ws, wd) = (xy_to_xyz(from), xy_to_xyz(to));
    let cone = |w: [f64; 3]| -> [f64; 3] {
        [0, 1, 2].map(|i| (0..3).map(|k| BRADFORD[i][k] * w[k]).sum())
    };
    let (cs, cd) = (cone(ws), cone(wd));
    let scale = [
        [cd[0] / cs[0], 0.0, 0.0],
        [0.0, cd[1] / cs[1], 0.0],
        [0.0, 0.0, cd[2] / cs[2]],
    ];
    mul64(&inv64(&BRADFORD), &mul64(&scale, &BRADFORD))
}

/// Transfer function family.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transfer {
    /// IEC 61966-2-1 piecewise curve.
    Srgb,
    /// Pure power law with this exponent.
    Gamma(f64),
}

impl ColorSpace {
    pub const ALL: [ColorSpace; 4] = [
        ColorSpace::Srgb,
        ColorSpace::DisplayP3,
        ColorSpace::AdobeRgb,
        ColorSpace::ProPhoto,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ColorSpace::Srgb => "sRGB",
            ColorSpace::DisplayP3 => "Display P3",
            ColorSpace::AdobeRgb => "Adobe RGB (1998) compatible",
            ColorSpace::ProPhoto => "ProPhoto RGB compatible",
        }
    }

    pub fn primaries(self) -> [[f64; 2]; 3] {
        match self {
            ColorSpace::Srgb => [[0.64, 0.33], [0.30, 0.60], [0.15, 0.06]],
            ColorSpace::DisplayP3 => [[0.680, 0.320], [0.265, 0.690], [0.150, 0.060]],
            ColorSpace::AdobeRgb => [[0.64, 0.33], [0.21, 0.71], [0.15, 0.06]],
            ColorSpace::ProPhoto => [[0.7347, 0.2653], [0.1596, 0.8404], [0.0366, 0.0001]],
        }
    }

    pub fn white(self) -> [f64; 2] {
        match self {
            ColorSpace::ProPhoto => D50_XY,
            _ => D65_XY,
        }
    }

    pub fn transfer(self) -> Transfer {
        match self {
            ColorSpace::Srgb | ColorSpace::DisplayP3 => Transfer::Srgb,
            ColorSpace::AdobeRgb => Transfer::Gamma(563.0 / 256.0),
            ColorSpace::ProPhoto => Transfer::Gamma(1.8),
        }
    }

    /// Linear RGB of this space -> XYZ relative to D65 (D50 spaces are Bradford-adapted).
    pub fn to_xyz_d65(self) -> Mat3 {
        let native = rgb_to_xyz(self.primaries(), self.white());
        if self.white() == D65_XY {
            m3(native)
        } else {
            m3(mul64(&bradford(self.white(), D65_XY), &native))
        }
    }

    /// Linear RGB of this space -> XYZ relative to D50 (ICC profile connection space).
    pub fn to_xyz_d50(self) -> [[f64; 3]; 3] {
        let native = rgb_to_xyz(self.primaries(), self.white());
        if self.white() == D50_XY {
            native
        } else {
            mul64(&bradford(self.white(), D50_XY), &native)
        }
    }

    /// Matrix from another D65-referred linear space into this one.
    pub fn from_space(self, src_to_xyz_d65: &Mat3) -> Mat3 {
        mul(
            &invert(&self.to_xyz_d65()).expect("invertible primaries"),
            src_to_xyz_d65,
        )
    }

    #[inline]
    pub fn encode(self, x: f32) -> f32 {
        match self.transfer() {
            Transfer::Srgb => crate::srgb_encode(x),
            Transfer::Gamma(g) => x.max(0.0).powf((1.0 / g) as f32),
        }
    }

    #[inline]
    pub fn decode(self, v: f32) -> f32 {
        match self.transfer() {
            Transfer::Srgb => crate::srgb_decode(v),
            Transfer::Gamma(g) => v.max(0.0).powf(g as f32),
        }
    }

    /// Luminance weights (second row of RGB -> XYZ).
    pub fn luma(self) -> [f32; 3] {
        self.to_xyz_d65()[1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, e: f32) -> bool {
        (a - b).abs() <= e
    }

    #[test]
    fn derived_srgb_matches_standard_matrix() {
        let m = ColorSpace::Srgb.to_xyz_d65();
        for (row, std_row) in m.iter().zip(crate::SRGB_TO_XYZ_D65) {
            for (v, s) in row.iter().zip(std_row) {
                assert!(close(*v, s, 5e-4), "{v} vs {s}");
            }
        }
    }

    #[test]
    fn whites_map_to_d65_and_d50() {
        for cs in ColorSpace::ALL {
            let w = crate::apply(&cs.to_xyz_d65(), [1.0, 1.0, 1.0]);
            assert!(
                close(w[0], 0.9505, 2e-3) && close(w[1], 1.0, 1e-4) && close(w[2], 1.089, 3e-3),
                "{cs:?} {w:?}"
            );
            let d50 = cs.to_xyz_d50();
            let w50 = [0, 1, 2].map(|i| d50[i].iter().sum::<f64>());
            assert!(
                (w50[0] - 0.9642).abs() < 2e-3 && (w50[2] - 0.8249).abs() < 3e-3,
                "{cs:?} {w50:?}"
            );
        }
    }

    #[test]
    fn transfer_roundtrips() {
        for cs in ColorSpace::ALL {
            for i in 0..=20 {
                let x = i as f32 / 20.0;
                assert!(close(cs.decode(cs.encode(x)), x, 1e-5), "{cs:?}");
            }
        }
    }
}
