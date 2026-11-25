//! Correlated colour temperature (CCT) and tint.
//!
//! - Planckian locus chromaticity from the cubic-spline approximation of Kim et al. (2002),
//!   valid for 1667 K to 25000 K.
//! - Tint is an offset perpendicular to the locus in the CIE 1960 (u, v) diagram, as Duv.
//!   **Our convention:** `tint = -Duv * 3000`, so positive tint moves away from green (towards
//!   magenta) and +/-150 tint corresponds to Duv -/+0.05.

/// Lowest and highest temperatures supported by the locus approximation.
pub const T_MIN: f32 = 1667.0;
pub const T_MAX: f32 = 25000.0;

const TINT_SCALE: f64 = 3000.0;

/// CIE xy chromaticity of a black-body radiator at `kelvin` (clamped to the supported range).
pub fn planck_xy(kelvin: f32) -> [f64; 2] {
    let t = (kelvin as f64).clamp(T_MIN as f64, T_MAX as f64);
    let (t2, t3) = (t * t, t * t * t);
    let x = if t <= 4000.0 {
        -0.266_123_9e9 / t3 - 0.234_358_9e6 / t2 + 0.877_695_6e3 / t + 0.179_910
    } else {
        -3.025_846_9e9 / t3 + 2.107_037_9e6 / t2 + 0.222_634_7e3 / t + 0.240_390
    };
    let (x2, x3) = (x * x, x * x * x);
    let y = if t <= 2222.0 {
        -1.106_381_4 * x3 - 1.348_110_20 * x2 + 2.185_558_32 * x - 0.202_196_83
    } else if t <= 4000.0 {
        -0.954_947_6 * x3 - 1.374_185_93 * x2 + 2.091_370_15 * x - 0.167_488_67
    } else {
        3.081_758_0 * x3 - 5.873_386_70 * x2 + 3.751_129_97 * x - 0.370_014_83
    };
    [x, y]
}

fn xy_to_uv([x, y]: [f64; 2]) -> [f64; 2] {
    let d = -2.0 * x + 12.0 * y + 3.0;
    [4.0 * x / d, 6.0 * y / d]
}

fn uv_to_xy([u, v]: [f64; 2]) -> [f64; 2] {
    let d = 2.0 * u - 8.0 * v + 4.0;
    [3.0 * u / d, 2.0 * v / d]
}

/// Unit normal to the locus at `kelvin` in (u, v), pointing to the green side (positive Duv).
fn locus_normal(kelvin: f64) -> [f64; 2] {
    let a = xy_to_uv(planck_xy(kelvin as f32));
    let b = xy_to_uv(planck_xy((kelvin * 1.01) as f32));
    let (du, dv) = (b[0] - a[0], b[1] - a[1]);
    let len = (du * du + dv * dv).sqrt().max(1e-12);
    let n = [-dv / len, du / len];
    if n[1] >= 0.0 {
        n
    } else {
        [-n[0], -n[1]]
    }
}

/// Chromaticity (CIE xy) for a temperature and tint.
pub fn temp_tint_to_xy(kelvin: f32, tint: f32) -> [f64; 2] {
    let k = (kelvin as f64).clamp(T_MIN as f64, T_MAX as f64);
    let uv0 = xy_to_uv(planck_xy(k as f32));
    let n = locus_normal(k);
    let duv = -(tint as f64) / TINT_SCALE;
    uv_to_xy([uv0[0] + duv * n[0], uv0[1] + duv * n[1]])
}

/// Nearest temperature and tint for a chromaticity (inverse of [`temp_tint_to_xy`]).
pub fn xy_to_temp_tint(xy: [f64; 2]) -> (f32, f32) {
    let uv = xy_to_uv(xy);
    let dist2 = |lt: f64| {
        let p = xy_to_uv(planck_xy(lt.exp() as f32));
        (p[0] - uv[0]).powi(2) + (p[1] - uv[1]).powi(2)
    };
    // Coarse scan then golden-section refinement over ln(T).
    let (lo, hi) = ((T_MIN as f64).ln(), (T_MAX as f64).ln());
    let steps = 200;
    let best = (0..=steps)
        .map(|i| lo + (hi - lo) * i as f64 / steps as f64)
        .min_by(|a, b| dist2(*a).total_cmp(&dist2(*b)))
        .unwrap();
    let step = (hi - lo) / steps as f64;
    let (mut a, mut b) = ((best - step).max(lo), (best + step).min(hi));
    let g = (5f64.sqrt() - 1.0) / 2.0;
    for _ in 0..60 {
        let c = b - g * (b - a);
        let d = a + g * (b - a);
        if dist2(c) < dist2(d) {
            b = d;
        } else {
            a = c;
        }
    }
    let lt = (a + b) / 2.0;
    let kelvin = lt.exp();
    let p = xy_to_uv(planck_xy(kelvin as f32));
    let n = locus_normal(kelvin);
    let duv = (uv[0] - p[0]) * n[0] + (uv[1] - p[1]) * n[1];
    (kelvin as f32, (-duv * TINT_SCALE) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d65_is_about_6500k() {
        // D65 white xy = (0.3127, 0.3290): CCT ~6504 K, slightly above the locus (green side).
        let (t, tint) = xy_to_temp_tint([0.3127, 0.3290]);
        assert!((t - 6504.0).abs() < 60.0, "{t}");
        assert!(tint < 0.0 && tint > -15.0, "{tint}");
    }

    #[test]
    fn roundtrip() {
        for &(t, tint) in &[
            (2800.0, 0.0),
            (5000.0, 20.0),
            (6500.0, -30.0),
            (12000.0, 10.0),
        ] {
            let (t2, tint2) = xy_to_temp_tint(temp_tint_to_xy(t, tint));
            assert!((t2 - t).abs() / t < 0.005, "{t} -> {t2}");
            assert!((tint2 - tint).abs() < 0.5, "{tint} -> {tint2}");
        }
    }

    #[test]
    fn locus_at_known_points() {
        // Illuminant A (2856 K) lies on the locus near xy (0.4476, 0.4074).
        let [x, y] = planck_xy(2856.0);
        assert!(
            (x - 0.4476).abs() < 0.002 && (y - 0.4074).abs() < 0.002,
            "{x} {y}"
        );
    }
}
