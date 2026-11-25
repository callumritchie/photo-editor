//! The non-destructive editing model: what a photo's develop settings are, how they are grouped,
//! fingerprinted and described. Storage (history, snapshots, presets) lives in `pe-catalog`.
//!
//! - [`PARAMS`] describes every slider (label, group, range, step, default) for UIs and validation.
//! - [`Group`] is the unit of partial copy/paste, sync and presets.
//! - [`digest`] fingerprints settings canonically; previews and caches are keyed by it.
//! - [`describe_change`] names a history step from the difference between two states.

use pe_pipeline::{Settings, WhiteBalance};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A selectable group of settings for copy/paste, sync and presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Group {
    WhiteBalance,
    Exposure,
    Contrast,
    Highlights,
    Shadows,
    Whites,
    Blacks,
    ToneCurve,
    /// Technical processing choices (demosaic method, orientation handling). Not part of "looks".
    Processing,
}

impl Group {
    pub const ALL: [Group; 9] = [
        Group::WhiteBalance,
        Group::Exposure,
        Group::Contrast,
        Group::Highlights,
        Group::Shadows,
        Group::Whites,
        Group::Blacks,
        Group::ToneCurve,
        Group::Processing,
    ];

    /// Groups that make up a "look" (the default selection for presets and paste).
    pub const LOOK: [Group; 8] = [
        Group::WhiteBalance,
        Group::Exposure,
        Group::Contrast,
        Group::Highlights,
        Group::Shadows,
        Group::Whites,
        Group::Blacks,
        Group::ToneCurve,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Group::WhiteBalance => "White Balance",
            Group::Exposure => "Exposure",
            Group::Contrast => "Contrast",
            Group::Highlights => "Highlights",
            Group::Shadows => "Shadows",
            Group::Whites => "Whites",
            Group::Blacks => "Blacks",
            Group::ToneCurve => "Tone Curve",
            Group::Processing => "Processing",
        }
    }
}

/// Copy the given groups from `src` into `dst`, leaving everything else in `dst` unchanged.
pub fn apply_groups(dst: &mut Settings, src: &Settings, groups: &[Group]) {
    for g in groups {
        match g {
            Group::WhiteBalance => dst.white_balance = src.white_balance.clone(),
            Group::Exposure => dst.exposure = src.exposure,
            Group::Contrast => dst.contrast = src.contrast,
            Group::Highlights => dst.highlights = src.highlights,
            Group::Shadows => dst.shadows = src.shadows,
            Group::Whites => dst.whites = src.whites,
            Group::Blacks => dst.blacks = src.blacks,
            Group::ToneCurve => dst.tone_curve = src.tone_curve.clone(),
            Group::Processing => {
                dst.demosaic = src.demosaic;
                dst.apply_orientation = src.apply_orientation;
            }
        }
    }
}

/// Description of one numeric slider.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamSpec {
    /// Field name in the settings JSON.
    pub key: &'static str,
    pub label: &'static str,
    pub group: Group,
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub default: f32,
    /// Decimal places for display.
    pub precision: u8,
}

const fn slider(key: &'static str, label: &'static str, group: Group) -> ParamSpec {
    ParamSpec {
        key,
        label,
        group,
        min: -100.0,
        max: 100.0,
        step: 1.0,
        default: 0.0,
        precision: 0,
    }
}

/// All numeric sliders, in panel order. White-balance temperature/tint are described separately
/// because they only apply in custom mode.
pub const PARAMS: [ParamSpec; 6] = [
    ParamSpec {
        key: "exposure",
        label: "Exposure",
        group: Group::Exposure,
        min: -5.0,
        max: 5.0,
        step: 0.05,
        default: 0.0,
        precision: 2,
    },
    slider("contrast", "Contrast", Group::Contrast),
    slider("highlights", "Highlights", Group::Highlights),
    slider("shadows", "Shadows", Group::Shadows),
    slider("whites", "Whites", Group::Whites),
    slider("blacks", "Blacks", Group::Blacks),
];

pub const TEMPERATURE: ParamSpec = ParamSpec {
    key: "temperature",
    label: "Temperature",
    group: Group::WhiteBalance,
    min: 2000.0,
    max: 25000.0,
    step: 50.0,
    default: 5500.0,
    precision: 0,
};

pub const TINT: ParamSpec = ParamSpec {
    key: "tint",
    label: "Tint",
    group: Group::WhiteBalance,
    min: -150.0,
    max: 150.0,
    step: 1.0,
    default: 0.0,
    precision: 0,
};

fn value_of(s: &Settings, key: &str) -> Option<f32> {
    Some(match key {
        "exposure" => s.exposure,
        "contrast" => s.contrast,
        "highlights" => s.highlights,
        "shadows" => s.shadows,
        "whites" => s.whites,
        "blacks" => s.blacks,
        _ => return None,
    })
}

/// Canonical fingerprint of settings (hex, 32 chars). Equal settings always give equal digests;
/// float values are normalised so -0.0 and 0.0, or tiny serialisation noise, do not differ.
pub fn digest(s: &Settings) -> String {
    let canonical = canonical_json(s);
    let hash = Sha256::digest(canonical.as_bytes());
    hash[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Settings serialised with keys sorted and floats rounded to 6 significant decimals.
pub fn canonical_json(s: &Settings) -> String {
    fn norm(v: serde_json::Value) -> serde_json::Value {
        use serde_json::Value as V;
        match v {
            V::Number(n) => match n.as_f64() {
                Some(f) if n.is_f64() => {
                    let r = (f * 1e6).round() / 1e6;
                    serde_json::json!(if r == 0.0 { 0.0 } else { r })
                }
                _ => V::Number(n),
            },
            V::Array(a) => V::Array(a.into_iter().map(norm).collect()),
            V::Object(o) => {
                let mut items: Vec<_> = o.into_iter().map(|(k, v)| (k, norm(v))).collect();
                items.sort_by(|a, b| a.0.cmp(&b.0));
                V::Object(items.into_iter().collect())
            }
            other => other,
        }
    }
    norm(serde_json::to_value(s).expect("settings serialise")).to_string()
}

/// Human-readable label for a history step going from `before` to `after`, e.g. "Exposure +0.50",
/// "White Balance: 5600 K", or "Contrast, Shadows" when several sliders changed. `None` if nothing changed.
pub fn describe_change(before: &Settings, after: &Settings) -> Option<String> {
    let mut parts = Vec::new();
    for p in PARAMS {
        let (a, b) = (value_of(before, p.key)?, value_of(after, p.key)?);
        if (a - b).abs() > 1e-6 {
            parts.push((p.label, b, b - a, p.precision));
        }
    }
    let wb_changed = before.white_balance != after.white_balance;
    let curve_changed = before.tone_curve != after.tone_curve;
    let other_changed =
        before.demosaic != after.demosaic || before.apply_orientation != after.apply_orientation;
    let changed =
        parts.len() + wb_changed as usize + curve_changed as usize + other_changed as usize;
    if changed == 0 {
        return None;
    }
    if changed == 1 {
        if let Some((label, value, delta, prec)) = parts.first() {
            let p = *prec as usize;
            return Some(format!("{label} {value:+.p$} ({delta:+.p$})"));
        }
        if wb_changed {
            return Some(match &after.white_balance {
                WhiteBalance::AsShot => "White Balance: As Shot".into(),
                WhiteBalance::Auto => "White Balance: Auto".into(),
                WhiteBalance::Custom { temperature, tint } => {
                    format!("White Balance: {temperature:.0} K, tint {tint:+.0}")
                }
            });
        }
        if curve_changed {
            return Some("Tone Curve".into());
        }
        return Some("Processing".into());
    }
    let mut names: Vec<&str> = parts.iter().map(|p| p.0).collect();
    if wb_changed {
        names.insert(0, "White Balance");
    }
    if curve_changed {
        names.push("Tone Curve");
    }
    if other_changed {
        names.push("Processing");
    }
    Some(names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_and_sensitive() {
        let a = Settings::default();
        let mut b = Settings::default();
        assert_eq!(digest(&a), digest(&b));
        b.exposure = -0.0;
        assert_eq!(digest(&a), digest(&b), "negative zero is normalised");
        b.exposure = 0.3;
        assert_ne!(digest(&a), digest(&b));
        assert_eq!(digest(&a).len(), 32);
    }

    #[test]
    fn groups_copy_only_selected_fields() {
        let src = Settings {
            exposure: 1.0,
            contrast: 20.0,
            white_balance: WhiteBalance::Auto,
            ..Default::default()
        };
        let mut dst = Settings {
            shadows: 40.0,
            ..Default::default()
        };
        apply_groups(&mut dst, &src, &[Group::Exposure, Group::WhiteBalance]);
        assert_eq!(dst.exposure, 1.0);
        assert_eq!(dst.contrast, 0.0, "contrast not selected");
        assert_eq!(dst.shadows, 40.0, "unrelated value kept");
        assert_eq!(dst.white_balance, WhiteBalance::Auto);
    }

    #[test]
    fn change_descriptions() {
        let a = Settings::default();
        let mut b = a.clone();
        assert_eq!(describe_change(&a, &b), None);
        b.exposure = 0.5;
        assert_eq!(describe_change(&a, &b).unwrap(), "Exposure +0.50 (+0.50)");
        b.shadows = 30.0;
        assert_eq!(describe_change(&a, &b).unwrap(), "Exposure, Shadows");
        let mut c = a.clone();
        c.white_balance = WhiteBalance::Custom {
            temperature: 5600.0,
            tint: 4.0,
        };
        assert_eq!(
            describe_change(&a, &c).unwrap(),
            "White Balance: 5600 K, tint +4"
        );
    }

    #[test]
    fn every_param_maps_to_a_settings_field() {
        let s = Settings::default();
        for p in PARAMS {
            assert_eq!(value_of(&s, p.key), Some(p.default), "{}", p.key);
            let json = serde_json::to_value(&s).unwrap();
            assert!(
                json.get(p.key).is_some(),
                "{} missing from settings JSON",
                p.key
            );
        }
    }
}
