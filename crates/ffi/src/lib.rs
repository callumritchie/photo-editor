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
            pe_catalog_close(h);
        }
    }
}
