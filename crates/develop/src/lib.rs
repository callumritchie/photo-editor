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
    /// Texture, clarity and dehaze.
    Presence,
    /// Vibrance and saturation.
    Color,
    ColorMixer,
    /// Treatment (colour / black and white) and the black-and-white mix.
    BlackWhite,
    ColorGrading,
    Sharpening,
    NoiseReduction,
    Vignette,
    Grain,
    /// Crop rectangle and straighten angle (not part of "looks").
    Crop,
    Transform,
    LensCorrections,
    /// Local adjustments (not part of "looks": they belong to one picture).
    Masks,
    /// Technical processing choices (demosaic method, orientation handling). Not part of "looks".
    Processing,
}

impl Group {
    pub const ALL: [Group; 22] = [
        Group::WhiteBalance,
        Group::Exposure,
        Group::Contrast,
        Group::Highlights,
        Group::Shadows,
        Group::Whites,
        Group::Blacks,
        Group::ToneCurve,
        Group::Presence,
        Group::Color,
        Group::ColorMixer,
        Group::BlackWhite,
        Group::ColorGrading,
        Group::Sharpening,
        Group::NoiseReduction,
        Group::Vignette,
        Group::Grain,
        Group::Crop,
        Group::Transform,
        Group::LensCorrections,
        Group::Masks,
        Group::Processing,
    ];

    /// Whether the group is part of a "look" (the default selection for presets and paste).
    pub fn is_look(self) -> bool {
        !matches!(self, Group::Processing | Group::Crop | Group::Masks)
    }

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
            Group::Presence => "Texture, Clarity & Dehaze",
            Group::Color => "Vibrance & Saturation",
            Group::ColorMixer => "Color Mixer",
            Group::BlackWhite => "Black & White",
            Group::ColorGrading => "Color Grading",
            Group::Sharpening => "Sharpening",
            Group::NoiseReduction => "Noise Reduction",
            Group::Vignette => "Vignette",
            Group::Grain => "Grain",
            Group::Crop => "Crop",
            Group::Transform => "Transform",
            Group::LensCorrections => "Lens Corrections",
            Group::Masks => "Masks",
            Group::Processing => "Processing",
        }
    }

    /// Top-level settings keys (JSON names) owned by the group.
    pub fn keys(self) -> &'static [&'static str] {
        match self {
            Group::WhiteBalance => &["whiteBalance"],
            Group::Exposure => &["exposure"],
            Group::Contrast => &["contrast"],
            Group::Highlights => &["highlights"],
            Group::Shadows => &["shadows"],
            Group::Whites => &["whites"],
            Group::Blacks => &["blacks"],
            Group::ToneCurve => &["toneCurve"],
            Group::Presence => &["texture", "clarity", "dehaze"],
            Group::Color => &["vibrance", "saturation"],
            Group::ColorMixer => &["hsl"],
            Group::BlackWhite => &["treatment", "bwMix"],
            Group::ColorGrading => &["grading"],
            Group::Sharpening => &["sharpening"],
            Group::NoiseReduction => &["noiseReduction"],
            Group::Vignette => &["vignette"],
            Group::Grain => &["grain"],
            Group::Crop => &["crop"],
            Group::Transform => &["transform"],
            Group::LensCorrections => &["lens"],
            Group::Masks => &["masks"],
            Group::Processing => &["demosaic", "applyOrientation"],
        }
    }

    /// The group owning a top-level settings key.
    pub fn of_key(key: &str) -> Option<Group> {
        Group::ALL.into_iter().find(|g| g.keys().contains(&key))
    }
}

fn to_json(s: &Settings) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::to_value(s).expect("settings serialise") {
        serde_json::Value::Object(m) => m,
        _ => unreachable!("settings serialise to an object"),
    }
}

/// Copy the given groups from `src` into `dst`, leaving everything else in `dst` unchanged.
pub fn apply_groups(dst: &mut Settings, src: &Settings, groups: &[Group]) {
    let from = to_json(src);
    let mut to = to_json(dst);
    for g in groups {
        for k in g.keys() {
            if let Some(v) = from.get(*k) {
                to.insert((*k).to_string(), v.clone());
            }
        }
    }
    *dst = serde_json::from_value(serde_json::Value::Object(to)).expect("settings round-trip");
}

/// Description of one numeric slider.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamSpec {
    /// Dotted path into the settings JSON, e.g. `exposure` or `hsl.hue.0`.
    pub key: String,
    pub label: String,
    pub group: Group,
    /// Panel section, and an optional tab within it (e.g. "Hue" in the colour mixer).
    pub section: &'static str,
    pub subsection: Option<&'static str>,
    /// Only shown for this treatment ("color" or "monochrome").
    pub when: Option<&'static str>,
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub default: f32,
    /// Decimal places for display.
    pub precision: u8,
    /// Hue sliders show a colour ramp in the UI.
    pub hue: bool,
}

impl ParamSpec {
    /// Name used in history steps, e.g. "Exposure" or "Red Hue".
    pub fn history_label(&self) -> String {
        match self.subsection {
            Some(sub) => format!("{} {}", self.label, sub),
            None => self.label.clone(),
        }
    }
}

/// A setting chosen from a fixed list (shown as a picker).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChoiceSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub section: &'static str,
    pub options: &'static [(&'static str, &'static str)],
}

pub const CHOICES: [ChoiceSpec; 1] = [ChoiceSpec {
    key: "treatment",
    label: "Treatment",
    section: "Tone",
    options: &[("color", "Color"), ("monochrome", "Black & White")],
}];

/// Names of the colour-mixer hue bands, in settings order.
pub const BAND_NAMES: [&str; pe_pipeline::HUE_BANDS] = [
    "Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta",
];

fn spec(
    key: impl Into<String>,
    label: impl Into<String>,
    group: Group,
    section: &'static str,
) -> ParamSpec {
    ParamSpec {
        key: key.into(),
        label: label.into(),
        group,
        section,
        subsection: None,
        when: None,
        min: -100.0,
        max: 100.0,
        step: 1.0,
        default: 0.0,
        precision: 0,
        hue: false,
    }
}

fn build_params() -> Vec<ParamSpec> {
    let mut v = vec![
        ParamSpec {
            min: -5.0,
            max: 5.0,
            step: 0.05,
            precision: 2,
            ..spec("exposure", "Exposure", Group::Exposure, "Tone")
        },
        spec("contrast", "Contrast", Group::Contrast, "Tone"),
        spec("highlights", "Highlights", Group::Highlights, "Tone"),
        spec("shadows", "Shadows", Group::Shadows, "Tone"),
        spec("whites", "Whites", Group::Whites, "Tone"),
        spec("blacks", "Blacks", Group::Blacks, "Tone"),
        spec("texture", "Texture", Group::Presence, "Presence"),
        spec("clarity", "Clarity", Group::Presence, "Presence"),
        spec("dehaze", "Dehaze", Group::Presence, "Presence"),
        spec("vibrance", "Vibrance", Group::Color, "Presence"),
        spec("saturation", "Saturation", Group::Color, "Presence"),
    ];
    for (field, sub) in [
        ("hue", "Hue"),
        ("saturation", "Saturation"),
        ("luminance", "Luminance"),
    ] {
        for (i, name) in BAND_NAMES.iter().enumerate() {
            v.push(ParamSpec {
                subsection: Some(sub),
                when: Some("color"),
                ..spec(
                    format!("hsl.{field}.{i}"),
                    *name,
                    Group::ColorMixer,
                    "Color Mixer",
                )
            });
        }
    }
    for (i, name) in BAND_NAMES.iter().enumerate() {
        v.push(ParamSpec {
            when: Some("monochrome"),
            ..spec(format!("bwMix.{i}"), *name, Group::BlackWhite, "B&W Mix")
        });
    }
    for (region, sub) in [
        ("shadows", "Shadows"),
        ("midtones", "Midtones"),
        ("highlights", "Highlights"),
        ("global", "Global"),
    ] {
        let g = |field: &str, label: &str| ParamSpec {
            subsection: Some(sub),
            ..spec(
                format!("grading.{region}.{field}"),
                label,
                Group::ColorGrading,
                "Color Grading",
            )
        };
        v.push(ParamSpec {
            min: 0.0,
            max: 360.0,
            hue: true,
            ..g("hue", "Hue")
        });
        v.push(ParamSpec {
            min: 0.0,
            ..g("saturation", "Saturation")
        });
        v.push(g("luminance", "Luminance"));
    }
    v.push(ParamSpec {
        min: 0.0,
        default: 50.0,
        ..spec(
            "grading.blending",
            "Blending",
            Group::ColorGrading,
            "Color Grading",
        )
    });
    v.push(spec(
        "grading.balance",
        "Balance",
        Group::ColorGrading,
        "Color Grading",
    ));
    let d = |key: &str, label: &str, group, sub| ParamSpec {
        min: 0.0,
        subsection: Some(sub),
        ..spec(key, label, group, "Detail")
    };
    v.push(ParamSpec {
        max: 150.0,
        ..d(
            "sharpening.amount",
            "Amount",
            Group::Sharpening,
            "Sharpening",
        )
    });
    v.push(ParamSpec {
        min: 0.5,
        max: 3.0,
        step: 0.1,
        default: 1.0,
        precision: 1,
        ..d(
            "sharpening.radius",
            "Radius",
            Group::Sharpening,
            "Sharpening",
        )
    });
    v.push(ParamSpec {
        default: 25.0,
        ..d(
            "sharpening.detail",
            "Detail",
            Group::Sharpening,
            "Sharpening",
        )
    });
    v.push(d(
        "sharpening.masking",
        "Masking",
        Group::Sharpening,
        "Sharpening",
    ));
    v.push(d(
        "noiseReduction.luminance",
        "Luminance",
        Group::NoiseReduction,
        "Noise Reduction",
    ));
    v.push(ParamSpec {
        default: 50.0,
        ..d(
            "noiseReduction.detail",
            "Detail",
            Group::NoiseReduction,
            "Noise Reduction",
        )
    });
    v.push(d(
        "noiseReduction.color",
        "Color",
        Group::NoiseReduction,
        "Noise Reduction",
    ));
    let e = |key: &str, label: &str, group, sub, min: f32, default: f32| ParamSpec {
        min,
        default,
        subsection: Some(sub),
        ..spec(key, label, group, "Effects")
    };
    v.push(e(
        "vignette.amount",
        "Amount",
        Group::Vignette,
        "Vignette",
        -100.0,
        0.0,
    ));
    v.push(e(
        "vignette.midpoint",
        "Midpoint",
        Group::Vignette,
        "Vignette",
        0.0,
        50.0,
    ));
    v.push(e(
        "vignette.roundness",
        "Roundness",
        Group::Vignette,
        "Vignette",
        -100.0,
        0.0,
    ));
    v.push(e(
        "vignette.feather",
        "Feather",
        Group::Vignette,
        "Vignette",
        0.0,
        50.0,
    ));
    v.push(e(
        "vignette.highlights",
        "Highlights",
        Group::Vignette,
        "Vignette",
        0.0,
        0.0,
    ));
    v.push(e("grain.amount", "Amount", Group::Grain, "Grain", 0.0, 0.0));
    v.push(e("grain.size", "Size", Group::Grain, "Grain", 0.0, 25.0));
    v.push(e(
        "grain.roughness",
        "Roughness",
        Group::Grain,
        "Grain",
        0.0,
        50.0,
    ));
    let g = |key: &str, label: &str, group, sub| ParamSpec {
        subsection: Some(sub),
        ..spec(key, label, group, "Geometry")
    };
    v.push(ParamSpec {
        min: -45.0,
        max: 45.0,
        step: 0.1,
        precision: 1,
        ..g("crop.angle", "Angle", Group::Crop, "Crop")
    });
    v.push(g(
        "transform.vertical",
        "Vertical",
        Group::Transform,
        "Transform",
    ));
    v.push(g(
        "transform.horizontal",
        "Horizontal",
        Group::Transform,
        "Transform",
    ));
    v.push(ParamSpec {
        min: 50.0,
        max: 150.0,
        default: 100.0,
        ..g("transform.scale", "Scale", Group::Transform, "Transform")
    });
    v.push(g(
        "lens.distortion",
        "Distortion",
        Group::LensCorrections,
        "Lens",
    ));
    v.push(g(
        "lens.vignetting",
        "Vignetting",
        Group::LensCorrections,
        "Lens",
    ));
    v.push(g(
        "lens.caRedCyan",
        "Red / Cyan Fringe",
        Group::LensCorrections,
        "Lens",
    ));
    v.push(g(
        "lens.caBlueYellow",
        "Blue / Yellow Fringe",
        Group::LensCorrections,
        "Lens",
    ));
    v
}

/// All numeric sliders, in panel order. White-balance temperature/tint are described separately
/// because they only apply in custom mode.
pub fn params() -> &'static [ParamSpec] {
    static P: std::sync::LazyLock<Vec<ParamSpec>> = std::sync::LazyLock::new(build_params);
    &P
}

/// Local adjustment sliders of a mask; keys are relative to the mask's `adjust` object.
pub fn mask_params() -> Vec<ParamSpec> {
    let m = |key: &str, label: &str| spec(key, label, Group::Masks, "Mask");
    vec![
        m("temperature", "Temperature"),
        m("tint", "Tint"),
        ParamSpec {
            min: -4.0,
            max: 4.0,
            step: 0.05,
            precision: 2,
            ..m("exposure", "Exposure")
        },
        m("contrast", "Contrast"),
        m("highlights", "Highlights"),
        m("shadows", "Shadows"),
        m("whites", "Whites"),
        m("blacks", "Blacks"),
        m("texture", "Texture"),
        m("clarity", "Clarity"),
        m("dehaze", "Dehaze"),
        m("saturation", "Saturation"),
        m("sharpness", "Sharpness"),
        ParamSpec {
            min: 0.0,
            ..m("noise", "Noise Reduction")
        },
    ]
}

pub fn temperature_spec() -> ParamSpec {
    ParamSpec {
        min: 2000.0,
        max: 25000.0,
        step: 50.0,
        default: 5500.0,
        ..spec(
            "temperature",
            "Temperature",
            Group::WhiteBalance,
            "White Balance",
        )
    }
}

pub fn tint_spec() -> ParamSpec {
    ParamSpec {
        min: -150.0,
        max: 150.0,
        ..spec("tint", "Tint", Group::WhiteBalance, "White Balance")
    }
}

fn pointer(key: &str) -> String {
    format!("/{}", key.replace('.', "/"))
}

/// Value of a slider (by its dotted key) in settings already converted to JSON.
fn value_in(json: &serde_json::Value, key: &str) -> Option<f32> {
    json.pointer(&pointer(key))?.as_f64().map(|v| v as f32)
}

/// Value of a slider by its dotted key, e.g. `hsl.hue.0`.
pub fn value_of(s: &Settings, key: &str) -> Option<f32> {
    value_in(&serde_json::to_value(s).ok()?, key)
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
    // Values at their defaults are left out, so adding a new setting does not change the digest of
    // existing edits (and does not invalidate their cached previews).
    let defaults = to_json(&Settings::default());
    let mut m = to_json(s);
    m.retain(|k, v| {
        k == "processVersion" || defaults.get(k).map(|d| norm(d.clone())) != Some(norm(v.clone()))
    });
    norm(serde_json::Value::Object(m)).to_string()
}

/// Human-readable label for a history step going from `before` to `after`, e.g. "Exposure +0.50",
/// "White Balance: 5600 K", or "Contrast, Shadows" when several things changed. `None` if nothing changed.
pub fn describe_change(before: &Settings, after: &Settings) -> Option<String> {
    let (ja, jb) = (
        serde_json::to_value(before).ok()?,
        serde_json::to_value(after).ok()?,
    );
    let mut sliders = Vec::new();
    let mut covered: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for p in params() {
        covered.insert(p.key.split('.').next().unwrap_or(""));
        let (Some(a), Some(b)) = (value_in(&ja, &p.key), value_in(&jb, &p.key)) else {
            continue;
        };
        if (a - b).abs() > 1e-6 {
            sliders.push((p.history_label(), b, b - a, p.precision));
        }
    }
    // Other settings that changed, named by their group (in group order, without repeats).
    let mut others: Vec<String> = Vec::new();
    let (ma, mb) = (to_json(before), to_json(after));
    for g in Group::ALL {
        let changed = g
            .keys()
            .iter()
            .any(|k| !covered.contains(k) && ma.get(*k) != mb.get(*k));
        if changed {
            others.push(match (g, after) {
                (Group::WhiteBalance, s) => match &s.white_balance {
                    WhiteBalance::AsShot => "White Balance: As Shot".into(),
                    WhiteBalance::Auto => "White Balance: Auto".into(),
                    WhiteBalance::Custom { temperature, tint } => {
                        format!("White Balance: {temperature:.0} K, tint {tint:+.0}")
                    }
                },
                (Group::BlackWhite, s) if s.treatment != before.treatment => match s.treatment {
                    pe_pipeline::Treatment::Monochrome => "Black & White".into(),
                    pe_pipeline::Treatment::Color => "Color".into(),
                },
                (g, _) => g.label().into(),
            });
        }
    }
    match (sliders.len(), others.len()) {
        (0, 0) => None,
        (1, 0) => {
            let (label, value, delta, prec) = &sliders[0];
            let p = *prec as usize;
            Some(format!("{label} {value:+.p$} ({delta:+.p$})"))
        }
        (0, 1) => Some(others.remove(0)),
        _ => {
            let mut names: Vec<String> = Vec::new();
            for o in others {
                // Multi-change labels use the short name ("White Balance", not its value).
                names.push(o.split(':').next().unwrap_or(&o).to_string());
            }
            names.extend(sliders.into_iter().map(|s| s.0));
            Some(names.join(", "))
        }
    }
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
        let mut d = a.clone();
        d.hsl.hue[0] = 20.0;
        assert_eq!(describe_change(&a, &d).unwrap(), "Red Hue +20 (+20)");
        d.treatment = pe_pipeline::Treatment::Monochrome;
        assert_eq!(describe_change(&a, &d).unwrap(), "Black & White, Red Hue");
        let mut e = a.clone();
        e.tone_curve = vec![[0.0, 10.0], [255.0, 255.0]];
        assert_eq!(describe_change(&a, &e).unwrap(), "Tone Curve");
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
        for p in params() {
            assert_eq!(value_of(&s, &p.key), Some(p.default), "{}", p.key);
            assert_eq!(
                Group::of_key(p.key.split('.').next().unwrap()),
                Some(p.group),
                "{}",
                p.key
            );
        }
        assert_eq!(
            params()
                .iter()
                .filter(|p| p.section == "Color Mixer")
                .count(),
            24
        );
    }

    #[test]
    fn every_settings_key_belongs_to_exactly_one_group() {
        for k in to_json(&Settings::default()).keys() {
            if k == "processVersion" {
                continue;
            }
            let n = Group::ALL
                .iter()
                .filter(|g| g.keys().contains(&k.as_str()))
                .count();
            assert_eq!(n, 1, "{k} is in {n} groups");
        }
    }

    #[test]
    fn digest_ignores_defaults_and_groups_copy_nested_settings() {
        // Default-valued fields are omitted from the canonical form.
        assert_eq!(
            canonical_json(&Settings::default()),
            r#"{"processVersion":1}"#
        );
        let mut src = Settings::default();
        src.hsl.saturation[5] = -40.0;
        src.grading.shadows.hue = 220.0;
        let mut dst = Settings::default();
        apply_groups(&mut dst, &src, &[Group::ColorMixer]);
        assert_eq!(dst.hsl, src.hsl);
        assert_eq!(
            dst.grading,
            Settings::default().grading,
            "grading not selected"
        );
        assert_ne!(digest(&dst), digest(&Settings::default()));
    }
}
