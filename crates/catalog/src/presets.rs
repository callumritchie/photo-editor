//! Editing presets, our preset file format, and importing Adobe-style XMP presets.
//!
//! **`.pepreset`** (our own, exact): JSON `{"format": "photo-editor-preset", "version": 1, "name",
//! "groups", "settings"}` where `settings` holds only the included groups' keys.
//!
//! **Adobe-style `.xmp` presets** (best effort): the `crs:` values of the XMP description are mapped
//! onto our settings by this project's own table ([`map_xmp`]). Our controls are implemented
//! differently, so the result looks similar rather than identical; values with no counterpart are
//! reported back.

use pe_develop::{canonical_json, Group};
use pe_pipeline::{Settings, Treatment, WhiteBalance, HUE_BANDS};
use rusqlite::params;
use serde::Serialize;

use crate::develop::Preset;
use crate::time::now_ms;
use crate::{Catalog, CatalogError, Result};

pub const FORMAT: &str = "photo-editor-preset";
pub const VERSION: u32 = 1;

/// The outcome of importing one preset file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedPreset {
    pub preset: Preset,
    /// Values in the file that have no counterpart here (Adobe-style XMP only).
    pub unmapped: Vec<String>,
}

/// Kind of a preset file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetFile {
    Ours,
    Xmp,
}

impl PresetFile {
    /// By extension (`pepreset`, `xmp`), falling back to sniffing the text.
    pub fn detect(name: &str, text: &str) -> PresetFile {
        let lower = name.to_lowercase();
        if lower.ends_with(".xmp")
            || (!lower.ends_with(".pepreset") && text.trim_start().starts_with('<'))
        {
            PresetFile::Xmp
        } else {
            PresetFile::Ours
        }
    }
}

fn invalid(msg: impl Into<String>) -> CatalogError {
    CatalogError::Invalid(msg.into())
}

/// Only the keys of `groups` from `s`, as a JSON object.
fn partial_json(s: &Settings, groups: &[Group]) -> serde_json::Map<String, serde_json::Value> {
    let full = serde_json::to_value(s).expect("settings serialise");
    let mut out = serde_json::Map::new();
    for g in groups {
        for k in g.keys() {
            if let Some(v) = full.get(*k) {
                out.insert((*k).to_string(), v.clone());
            }
        }
    }
    out
}

impl Catalog {
    fn preset_name_taken(&self, name: &str, except: Option<i64>) -> Result<bool> {
        let id: Option<i64> = rusqlite::OptionalExtension::optional(self.conn.query_row(
            "SELECT id FROM presets WHERE name = ?1",
            [name],
            |r| r.get(0),
        ))?;
        Ok(id.is_some_and(|i| Some(i) != except))
    }

    /// A free name based on `name`: itself, else "name (2)", "name (3)", ...
    fn free_preset_name(&self, name: &str) -> Result<String> {
        let base = name.trim();
        if !self.preset_name_taken(base, None)? {
            return Ok(base.to_string());
        }
        for n in 2.. {
            let candidate = format!("{base} ({n})");
            if !self.preset_name_taken(&candidate, None)? {
                return Ok(candidate);
            }
        }
        unreachable!()
    }

    fn insert_preset(
        &mut self,
        name: &str,
        groups: &[Group],
        settings: &Settings,
    ) -> Result<Preset> {
        if name.trim().is_empty() || groups.is_empty() {
            return Err(invalid("a preset needs a name and at least one group"));
        }
        let groups_json = serde_json::to_string(groups).map_err(|e| invalid(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO presets (uuid, name, groups, settings, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![uuid::Uuid::new_v4().to_string(), name.trim(), groups_json, canonical_json(settings), now_ms()],
        )?;
        self.preset(self.conn.last_insert_rowid())
    }

    /// Rename a preset, change its groups, and optionally replace its values (e.g. with the current
    /// photo's settings).
    pub fn update_preset(
        &mut self,
        id: i64,
        name: &str,
        groups: &[Group],
        settings: Option<&Settings>,
    ) -> Result<Preset> {
        let old = self.preset(id)?;
        let name = name.trim();
        if name.is_empty() || groups.is_empty() {
            return Err(invalid("a preset needs a name and at least one group"));
        }
        if self.preset_name_taken(name, Some(id))? {
            return Err(invalid(format!(
                "there is already a preset called \"{name}\""
            )));
        }
        let settings = settings.unwrap_or(&old.settings);
        let groups_json = serde_json::to_string(groups).map_err(|e| invalid(e.to_string()))?;
        self.conn.execute(
            "UPDATE presets SET name = ?2, groups = ?3, settings = ?4 WHERE id = ?1",
            params![id, name, groups_json, canonical_json(settings)],
        )?;
        self.preset(id)
    }

    /// Copy a preset as "<name> copy" (numbered if taken).
    pub fn duplicate_preset(&mut self, id: i64) -> Result<Preset> {
        let p = self.preset(id)?;
        let name = self.free_preset_name(&format!("{} copy", p.name))?;
        self.insert_preset(&name, &p.groups, &p.settings)
    }

    /// What applying `preset_id` at `amount` would make of the photo's settings, without applying it.
    pub fn preview_preset(&self, image_id: i64, preset_id: i64, amount: f32) -> Result<Settings> {
        let p = self.preset(preset_id)?;
        let mut s = self.develop_settings(image_id)?;
        pe_develop::blend(&mut s, &p.settings, &p.groups, amount);
        Ok(s)
    }

    /// A preset as `.pepreset` file text.
    pub fn export_preset(&self, id: i64) -> Result<String> {
        let p = self.preset(id)?;
        let v = serde_json::json!({
            "format": FORMAT,
            "version": VERSION,
            "name": p.name,
            "groups": p.groups,
            "settings": partial_json(&p.settings, &p.groups),
        });
        Ok(serde_json::to_string_pretty(&v).expect("json") + "\n")
    }

    /// Import a preset file (ours or an Adobe-style XMP); `file_name` names it when the file does not.
    /// A name already in use gets a number.
    pub fn import_preset(&mut self, text: &str, file_name: &str) -> Result<ImportedPreset> {
        let fallback = std::path::Path::new(file_name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Imported".into());
        let (name, groups, settings, unmapped) = match PresetFile::detect(file_name, text) {
            PresetFile::Ours => {
                let (n, g, s) = parse_ours(text)?;
                (n.unwrap_or(fallback), g, s, Vec::new())
            }
            PresetFile::Xmp => {
                let x = parse_xmp(text)?;
                let (s, g, unmapped) = map_xmp(&x);
                if g.is_empty() {
                    return Err(invalid("the XMP file has no settings this app can use"));
                }
                (x.name.unwrap_or(fallback), g, s, unmapped)
            }
        };
        let name = self.free_preset_name(&name)?;
        let preset = self.insert_preset(&name, &groups, &settings)?;
        Ok(ImportedPreset { preset, unmapped })
    }
}

/// Parse our own preset file: (name, groups, settings over defaults).
pub fn parse_ours(text: &str) -> Result<(Option<String>, Vec<Group>, Settings)> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| invalid(format!("not a preset file: {e}")))?;
    if v.get("format").and_then(|f| f.as_str()) != Some(FORMAT) {
        return Err(invalid("not a photo-editor preset file"));
    }
    let version = v.get("version").and_then(|n| n.as_u64()).unwrap_or(0);
    if version == 0 || version > VERSION as u64 {
        return Err(invalid(format!(
            "preset file version {version} is not supported (this app reads version {VERSION})"
        )));
    }
    let groups: Vec<Group> = serde_json::from_value(v.get("groups").cloned().unwrap_or_default())
        .map_err(|e| invalid(format!("bad groups: {e}")))?;
    let mut full = serde_json::to_value(Settings::default()).expect("json");
    if let (Some(obj), Some(part)) = (
        full.as_object_mut(),
        v.get("settings").and_then(|s| s.as_object()),
    ) {
        for (k, val) in part {
            if obj.contains_key(k) {
                obj.insert(k.clone(), val.clone());
            }
        }
    }
    let settings: Settings =
        serde_json::from_value(full).map_err(|e| invalid(format!("bad settings: {e}")))?;
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .filter(|n| !n.trim().is_empty());
    Ok((name, groups, settings.clamped()))
}

// --- Adobe-style XMP presets ---------------------------------------------------------------------

/// The `crs:` values of an XMP preset.
#[derive(Debug, Clone, Default)]
pub struct XmpPreset {
    pub name: Option<String>,
    /// Simple values by name (without the prefix), as written.
    pub values: Vec<(String, String)>,
    /// `ToneCurvePV2012` points as written ("x, y", 0..255).
    pub curve: Vec<String>,
    /// Other list-valued entries (reported as unmapped).
    pub lists: Vec<String>,
}

impl XmpPreset {
    fn get(&self, key: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn num(&self, key: &str) -> Option<f32> {
        self.get(key)?.trim().trim_start_matches('+').parse().ok()
    }
}

/// Read the `crs:` values (attributes and child elements) of an XMP document.
pub fn parse_xmp(text: &str) -> Result<XmpPreset> {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_str(text);
    let mut x = XmpPreset::default();
    // The crs element we are inside (child-element form), and whether inside its rdf:li.
    let mut current: Option<String> = None;
    let mut in_li = false;
    let mut saw_xmp = false;
    loop {
        match r.read_event() {
            Ok(Event::Eof) => break,
            Err(e) => return Err(invalid(format!("not a valid XMP file: {e}"))),
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "rdf:Description" || name == "x:xmpmeta" || name == "rdf:RDF" {
                    saw_xmp = true;
                }
                for a in e.attributes().flatten() {
                    let k = String::from_utf8_lossy(a.key.as_ref()).into_owned();
                    if let Some(k) = k.strip_prefix("crs:") {
                        let v = a
                            .unescape_value()
                            .map(|v| v.into_owned())
                            .unwrap_or_default();
                        x.values.push((k.to_string(), v));
                    }
                }
                if let Some(k) = name.strip_prefix("crs:") {
                    current = Some(k.to_string());
                } else if name == "rdf:li" {
                    in_li = true;
                }
            }
            Ok(Event::Text(t)) => {
                let v = t
                    .unescape()
                    .map(|v| v.trim().to_string())
                    .unwrap_or_default();
                if v.is_empty() {
                    continue;
                }
                match (&current, in_li) {
                    (Some(k), true) if k == "Name" => x.name = Some(v),
                    (Some(k), true) if k == "ToneCurvePV2012" => x.curve.push(v),
                    (Some(k), true) => {
                        if !x.lists.contains(k) {
                            x.lists.push(k.clone());
                        }
                    }
                    (Some(k), false) => x.values.push((k.clone(), v)),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "rdf:li" {
                    in_li = false;
                } else if name.starts_with("crs:") {
                    current = None;
                }
            }
            _ => {}
        }
    }
    if !saw_xmp {
        return Err(invalid("not an XMP file"));
    }
    if x.name.is_none() {
        x.name = x.get("Name").map(str::to_string);
    }
    Ok(x)
}

/// Descriptive / bookkeeping `crs:` entries (never "unmapped").
const XMP_META: &[&str] = &[
    "Version",
    "ProcessVersion",
    "PresetType",
    "UUID",
    "SupportsAmount",
    "SupportsAmount2",
    "SupportsColor",
    "SupportsMonochrome",
    "SupportsHighDynamicRange",
    "SupportsNormalDynamicRange",
    "SupportsSceneReferred",
    "SupportsOutputReferred",
    "RequiresRGBTables",
    "CameraModelRestriction",
    "Copyright",
    "ContactInfo",
    "Name",
    "ShortName",
    "SortName",
    "Group",
    "Description",
    "HasSettings",
    "Cluster",
    "HasCrop",
    "AlreadyApplied",
    "ToneCurveName2012",
    "ToneCurveName",
    "CompatibleVersion",
];

const BANDS: [&str; HUE_BANDS] = [
    "Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta",
];

/// Map an XMP preset onto our settings (over defaults): (settings, groups, unmapped entries).
pub fn map_xmp(x: &XmpPreset) -> (Settings, Vec<Group>, Vec<String>) {
    let mut s = Settings::default();
    let mut groups: Vec<Group> = Vec::new();
    let mut used: Vec<String> = Vec::new();
    let take = |key: &str, used: &mut Vec<String>| -> Option<f32> {
        let v = x.num(key)?;
        used.push(key.to_string());
        Some(v)
    };
    let add = |g: Group, groups: &mut Vec<Group>| {
        if !groups.contains(&g) {
            groups.push(g);
        }
    };
    // Tone.
    for (key, group, field) in [
        ("Exposure2012", Group::Exposure, &mut s.exposure),
        ("Contrast2012", Group::Contrast, &mut s.contrast),
        ("Highlights2012", Group::Highlights, &mut s.highlights),
        ("Shadows2012", Group::Shadows, &mut s.shadows),
        ("Whites2012", Group::Whites, &mut s.whites),
        ("Blacks2012", Group::Blacks, &mut s.blacks),
        ("Texture", Group::Presence, &mut s.texture),
        ("Clarity2012", Group::Presence, &mut s.clarity),
        ("Dehaze", Group::Presence, &mut s.dehaze),
        ("Vibrance", Group::Color, &mut s.vibrance),
        ("Saturation", Group::Color, &mut s.saturation),
    ] {
        if let Some(v) = take(key, &mut used) {
            *field = v;
            add(group, &mut groups);
        }
    }
    // Colour mixer.
    for (b, band) in BANDS.iter().enumerate() {
        for (prefix, arr) in [
            ("HueAdjustment", &mut s.hsl.hue),
            ("SaturationAdjustment", &mut s.hsl.saturation),
            ("LuminanceAdjustment", &mut s.hsl.luminance),
        ] {
            if let Some(v) = take(&format!("{prefix}{band}"), &mut used) {
                arr[b] = v;
                add(Group::ColorMixer, &mut groups);
            }
        }
    }
    // Black and white.
    if let Some(v) = x.get("ConvertToGrayscale") {
        used.push("ConvertToGrayscale".into());
        s.treatment = if v.eq_ignore_ascii_case("true") {
            Treatment::Monochrome
        } else {
            Treatment::Color
        };
        add(Group::BlackWhite, &mut groups);
    }
    for (b, band) in BANDS.iter().enumerate() {
        if let Some(v) = take(&format!("GrayMixer{band}"), &mut used) {
            s.bw_mix[b] = v;
            add(Group::BlackWhite, &mut groups);
        }
    }
    // Colour grading.
    for (prefix, wheel) in [
        ("Shadow", &mut s.grading.shadows),
        ("Midtone", &mut s.grading.midtones),
        ("Highlight", &mut s.grading.highlights),
        ("Global", &mut s.grading.global),
    ] {
        let w = wheel;
        for (suffix, f) in [
            ("Hue", &mut w.hue),
            ("Sat", &mut w.saturation),
            ("Lum", &mut w.luminance),
        ] {
            if let Some(v) = take(&format!("ColorGrade{prefix}{suffix}"), &mut used) {
                *f = v;
                add(Group::ColorGrading, &mut groups);
            }
        }
    }
    if let Some(v) = take("ColorGradeBlending", &mut used) {
        s.grading.blending = v;
        add(Group::ColorGrading, &mut groups);
    }
    if let Some(v) = take("ColorGradeBalance", &mut used) {
        s.grading.balance = v;
        add(Group::ColorGrading, &mut groups);
    }
    // Point curve (0..255 pairs); an identity curve means none.
    if !x.curve.is_empty() {
        used.push("ToneCurvePV2012".into());
        let pts: Vec<[f32; 2]> = x
            .curve
            .iter()
            .filter_map(|p| {
                let mut it = p.split(',').map(|v| v.trim().parse::<f32>().ok());
                Some([it.next()?? / 255.0, it.next()?? / 255.0])
            })
            .collect();
        let identity = pts.iter().all(|p| (p[0] - p[1]).abs() < 1e-3);
        if !identity && pts.len() >= 2 {
            s.tone_curve = pts;
        }
        add(Group::ToneCurve, &mut groups);
    }
    // Detail.
    for (key, f) in [
        ("Sharpness", &mut s.sharpening.amount),
        ("SharpenRadius", &mut s.sharpening.radius),
        ("SharpenDetail", &mut s.sharpening.detail),
        ("SharpenEdgeMasking", &mut s.sharpening.masking),
    ] {
        if let Some(v) = take(key, &mut used) {
            *f = v;
            add(Group::Sharpening, &mut groups);
        }
    }
    for (key, f) in [
        ("LuminanceSmoothing", &mut s.noise_reduction.luminance),
        (
            "LuminanceNoiseReductionDetail",
            &mut s.noise_reduction.detail,
        ),
        ("ColorNoiseReduction", &mut s.noise_reduction.color),
    ] {
        if let Some(v) = take(key, &mut used) {
            *f = v;
            add(Group::NoiseReduction, &mut groups);
        }
    }
    // Effects.
    for (key, f) in [
        ("PostCropVignetteAmount", &mut s.vignette.amount),
        ("PostCropVignetteMidpoint", &mut s.vignette.midpoint),
        ("PostCropVignetteRoundness", &mut s.vignette.roundness),
        ("PostCropVignetteFeather", &mut s.vignette.feather),
        (
            "PostCropVignetteHighlightContrast",
            &mut s.vignette.highlights,
        ),
    ] {
        if let Some(v) = take(key, &mut used) {
            *f = v;
            add(Group::Vignette, &mut groups);
        }
    }
    for (key, f) in [
        ("GrainAmount", &mut s.grain.amount),
        ("GrainSize", &mut s.grain.size),
        ("GrainFrequency", &mut s.grain.roughness),
    ] {
        if let Some(v) = take(key, &mut used) {
            *f = v;
            add(Group::Grain, &mut groups);
        }
    }
    // White balance.
    if let Some(wb) = x.get("WhiteBalance") {
        used.push("WhiteBalance".into());
        let custom = || Some((x.num("Temperature")?, x.num("Tint").unwrap_or(0.0)));
        s.white_balance = match wb {
            "As Shot" => Some(WhiteBalance::AsShot),
            "Auto" => Some(WhiteBalance::Auto),
            _ => custom().map(|(temperature, tint)| WhiteBalance::Custom { temperature, tint }),
        }
        .unwrap_or_default();
        used.extend(["Temperature".into(), "Tint".into()]);
        add(Group::WhiteBalance, &mut groups);
    }
    // Everything else that would change a picture is reported.
    let neutral = |v: &str| {
        let t = v.trim().trim_start_matches('+');
        t.is_empty() || t.parse::<f64>().is_ok_and(|n| n == 0.0) || t.eq_ignore_ascii_case("false")
    };
    let mut unmapped: Vec<String> = x
        .values
        .iter()
        .filter(|(k, v)| !used.contains(k) && !XMP_META.contains(&k.as_str()) && !neutral(v))
        .map(|(k, _)| k.clone())
        .chain(
            x.lists
                .iter()
                .filter(|k| !XMP_META.contains(&k.as_str()))
                .cloned(),
        )
        .collect();
    unmapped.sort();
    unmapped.dedup();
    groups.sort_by_key(|g| Group::ALL.iter().position(|a| a == g));
    (s.clamped(), groups, unmapped)
}
