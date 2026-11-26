//! C ABI for the macOS app. See `include/pe_ffi.h` for the contract.
//!
//! Conventions:
//! - Strings in and out are NUL-terminated UTF-8. Returned strings are owned by the caller and must be
//!   released with [`pe_string_free`]; returned byte buffers with [`pe_bytes_free`].
//! - Structured data crosses as JSON. Failures return `{"error": "..."}` (JSON functions) or NULL/false.
//! - A catalog handle may be used from any thread; calls are serialised internally.

#![allow(clippy::missing_safety_doc)]

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use pe_catalog::{Catalog, Flag, ImportOptions, Query, RenderStores};
use serde::Serialize;

pub struct PeCatalog {
    inner: Arc<Mutex<Catalog>>,
    renders: Arc<RenderQueue>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for PeCatalog {
    fn drop(&mut self) {
        self.renders.stop.store(true, Ordering::SeqCst);
        self.renders.wake.notify_all();
        if let Some(w) = self.worker.take() {
            let _ = w.join(); // waits for at most the render in progress
        }
    }
}

/// Called on the render worker's thread after a photo's previews were rendered:
/// `(ctx, image_id, thumbnail_changed)`.
pub type PeRenderedFn = extern "C" fn(*mut c_void, i64, bool);

/// Pending background renders (loupe previews and edited thumbnails), most urgent first.
#[derive(Default)]
struct RenderQueue {
    queue: Mutex<VecDeque<i64>>,
    wake: Condvar,
    stop: AtomicBool,
    callback: Mutex<Option<(PeRenderedFn, usize)>>,
}

impl RenderQueue {
    fn request(&self, ids: &[i64], urgent: bool) {
        let mut q = self.queue.lock().unwrap();
        q.retain(|id| !ids.contains(id));
        if urgent {
            for &id in ids.iter().rev() {
                q.push_front(id);
            }
        } else {
            q.extend(ids);
        }
        drop(q);
        self.wake.notify_one();
    }
}

fn render_worker(cat: Arc<Mutex<Catalog>>, q: Arc<RenderQueue>, dir: PathBuf) {
    let Ok(mut stores) = RenderStores::open(&dir) else {
        return;
    };
    // Photos that failed to render, by the digest that failed, so they are not retried in a loop.
    let mut failed: HashMap<i64, String> = HashMap::new();
    loop {
        let id = {
            let mut g = q.queue.lock().unwrap();
            loop {
                if q.stop.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(id) = g.pop_front() {
                    break id;
                }
                g = q.wake.wait(g).unwrap();
            }
        };
        let job = match cat.lock().unwrap().render_job(id) {
            Ok(Some(j)) => j,
            _ => continue,
        };
        if failed.get(&id) == Some(&job.digest) || stores.is_fresh(&job).unwrap_or(false) {
            continue;
        }
        match stores.run(&job, gpu()) {
            Ok(thumb_changed) => {
                if let Some((f, ctx)) = *q.callback.lock().unwrap() {
                    f(ctx as *mut c_void, id, thumb_changed);
                }
            }
            Err(_) => {
                failed.insert(id, job.digest);
            }
        }
    }
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
        Ok(c) => {
            let dir = c.dir().to_path_buf();
            let inner = Arc::new(Mutex::new(c));
            let renders = Arc::new(RenderQueue::default());
            let worker = std::thread::Builder::new()
                .name("pe-render".into())
                .spawn({
                    let (cat, q) = (inner.clone(), renders.clone());
                    move || render_worker(cat, q, dir)
                })
                .ok();
            Box::into_raw(Box::new(PeCatalog {
                inner,
                renders,
                worker,
            }))
        }
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
    // Never renders here: stale or placeholder thumbnails come back at once and a background render is
    // queued; the render callback reports when the real one is ready.
    let quick = c.inner.lock().unwrap().thumbnail_quick(id);
    match quick {
        Ok((jpeg, needs_render)) => {
            if needs_render {
                c.renders.request(&[id], true);
            }
            jpeg.map_or(ptr::null_mut(), |j| bytes_out(j, out_len))
        }
        Err(_) => ptr::null_mut(),
    }
}

/// Cached loupe preview JPEG (free with `pe_bytes_free`), or NULL. `out_fresh` reports whether it
/// shows the current settings; when missing or stale and `queue_if_stale`, a background render is
/// queued (pass false for a photo that is about to be rendered live anyway).
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_preview(
    h: *mut PeCatalog,
    id: i64,
    queue_if_stale: bool,
    out_len: *mut usize,
    out_fresh: *mut bool,
) -> *mut u8 {
    let Some(c) = catalog(h) else {
        return ptr::null_mut();
    };
    let cached = c.inner.lock().unwrap().preview(id).ok().flatten();
    let fresh = cached.as_ref().is_some_and(|p| p.1);
    if !fresh && queue_if_stale {
        c.renders.request(&[id], true);
    }
    if !out_fresh.is_null() {
        *out_fresh = fresh;
    }
    cached.map_or(ptr::null_mut(), |(j, _)| bytes_out(j, out_len))
}

/// Queue background renders of previews (and edited thumbnails). `urgent` puts them first, e.g. the
/// photos next to the one being viewed; otherwise they go to the back (e.g. "build previews").
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_request_renders(
    h: *mut PeCatalog,
    ids: *const i64,
    count: usize,
    urgent: bool,
) {
    let Some(c) = catalog(h) else { return };
    if ids.is_null() || count == 0 {
        return;
    }
    c.renders
        .request(std::slice::from_raw_parts(ids, count), urgent);
}

/// Number of background renders waiting.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_pending_renders(h: *mut PeCatalog) -> usize {
    catalog(h).map_or(0, |c| c.renders.queue.lock().unwrap().len())
}

/// Register (or clear, with NULL) the callback run after each background render. It runs on a worker
/// thread; `ctx` must stay valid until it is cleared or the catalog is closed.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_set_render_callback(
    h: *mut PeCatalog,
    callback: Option<PeRenderedFn>,
    ctx: *mut c_void,
) {
    if let Some(c) = catalog(h) {
        *c.renders.callback.lock().unwrap() = callback.map(|f| (f, ctx as usize));
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
        .map(|g| serde_json::json!({ "id": g, "label": g.label(), "look": g.is_look() }))
        .collect();
    json_out(&serde_json::json!({
        "params": pe_develop::params(),
        "maskParams": pe_develop::mask_params(),
        "choices": pe_develop::CHOICES,
        "temperature": pe_develop::temperature_spec(),
        "tint": pe_develop::tint_spec(),
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
/// A region prepared for 1:1 rendering: its sensor rectangle, demosaiced pixels and GPU upload.
struct RegionCache {
    rect: [usize; 4],
    demosaic: pe_pipeline::Demosaic,
    rgb: pe_raw::RawFrame,
    gpu: Option<pe_gpu::GpuFrame>,
}

pub struct PeImage {
    /// The whole decoded frame, for 1:1 regions.
    full: pe_raw::RawFrame,
    region: Mutex<Option<RegionCache>>,
    preview: pe_raw::RawFrame,
    as_shot: Option<(f32, f32)>,
    gpu: Option<pe_gpu::GpuFrame>,
    /// Mask rasters available to this photo's masks (subject / sky selections).
    rasters: Mutex<pe_pipeline::masks::Rasters>,
}

/// The process-wide GPU, initialised on first use (`None` when unavailable or `PE_NO_GPU` is set).
fn gpu() -> Option<&'static pe_gpu::Gpu> {
    static GPU: std::sync::OnceLock<Option<pe_gpu::Gpu>> = std::sync::OnceLock::new();
    GPU.get_or_init(pe_gpu::Gpu::new).as_ref()
}

/// Name of the renderer used for interactive previews, e.g. "Apple M1 (Metal)" or "CPU".
#[no_mangle]
pub extern "C" fn pe_renderer_name() -> *mut c_char {
    let name = gpu()
        .map(|g| g.name().to_string())
        .unwrap_or_else(|| "CPU".into());
    CString::new(name)
        .map(CString::into_raw)
        .unwrap_or(ptr::null_mut())
}

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

/// Run a library command (collections, keywords, virtual copies, stacks); see
/// `pe_catalog::LibraryCommand`. Returns its JSON result or `{"error": ...}`.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_library_json(
    h: *mut PeCatalog,
    command_json: *const c_char,
) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    let cmd: pe_catalog::LibraryCommand = match str_in(command_json).map(serde_json::from_str) {
        Some(Ok(v)) => v,
        Some(Err(e)) => return error_out(e),
        None => return error_out("missing command"),
    };
    match c.inner.lock().unwrap().library_execute(&cmd) {
        Ok(v) => json_out(&v),
        Err(e) => error_out(e),
    }
}

/// Store an 8-bit grey mask raster (`w` x `h`, fractions of the oriented frame) for a photo.
/// Returns `{"id": "<uuid>"}` or `{"error": ...}`.
#[no_mangle]
pub unsafe extern "C" fn pe_catalog_save_raster(
    h: *mut PeCatalog,
    image_id: i64,
    w: u32,
    hh: u32,
    data: *const u8,
    len: usize,
) -> *mut c_char {
    let Some(c) = catalog(h) else {
        return error_out("null catalog");
    };
    if data.is_null() {
        return error_out("no data");
    }
    let bytes = std::slice::from_raw_parts(data, len);
    match c.inner.lock().unwrap().save_raster(image_id, w, hh, bytes) {
        Ok(id) => json_out(&serde_json::json!({ "id": id })),
        Err(e) => error_out(e),
    }
}

/// Give the live preview the rasters that photo `image_id`'s masks refer to.
#[no_mangle]
pub unsafe extern "C" fn pe_image_attach_rasters(
    img: *mut PeImage,
    h: *mut PeCatalog,
    image_id: i64,
) -> bool {
    let (Some(img), Some(c)) = (img.as_ref(), catalog(h)) else {
        return false;
    };
    match c.inner.lock().unwrap().render_settings(image_id) {
        Ok(s) => {
            *img.rasters.lock().unwrap() = s.rasters;
            true
        }
        Err(_) => false,
    }
}

/// Add one raster (8-bit grey) to the live preview under `id` (after a new selection).
#[no_mangle]
pub unsafe extern "C" fn pe_image_add_raster(
    img: *mut PeImage,
    id: *const c_char,
    w: u32,
    hh: u32,
    data: *const u8,
    len: usize,
) -> bool {
    let (Some(img), Some(id)) = (img.as_ref(), str_in(id)) else {
        return false;
    };
    if data.is_null() || len != (w as usize) * (hh as usize) {
        return false;
    }
    let bytes = std::slice::from_raw_parts(data, len);
    let raster = pe_pipeline::masks::Raster {
        width: w as usize,
        height: hh as usize,
        data: bytes.iter().map(|v| *v as f32 / 255.0).collect(),
    };
    let mut r = img.rasters.lock().unwrap();
    let mut map = (**r).clone();
    map.insert(id.to_string(), raster);
    *r = std::sync::Arc::new(map);
    true
}

/// One mask's values over the preview frame (oriented, uncropped), as 8-bit grey for an overlay.
/// Free with `pe_bytes_free(p, w*h)`. NULL if the mask does not exist or is disabled.
#[no_mangle]
pub unsafe extern "C" fn pe_image_mask_overlay(
    img: *mut PeImage,
    settings_json: *const c_char,
    index: u32,
    w: *mut u32,
    hh: *mut u32,
) -> *mut u8 {
    let Some(img) = img.as_ref() else {
        return ptr::null_mut();
    };
    let Some(Ok(mut s)) = str_in(settings_json).map(serde_json::from_str::<pe_pipeline::Settings>)
    else {
        return ptr::null_mut();
    };
    s.rasters = img.rasters.lock().unwrap().clone();
    match pe_pipeline::mask_overlay(&img.preview, &s, index as usize) {
        Ok(Some((ow, oh, v))) => {
            if !w.is_null() {
                *w = ow as u32;
            }
            if !hh.is_null() {
                *hh = oh as u32;
            }
            bytes_out(
                v.iter().map(|x| (x * 255.0 + 0.5) as u8).collect(),
                ptr::null_mut(),
            )
        }
        _ => ptr::null_mut(),
    }
}

/// OKLab colour (as colour-range masks see it) at frame fractions (fx, fy) into `out[3]`.
#[no_mangle]
pub unsafe extern "C" fn pe_image_pick_color(
    img: *mut PeImage,
    settings_json: *const c_char,
    fx: f32,
    fy: f32,
    out: *mut f32,
) -> bool {
    let Some(img) = img.as_ref() else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    let s: pe_pipeline::Settings = match str_in(settings_json).map(serde_json::from_str).transpose()
    {
        Ok(s) => s.unwrap_or_default(),
        Err(_) => return false,
    };
    match pe_pipeline::pick_color(&img.preview, &s, fx, fy) {
        Ok(lab) => {
            std::slice::from_raw_parts_mut(out, 3).copy_from_slice(&lab);
            true
        }
        Err(_) => false,
    }
}

/// Size of the full-resolution output (after crop and geometry) for `settings_json`.
#[no_mangle]
pub unsafe extern "C" fn pe_image_output_size(
    img: *mut PeImage,
    settings_json: *const c_char,
    w: *mut u32,
    hh: *mut u32,
) -> bool {
    let Some(img) = img.as_ref() else {
        return false;
    };
    let s: pe_pipeline::Settings = match str_in(settings_json)
        .map(serde_json::from_str::<pe_pipeline::Settings>)
        .transpose()
    {
        Ok(s) => s.unwrap_or_default().clamped(),
        Err(_) => return false,
    };
    let g = pe_pipeline::geometry::GeometryParams::new(
        &s,
        img.full.width,
        img.full.height,
        img.full.orientation,
    );
    let (ow, oh) = g.output_size();
    if !w.is_null() {
        *w = ow as u32;
    }
    if !hh.is_null() {
        *hh = oh as u32;
    }
    true
}

/// Render the full-resolution output rectangle (x, y, w, h) at 1:1 as RGBA8 (free with
/// `pe_bytes_free(p, w*h*4)`). The region (plus a margin) is demosaiced and uploaded once and reused
/// while it still covers the rectangle; whole-picture measurements come from the preview.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn pe_image_render_region(
    img: *mut PeImage,
    settings_json: *const c_char,
    x: u32,
    y: u32,
    rw: u32,
    rh: u32,
    w: *mut u32,
    hh: *mut u32,
) -> *mut u8 {
    let Some(img) = img.as_ref() else {
        return ptr::null_mut();
    };
    let mut s: pe_pipeline::Settings =
        match str_in(settings_json).map(serde_json::from_str).transpose() {
            Ok(s) => s.unwrap_or_default(),
            Err(_) => return ptr::null_mut(),
        };
    s.rasters = img.rasters.lock().unwrap().clone();
    let frame = &img.full;
    let out = [
        x as usize,
        y as usize,
        rw.max(1) as usize,
        rh.max(1) as usize,
    ];
    let (rect, region) = pe_pipeline::region::plan_region(frame, &s, out);
    let demosaic = s.demosaic;
    let mut cache = img.region.lock().unwrap();
    // Reuse the prepared region when it covers the new one (same demosaic method).
    let covers = cache.as_ref().is_some_and(|c| {
        c.demosaic == demosaic
            && c.rect[0] <= rect[0]
            && c.rect[1] <= rect[1]
            && c.rect[0] + c.rect[2] >= rect[0] + rect[2]
            && c.rect[1] + c.rect[3] >= rect[1] + rect[3]
    });
    if !covers {
        // Prepare a little more than needed, so small pans reuse it.
        let grow = 256usize;
        let a = pe_pipeline::region::alignment(
            frame.width,
            frame.height,
            match &frame.layout {
                pe_raw::Layout::Mosaic(c) => c.width.max(c.height),
                _ => 1,
            },
        );
        let gx0 = rect[0].saturating_sub(grow) / a * a;
        let gy0 = rect[1].saturating_sub(grow) / a * a;
        let gx1 = (rect[0] + rect[2] + grow).min(frame.width);
        let gy1 = (rect[1] + rect[3] + grow).min(frame.height);
        let big = [gx0, gy0, gx1 - gx0, gy1 - gy0];
        let crop = pe_pipeline::region::crop_frame(frame, big[0], big[1], big[2], big[3]);
        let rgb = match (gpu(), demosaic) {
            (Some(g), pe_pipeline::Demosaic::Auto) => g.demosaic(&crop).ok(),
            _ => None,
        }
        .unwrap_or_else(|| pe_raw::RawFrame {
            layout: pe_raw::Layout::LinearRgb,
            data: pe_pipeline::camera_rgb(&crop, &s),
            ..crop.clone_header()
        });
        let ctx = pe_pipeline::region::Ctx {
            full_w: frame.width,
            full_h: frame.height,
            x0: big[0],
            y0: big[1],
        };
        let up = gpu().and_then(|g| g.upload_region(&rgb, ctx).ok());
        *cache = Some(RegionCache {
            rect: big,
            demosaic,
            rgb,
            gpu: up,
        });
    }
    let c = cache.as_ref().expect("prepared");
    // Whole-picture measurements that do not depend on scale, from the preview.
    let auto_wb = pe_pipeline::wb::multipliers(
        &img.preview,
        &pe_pipeline::WhiteBalance::Auto,
        &img.preview.data,
    );
    let airlight = (s.dehaze != 0.0)
        .then(|| {
            let sc = pe_pipeline::render_scene(&img.preview, &s.clone().clamped()).ok()?;
            Some(pe_pipeline::local::airlight(
                &pe_pipeline::local::airlight_samples(&sc.data, sc.width, sc.height),
            ))
        })
        .flatten();
    let globals = pe_pipeline::region::Globals {
        auto_wb: Some(auto_wb),
        airlight,
        noise: None,
    };
    let region = pe_pipeline::region::Region {
        x0: c.rect[0],
        y0: c.rect[1],
        ..region
    };
    let rgba = match (gpu(), &c.gpu) {
        (Some(g), Some(f)) => g
            .render_region(f, &s, out, &globals)
            .ok()
            .map(|(_, _, p)| p),
        _ => None,
    }
    .or_else(|| {
        let out_img = pe_pipeline::render_region(&c.rgb, &s, &region, &globals).ok()?;
        Some(
            out_img
                .data
                .chunks(3)
                .flat_map(|p| {
                    [
                        (p[0] * 255.0 + 0.5) as u8,
                        (p[1] * 255.0 + 0.5) as u8,
                        (p[2] * 255.0 + 0.5) as u8,
                        255,
                    ]
                })
                .collect(),
        )
    });
    let Some(rgba) = rgba else {
        return ptr::null_mut();
    };
    if !w.is_null() {
        *w = out[2] as u32;
    }
    if !hh.is_null() {
        *hh = out[3] as u32;
    }
    bytes_out(rgba, ptr::null_mut())
}

/// Convert `n` points (x, y pairs, fractions) in place between output positions (after crop and
/// geometry) and mask positions (fractions of the oriented, uncropped frame).
#[no_mangle]
pub unsafe extern "C" fn pe_image_map_points(
    img: *mut PeImage,
    settings_json: *const c_char,
    to_frame: bool,
    pts: *mut f32,
    n: usize,
) -> bool {
    let Some(img) = img.as_ref() else {
        return false;
    };
    if pts.is_null() {
        return false;
    }
    let s: pe_pipeline::Settings = match str_in(settings_json)
        .map(serde_json::from_str::<pe_pipeline::Settings>)
        .transpose()
    {
        Ok(s) => s.unwrap_or_default().clamped(),
        Err(_) => return false,
    };
    let g = pe_pipeline::geometry::GeometryParams::new(
        &s,
        img.preview.width,
        img.preview.height,
        img.preview.orientation,
    );
    for p in std::slice::from_raw_parts_mut(pts, n * 2).chunks_mut(2) {
        let (a, b) = if to_frame {
            g.output_to_frame(p[0], p[1])
        } else {
            g.frame_to_output(p[0], p[1])
        };
        p[0] = a;
        p[1] = b;
    }
    true
}

/// Select Sky on the preview (default rendering, oriented, uncropped). Returns an 8-bit grey raster
/// (free with `pe_bytes_free(p, w*h)`), or NULL.
#[no_mangle]
pub unsafe extern "C" fn pe_image_select_sky(
    img: *mut PeImage,
    w: *mut u32,
    hh: *mut u32,
) -> *mut u8 {
    let Some(img) = img.as_ref() else {
        return ptr::null_mut();
    };
    let Ok(out) = pe_pipeline::render(&img.preview, &pe_pipeline::Settings::default()) else {
        return ptr::null_mut();
    };
    let r = pe_pipeline::select::select_sky(&out.data, out.width, out.height);
    if !w.is_null() {
        *w = r.width as u32;
    }
    if !hh.is_null() {
        *hh = r.height as u32;
    }
    bytes_out(
        r.data.iter().map(|v| (v * 255.0 + 0.5) as u8).collect(),
        ptr::null_mut(),
    )
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

/// Decode `path` and prepare a preview whose long edge is at least `long_edge` (where the file allows).
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
    let gpu = gpu().and_then(|g| g.upload(&preview).ok());
    Box::into_raw(Box::new(PeImage {
        full: frame,
        region: Mutex::new(None),
        preview,
        as_shot,
        gpu,
        rasters: Mutex::new(Default::default()),
    }))
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
        "gpu": img.gpu.is_some(),
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
    let mut settings: pe_pipeline::Settings =
        match str_in(settings_json).map(serde_json::from_str).transpose() {
            Ok(s) => s.unwrap_or_default(),
            Err(_) => return ptr::null_mut(),
        };
    settings.rasters = img.rasters.lock().unwrap().clone();
    if let (Some(g), Some(frame)) = (gpu(), &img.gpu) {
        if let Ok((ow, oh, rgba)) = g.render(frame, &settings) {
            if !w.is_null() {
                *w = ow;
            }
            if !hh.is_null() {
                *hh = oh;
            }
            return bytes_out(rgba, ptr::null_mut());
        }
        // Fall through to the CPU if the GPU fails (e.g. device lost).
    }
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

    static RENDERED: Mutex<Vec<(i64, bool)>> = Mutex::new(Vec::new());
    extern "C" fn on_rendered(_ctx: *mut c_void, id: i64, thumb_changed: bool) {
        RENDERED.lock().unwrap().push((id, thumb_changed));
    }

    #[test]
    fn background_worker_renders_previews() {
        let raw = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/raw/Nikon-D750.nef");
        if !raw.exists() {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("p");
        std::fs::create_dir_all(&src).unwrap();
        for n in ["a.nef", "b.nef", "c.nef"] {
            std::fs::copy(&raw, src.join(n)).unwrap();
        }
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
            let imgs: Vec<serde_json::Value> =
                serde_json::from_str(&take(pe_catalog_images_json(h, ptr::null()))).unwrap();
            let ids: Vec<i64> = imgs.iter().map(|i| i["id"].as_i64().unwrap()).collect();
            pe_catalog_set_render_callback(h, Some(on_rendered), ptr::null_mut());

            // No preview yet: NULL now, rendered in the background.
            let (mut len, mut fresh) = (0usize, true);
            assert!(pe_catalog_preview(h, ids[0], true, &mut len, &mut fresh).is_null());
            assert!(!fresh);
            let t0 = std::time::Instant::now();
            while !RENDERED.lock().unwrap().iter().any(|r| r.0 == ids[0]) {
                assert!(t0.elapsed().as_secs() < 30, "render did not finish");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let p = pe_catalog_preview(h, ids[0], true, &mut len, &mut fresh);
            assert!(!p.is_null() && fresh && len > 10_000);
            pe_bytes_free(p, len);

            // Closing with work still queued stops promptly.
            pe_catalog_request_renders(h, ids[1..].as_ptr(), 2, false);
            assert!(pe_catalog_pending_renders(h) <= 2);
            let t0 = std::time::Instant::now();
            pe_catalog_close(h);
            assert!(t0.elapsed().as_secs() < 10);
        }
    }

    #[test]
    fn library_commands_and_query_fields() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("p");
        std::fs::create_dir_all(&src).unwrap();
        for n in ["a.jpg", "b.jpg"] {
            image::RgbImage::from_pixel(16, 16, image::Rgb([10, 20, 30]))
                .save(src.join(n))
                .unwrap();
        }
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
            let run = |cmd: serde_json::Value| -> serde_json::Value {
                let c = CString::new(cmd.to_string()).unwrap();
                serde_json::from_str(&take(pe_catalog_library_json(h, c.as_ptr()))).unwrap()
            };
            let r = run(
                serde_json::json!({"command": "addKeywords", "imageIds": [1, 2], "paths": ["Trip > Day 1"]}),
            );
            assert_eq!(r["ok"], true);
            let kw = run(serde_json::json!({"command": "keywords"}));
            assert_eq!(kw[1]["path"], "Trip > Day 1");
            let r = run(serde_json::json!({"command": "stack", "imageIds": [2, 1]}));
            assert!(r["id"].as_i64().is_some());
            let bad = run(
                serde_json::json!({"command": "createCollection", "name": " ", "kind": "collection"}),
            );
            assert!(bad["error"].is_string());
            let q = CString::new(r#"{"collapseStacks": true, "text": "day"}"#).unwrap();
            let rows: serde_json::Value =
                serde_json::from_str(&take(pe_catalog_images_json(h, q.as_ptr()))).unwrap();
            assert_eq!(rows.as_array().unwrap().len(), 1);
            assert_eq!(
                (rows[0]["id"].as_i64(), rows[0]["stackSize"].as_i64()),
                (Some(2), Some(2))
            );
            pe_catalog_close(h);
        }
    }

    #[test]
    fn one_to_one_regions() {
        let raw = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/raw/Nikon-D750.nef");
        if !raw.exists() {
            return;
        }
        unsafe {
            let p = CString::new(raw.to_str().unwrap()).unwrap();
            let img = pe_image_open(p.as_ptr(), 1024);
            let s = CString::new(r#"{"exposure": 0.5, "shadows": 40}"#).unwrap();
            let (mut w, mut h) = (0u32, 0u32);
            assert!(pe_image_output_size(img, s.as_ptr(), &mut w, &mut h));
            assert_eq!((w, h), (6016, 4016));
            let t = std::time::Instant::now();
            let (mut rw, mut rh) = (0u32, 0u32);
            let px =
                pe_image_render_region(img, s.as_ptr(), 2000, 1500, 1200, 800, &mut rw, &mut rh);
            let first = t.elapsed();
            assert!(!px.is_null() && (rw, rh) == (1200, 800));
            let region: Vec<u8> = std::slice::from_raw_parts(px, (rw * rh * 4) as usize).to_vec();
            pe_bytes_free(px, (rw * rh * 4) as usize);
            // Same area with other settings, then a small pan: the prepared region is reused.
            let s2 = CString::new(r#"{"exposure": 0.8, "shadows": 40}"#).unwrap();
            let t = std::time::Instant::now();
            let px =
                pe_image_render_region(img, s2.as_ptr(), 2000, 1500, 1200, 800, &mut rw, &mut rh);
            let again = t.elapsed();
            pe_bytes_free(px, (rw * rh * 4) as usize);
            let t = std::time::Instant::now();
            let px =
                pe_image_render_region(img, s2.as_ptr(), 2100, 1550, 1200, 800, &mut rw, &mut rh);
            let pan = t.elapsed();
            pe_bytes_free(px, (rw * rh * 4) as usize);
            eprintln!("1:1 region: first {first:.0?}, re-render {again:.0?}, small pan {pan:.0?}");
            // The region matches the same part of a full render (CPU).
            let frame = pe_raw::decode_file(&raw).unwrap();
            let settings: pe_pipeline::Settings =
                serde_json::from_str(r#"{"exposure": 0.5, "shadows": 40}"#).unwrap();
            let full = pe_pipeline::render(&frame, &settings).unwrap();
            let mut worst = 0i32;
            let mut sum = 0i64;
            for y in 0..800usize {
                for x in 0..1200usize {
                    for c in 0..3 {
                        let a = region[(y * 1200 + x) * 4 + c] as i32;
                        let b = (full.data[((y + 1500) * full.width + x + 2000) * 3 + c] * 255.0
                            + 0.5) as i32;
                        worst = worst.max((a - b).abs());
                        sum += (a - b).abs() as i64;
                    }
                }
            }
            let mean = sum as f64 / (1200.0 * 800.0 * 3.0);
            eprintln!("vs full render: worst {worst} steps, mean {mean:.4}");
            // Noise is measured per region and auto WB comes from the preview, but neither is used
            // here; only float rounding remains.
            assert!(worst <= 1, "worst {worst}");
            pe_image_close(img);
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
