//! C ABI for the macOS app. See `include/pe_ffi.h` for the contract.
//!
//! Conventions:
//! - Strings in and out are NUL-terminated UTF-8. Returned strings are owned by the caller and must be
//!   released with [`pe_string_free`]; returned byte buffers with [`pe_bytes_free`].
//! - Structured data crosses as JSON. Failures return `{"error": "..."}` (JSON functions) or NULL/false.
//! - A catalog handle may be used from any thread; calls are serialised internally.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;

use pe_catalog::{Catalog, Flag, ImportOptions, Query};
use serde::Serialize;

pub struct PeCatalog {
    inner: Mutex<Catalog>,
}

fn json_out<T: Serialize>(v: &T) -> *mut c_char {
    let s = serde_json::to_string(v).unwrap_or_else(|e| format!(r#"{{"error":"{e}"}}"#));
    CString::new(s)
        .map(CString::into_raw)
        .unwrap_or(ptr::null_mut())
}

fn error_out(msg: impl std::fmt::Display) -> *mut c_char {
    json_out(&serde_json::json!({ "error": msg.to_string() }))
}

unsafe fn str_in<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        CStr::from_ptr(p).to_str().ok()
    }
}

unsafe fn catalog<'a>(h: *mut PeCatalog) -> Option<&'a PeCatalog> {
    h.as_ref()
}

fn bytes_out(mut v: Vec<u8>, out_len: *mut usize) -> *mut u8 {
    v.shrink_to_fit();
    let b = v.into_boxed_slice();
    unsafe {
        if !out_len.is_null() {
            *out_len = b.len();
        }
    }
    Box::into_raw(b) as *mut u8
}

/// Open a catalog directory, creating it first if `create` is true and it does not exist.
/// On failure returns NULL and, if `err_out` is non-NULL, stores an error string there.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_open(
    path: *const c_char,
    create: bool,
    err_out: *mut *mut c_char,
) -> *mut PeCatalog {
    let res = (|| {
        let p = Path::new(str_in(path).ok_or("invalid path")?);
        let cat = if create && !p.exists() {
            Catalog::create(p)
        } else {
            Catalog::open(p)
        };
        cat.map_err(|e| e.to_string())
    })();
    match res {
        Ok(c) => Box::into_raw(Box::new(PeCatalog {
            inner: Mutex::new(c),
        })),
        Err(e) => {
            if !err_out.is_null() {
                *err_out = CString::new(e)
                    .map(CString::into_raw)
                    .unwrap_or(ptr::null_mut());
            }
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn pe_catalog_close(h: *mut PeCatalog) {
    if !h.is_null() {
        drop(Box::from_raw(h));
    }
}

/// JSON array of folders.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_folders_json(h: *mut PeCatalog) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    match c.inner.lock().unwrap().folders() {
        Ok(v) => json_out(&v),
        Err(e) => error_out(e),
    }
}

/// JSON array of images matching a JSON query (`{"folderId":..,"recursive":..,"minRating":..,"flag":..,"label":..}`).
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_images_json(
    h: *mut PeCatalog,
    query_json: *const c_char,
) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    let q: Query = match str_in(query_json).map(serde_json::from_str).transpose() {
        Ok(q) => q.unwrap_or_default(),
        Err(e) => return error_out(e),
    };
    match c.inner.lock().unwrap().images(&q) {
        Ok(v) => json_out(&v),
        Err(e) => error_out(e),
    }
}

/// Progress callback: (context, done, total).
pub type PeProgressFn = extern "C" fn(*mut c_void, u64, u64);

/// Import. `sources_json` is a JSON array of paths; `options_json` an ImportOptions object
/// (e.g. `{"mode":{"mode":"copy","dest":"/x","template":"%Y/%Y-%m-%d"},"skipDuplicates":true}`).
/// Returns the import report as JSON.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_import_json(
    h: *mut PeCatalog,
    sources_json: *const c_char,
    options_json: *const c_char,
    progress: Option<PeProgressFn>,
    ctx: *mut c_void,
) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    let sources: Vec<PathBuf> = match str_in(sources_json).map(serde_json::from_str) {
        Some(Ok(v)) => v,
        Some(Err(e)) => return error_out(e),
        None => return error_out("missing sources"),
    };
    let opts: ImportOptions = match str_in(options_json).map(serde_json::from_str).transpose() {
        Ok(o) => o.unwrap_or_default(),
        Err(e) => return error_out(e),
    };
    let ctx_addr = ctx as usize;
    let mut cb = |p: &pe_catalog::ImportProgress| {
        if let Some(f) = progress {
            f(ctx_addr as *mut c_void, p.done as u64, p.total as u64);
        }
    };
    match c.inner.lock().unwrap().import(&sources, &opts, &mut cb) {
        Ok(r) => json_out(&r),
        Err(e) => error_out(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn pe_catalog_set_rating(
    h: *mut PeCatalog,
    ids: *const i64,
    count: usize,
    rating: u8,
) -> bool {
    let (Some(c), false) = (catalog(h), ids.is_null()) else {
        return false;
    };
    c.inner
        .lock()
        .unwrap()
        .set_rating(std::slice::from_raw_parts(ids, count), rating)
        .is_ok()
}

/// `flag`: -1 rejected, 0 none, 1 picked.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_set_flag(
    h: *mut PeCatalog,
    ids: *const i64,
    count: usize,
    flag: i8,
) -> bool {
    let (Some(c), false) = (catalog(h), ids.is_null()) else {
        return false;
    };
    let f = match flag {
        -1 => Flag::Rejected,
        1 => Flag::Picked,
        _ => Flag::None,
    };
    c.inner
        .lock()
        .unwrap()
        .set_flag(std::slice::from_raw_parts(ids, count), f)
        .is_ok()
}

/// `label`: one of red/yellow/green/blue/purple, or NULL to clear.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_set_label(
    h: *mut PeCatalog,
    ids: *const i64,
    count: usize,
    label: *const c_char,
) -> bool {
    let (Some(c), false) = (catalog(h), ids.is_null()) else {
        return false;
    };
    c.inner
        .lock()
        .unwrap()
        .set_label(std::slice::from_raw_parts(ids, count), str_in(label))
        .is_ok()
}

/// Grid thumbnail as JPEG bytes (generated on demand). NULL if unavailable.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_thumbnail(
    h: *mut PeCatalog,
    id: i64,
    out_len: *mut usize,
) -> *mut u8 {
    let Some(c) = catalog(h) else {
        return ptr::null_mut();
    };
    match c.inner.lock().unwrap().thumbnail(id) {
        Ok(Some(j)) => bytes_out(j, out_len),
        _ => ptr::null_mut(),
    }
}

fn rgba_out(img: image::RgbImage, w: *mut u32, h: *mut u32) -> *mut u8 {
    let (iw, ih) = img.dimensions();
    let mut rgba = Vec::with_capacity((iw * ih * 4) as usize);
    for p in img.pixels() {
        rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
    }
    unsafe {
        if !w.is_null() {
            *w = iw;
        }
        if !h.is_null() {
            *h = ih;
        }
    }
    bytes_out(rgba, ptr::null_mut())
}

/// Quick display image (embedded preview or decoded JPEG), oriented, long edge <= `long_edge`.
/// RGBA8, `w * h * 4` bytes; free with `pe_bytes_free(ptr, w*h*4)`.
#[no_mangle]
pub unsafe extern "C" fn pe_quick_image(
    path: *const c_char,
    orientation: u8,
    long_edge: u32,
    w: *mut u32,
    h: *mut u32,
) -> *mut u8 {
    let Some(p) = str_in(path) else {
        return ptr::null_mut();
    };
    match pe_preview::quick_image(Path::new(p), orientation, long_edge) {
        Ok(img) => rgba_out(pe_preview::fit(&img, long_edge), w, h),
        Err(_) => ptr::null_mut(),
    }
}

/// Full-quality render with the pipeline (`settings_json` may be NULL for defaults), downscaled so the
/// long edge is <= `long_edge`. RGBA8 sRGB; free with `pe_bytes_free(ptr, w*h*4)`.
#[no_mangle]
pub unsafe extern "C" fn pe_render(
    path: *const c_char,
    settings_json: *const c_char,
    long_edge: u32,
    w: *mut u32,
    h: *mut u32,
) -> *mut u8 {
    let Some(p) = str_in(path) else {
        return ptr::null_mut();
    };
    let settings: pe_pipeline::Settings =
        match str_in(settings_json).map(serde_json::from_str).transpose() {
            Ok(s) => s.unwrap_or_default(),
            Err(_) => return ptr::null_mut(),
        };
    let Ok(frame) = pe_raw::decode_file(p) else {
        return ptr::null_mut();
    };
    let Ok(out) = pe_pipeline::render(&frame, &settings) else {
        return ptr::null_mut();
    };
    let data = out.data.iter().map(|v| (v * 255.0 + 0.5) as u8).collect();
    let Some(img) = image::RgbImage::from_raw(out.width as u32, out.height as u32, data) else {
        return ptr::null_mut();
    };
    rgba_out(pe_preview::fit(&img, long_edge), w, h)
}

// ---------------------------------------------------------------------------------------------
// Develop

/// Slider schema for building UIs: `{"params":[...],"temperature":{...},"tint":{...},"groups":[{"id","label"}]}`.
#[no_mangle]
pub extern "C" fn pe_develop_schema_json() -> *mut c_char {
    let groups: Vec<_> = pe_develop::Group::ALL
        .iter()
        .map(|g| serde_json::json!({ "id": g, "label": g.label(), "look": pe_develop::Group::LOOK.contains(g) }))
        .collect();
    json_out(&serde_json::json!({
        "params": pe_develop::PARAMS,
        "temperature": pe_develop::TEMPERATURE,
        "tint": pe_develop::TINT,
        "groups": groups,
    }))
}

/// Develop state (settings, history, snapshots) of one image as JSON.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_develop_state_json(h: *mut PeCatalog, id: i64) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    match c.inner.lock().unwrap().develop_state(id) {
        Ok(s) => json_out(&s),
        Err(e) => error_out(e),
    }
}

/// Run a develop command (JSON, see `DevelopCommand`) on `count` images. Returns their new states.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_develop_json(
    h: *mut PeCatalog,
    ids: *const i64,
    count: usize,
    command_json: *const c_char,
) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    if ids.is_null() {
        return error_out("no images");
    }
    let cmd: pe_catalog::DevelopCommand = match str_in(command_json).map(serde_json::from_str) {
        Some(Ok(c)) => c,
        Some(Err(e)) => return error_out(e),
        None => return error_out("missing command"),
    };
    match c
        .inner
        .lock()
        .unwrap()
        .develop_execute(std::slice::from_raw_parts(ids, count), &cmd)
    {
        Ok(s) => json_out(&s),
        Err(e) => error_out(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn pe_catalog_presets_json(h: *mut PeCatalog) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    match c.inner.lock().unwrap().presets() {
        Ok(p) => json_out(&p),
        Err(e) => error_out(e),
    }
}

/// Save (or replace by name) a preset. `groups_json`: JSON array of group ids.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_save_preset_json(
    h: *mut PeCatalog,
    name: *const c_char,
    groups_json: *const c_char,
    settings_json: *const c_char,
) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    let groups: Vec<pe_develop::Group> = match str_in(groups_json).map(serde_json::from_str) {
        Some(Ok(g)) => g,
        _ => return error_out("bad groups"),
    };
    let settings: pe_pipeline::Settings = match str_in(settings_json).map(serde_json::from_str) {
        Some(Ok(s)) => s,
        _ => return error_out("bad settings"),
    };
    match c
        .inner
        .lock()
        .unwrap()
        .save_preset(str_in(name).unwrap_or(""), &groups, &settings)
    {
        Ok(p) => json_out(&p),
        Err(e) => error_out(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn pe_catalog_delete_preset(h: *mut PeCatalog, id: i64) -> bool {
    let Some(c) = catalog(h) else { return false };
    c.inner.lock().unwrap().delete_preset(id).is_ok()
}

// ---------------------------------------------------------------------------------------------
// Interactive preview image

/// A raw file decoded once and reduced for fast re-rendering while editing.
pub struct PeImage {
    preview: pe_raw::RawFrame,
    as_shot: Option<(f32, f32)>,
}

/// Decode `path` and prepare a preview whose long edge is at least `long_edge` (where the file allows).
/// Export images. `settings_json` is an ExportSettings object (missing fields take defaults).
/// The catalog is only locked while gathering the items, so other calls proceed during rendering.
/// Returns the export report as JSON.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_export_json(
    h: *mut PeCatalog,
    ids: *const i64,
    count: usize,
    settings_json: *const c_char,
    progress: Option<PeProgressFn>,
    ctx: *mut c_void,
) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    if ids.is_null() && count > 0 {
        return error_out("null ids");
    }
    let ids = if count == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(ids, count)
    };
    let settings: pe_export::ExportSettings = match str_in(settings_json).map(serde_json::from_str)
    {
        Some(Ok(s)) => s,
        Some(Err(e)) => return error_out(e),
        None => return error_out("missing settings"),
    };
    let items = match c.inner.lock().unwrap().export_items(ids) {
        Ok(i) => i,
        Err(e) => return error_out(e),
    };
    let ctx_addr = ctx as usize;
    let mut cb = |p: &pe_export::ExportProgress| {
        if let Some(f) = progress {
            f(ctx_addr as *mut c_void, p.done as u64, p.total as u64);
        }
    };
    json_out(&pe_export::export(&items, &settings, &mut cb))
}

/// Default export settings as JSON.
#[no_mangle]
pub extern "C" fn pe_export_defaults_json() -> *mut c_char {
    json_out(&pe_export::ExportSettings::default())
}

#[no_mangle]
pub unsafe extern "C" fn pe_catalog_export_presets_json(h: *mut PeCatalog) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    match c.inner.lock().unwrap().export_presets() {
        Ok(p) => json_out(&p),
        Err(e) => error_out(e),
    }
}

/// Save (or replace by name) an export preset.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_save_export_preset_json(
    h: *mut PeCatalog,
    name: *const c_char,
    settings_json: *const c_char,
) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    let settings: pe_export::ExportSettings = match str_in(settings_json).map(serde_json::from_str)
    {
        Some(Ok(s)) => s,
        _ => return error_out("bad settings"),
    };
    match c
        .inner
        .lock()
        .unwrap()
        .save_export_preset(str_in(name).unwrap_or(""), &settings)
    {
        Ok(p) => json_out(&p),
        Err(e) => error_out(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn pe_catalog_delete_export_preset(h: *mut PeCatalog, id: i64) -> bool {
    catalog(h).is_some_and(|c| c.inner.lock().unwrap().delete_export_preset(id).is_ok())
}

#[no_mangle]
pub unsafe extern "C" fn pe_image_open(path: *const c_char, long_edge: u32) -> *mut PeImage {
    let Some(p) = str_in(path) else {
        return ptr::null_mut();
    };
    let Ok(frame) = pe_raw::decode_file(p) else {
        return ptr::null_mut();
    };
    let as_shot = pe_pipeline::wb::temperature_of(&frame.xyz_to_cam, frame.wb);
    let preview = pe_pipeline::prepare_preview(&frame, long_edge as usize);
    Box::into_raw(Box::new(PeImage { preview, as_shot }))
}

#[no_mangle]
pub unsafe extern "C" fn pe_image_close(h: *mut PeImage) {
    if !h.is_null() {
        drop(Box::from_raw(h));
    }
}

/// `{"width","height","asShotTemperature","asShotTint"}` of the prepared preview.
#[no_mangle]
pub unsafe extern "C" fn pe_image_info_json(h: *mut PeImage) -> *mut c_char {
    let Some(img) = h.as_ref() else {
        return error_out("null image");
    };
    json_out(&serde_json::json!({
        "width": img.preview.width,
        "height": img.preview.height,
        "asShotTemperature": img.as_shot.map(|t| t.0),
        "asShotTint": img.as_shot.map(|t| t.1),
    }))
}

/// Render the prepared preview with `settings_json` (NULL = defaults). RGBA8; free with
/// `pe_bytes_free(ptr, w*h*4)`.
#[no_mangle]
pub unsafe extern "C" fn pe_image_render(
    h: *mut PeImage,
    settings_json: *const c_char,
    w: *mut u32,
    hh: *mut u32,
) -> *mut u8 {
    let Some(img) = h.as_ref() else {
        return ptr::null_mut();
    };
    let settings: pe_pipeline::Settings =
        match str_in(settings_json).map(serde_json::from_str).transpose() {
            Ok(s) => s.unwrap_or_default(),
            Err(_) => return ptr::null_mut(),
        };
    let Ok(out) = pe_pipeline::render(&img.preview, &settings) else {
        return ptr::null_mut();
    };
    let data = out.data.iter().map(|v| (v * 255.0 + 0.5) as u8).collect();
    let Some(rgb) = image::RgbImage::from_raw(out.width as u32, out.height as u32, data) else {
        return ptr::null_mut();
    };
    rgba_out(rgb, w, hh)
}

#[no_mangle]
pub unsafe extern "C" fn pe_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

#[no_mangle]
pub unsafe extern "C" fn pe_bytes_free(p: *mut u8, len: usize) {
    if !p.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, len)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn take(s: *mut c_char) -> String {
        let out = CStr::from_ptr(s).to_str().unwrap().to_owned();
        pe_string_free(s);
        out
    }

    #[test]
    fn open_import_query_roundtrip() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("p");
        std::fs::create_dir_all(&src).unwrap();
        image::RgbImage::from_pixel(32, 24, image::Rgb([200, 100, 50]))
            .save(src.join("a.jpg"))
            .unwrap();
        unsafe {
            let cat_path = CString::new(t.path().join("x.pecat").to_str().unwrap()).unwrap();
            let h = pe_catalog_open(cat_path.as_ptr(), true, ptr::null_mut());
            assert!(!h.is_null());
            let sources = CString::new(serde_json::to_string(&[&src]).unwrap()).unwrap();
            let report = take(pe_catalog_import_json(
                h,
                sources.as_ptr(),
                ptr::null(),
                None,
                ptr::null_mut(),
            ));
            assert!(report.contains("\"imported\":1"), "{report}");
            let imgs: serde_json::Value =
                serde_json::from_str(&take(pe_catalog_images_json(h, ptr::null()))).unwrap();
            let id = imgs[0]["id"].as_i64().unwrap();
            assert!(pe_catalog_set_rating(h, &id, 1, 3));
            let q = CString::new(r#"{"minRating":3}"#).unwrap();
            let imgs: serde_json::Value =
                serde_json::from_str(&take(pe_catalog_images_json(h, q.as_ptr()))).unwrap();
            assert_eq!(imgs.as_array().unwrap().len(), 1);
            let mut len = 0;
            let jpeg = pe_catalog_thumbnail(h, id, &mut len);
            assert!(!jpeg.is_null() && len > 100);
            pe_bytes_free(jpeg, len);

            // Develop: set, then undo, via JSON commands.
            let cmd = CString::new(r#"{"command":"set","settings":{"exposure":0.7}}"#).unwrap();
            let states: serde_json::Value =
                serde_json::from_str(&take(pe_catalog_develop_json(h, &id, 1, cmd.as_ptr())))
                    .unwrap();
            assert_eq!(states[0]["settings"]["exposure"], 0.7);
            assert_eq!(states[0]["canUndo"], true);
            let undo = CString::new(r#"{"command":"undo"}"#).unwrap();
            let states: serde_json::Value =
                serde_json::from_str(&take(pe_catalog_develop_json(h, &id, 1, undo.as_ptr())))
                    .unwrap();
            assert_eq!(states[0]["edited"], false);
            let bad = CString::new(r#"{"command":"nope"}"#).unwrap();
            assert!(take(pe_catalog_develop_json(h, &id, 1, bad.as_ptr())).contains("error"));

            // Presets and schema.
            let name = CString::new("Bright").unwrap();
            let groups = CString::new(r#"["exposure"]"#).unwrap();
            let settings = CString::new(r#"{"exposure":1.0}"#).unwrap();
            let p = take(pe_catalog_save_preset_json(
                h,
                name.as_ptr(),
                groups.as_ptr(),
                settings.as_ptr(),
            ));
            assert!(p.contains("\"name\":\"Bright\""), "{p}");
            assert!(take(pe_catalog_presets_json(h)).contains("Bright"));
            let schema = take(pe_develop_schema_json());
            assert!(schema.contains("\"key\":\"exposure\"") && schema.contains("whiteBalance"));
            pe_catalog_close(h);
        }
    }

    #[test]
    fn export_and_export_presets() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("p");
        std::fs::create_dir_all(&src).unwrap();
        image::RgbImage::from_pixel(64, 48, image::Rgb([200, 120, 40]))
            .save(src.join("a.jpg"))
            .unwrap();
        unsafe {
            let path = CString::new(t.path().join("lib.pecat").to_str().unwrap()).unwrap();
            let h = pe_catalog_open(path.as_ptr(), true, ptr::null_mut());
            let sources = CString::new(serde_json::to_string(&[&src]).unwrap()).unwrap();
            take(pe_catalog_import_json(
                h,
                sources.as_ptr(),
                ptr::null(),
                None,
                ptr::null_mut(),
            ));
            let ids: Vec<serde_json::Value> =
                serde_json::from_str(&take(pe_catalog_images_json(h, ptr::null()))).unwrap();
            let id = ids[0]["id"].as_i64().unwrap();
            let settings = CString::new(
                serde_json::json!({
                    "destination": t.path().join("out"),
                    "format": {"type": "png", "sixteenBit": false},
                    "resize": {"mode": "longEdge", "pixels": 32}
                })
                .to_string(),
            )
            .unwrap();
            let r: serde_json::Value = serde_json::from_str(&take(pe_catalog_export_json(
                h,
                &id,
                1,
                settings.as_ptr(),
                None,
                ptr::null_mut(),
            )))
            .unwrap();
            let written = r["written"][0].as_str().unwrap();
            assert!(written.ends_with("a.png"), "{r}");
            assert_eq!(image::open(written).unwrap().width(), 32);
            let name = CString::new("Small PNG").unwrap();
            let p: serde_json::Value = serde_json::from_str(&take(
                pe_catalog_save_export_preset_json(h, name.as_ptr(), settings.as_ptr()),
            ))
            .unwrap();
            assert_eq!(p["settings"]["resize"]["pixels"], 32);
            assert!(take(pe_catalog_export_presets_json(h)).contains("Small PNG"));
            assert!(pe_catalog_delete_export_preset(
                h,
                p["id"].as_i64().unwrap()
            ));
            assert!(take(pe_export_defaults_json()).contains("\"fileNaming\":\"{name}\""));
            pe_catalog_close(h);
        }
    }

    #[test]
    fn preview_image_renders_faster_and_reports_white_balance() {
        let raw = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/raw/Nikon-D750.nef");
        if !raw.exists() {
            return;
        }
        unsafe {
            let p = CString::new(raw.to_str().unwrap()).unwrap();
            let img = pe_image_open(p.as_ptr(), 1024);
            assert!(!img.is_null());
            let info: serde_json::Value =
                serde_json::from_str(&take(pe_image_info_json(img))).unwrap();
            assert!(info["asShotTemperature"].as_f64().unwrap() > 2000.0);
            let (mut w, mut hh) = (0u32, 0u32);
            let px = pe_image_render(img, ptr::null(), &mut w, &mut hh);
            assert!(!px.is_null() && w >= 1024 && hh > 0);
            pe_bytes_free(px, (w * hh * 4) as usize);
            pe_image_close(img);
        }
    }
}
